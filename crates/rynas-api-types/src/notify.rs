//! Notifications (`/api/v1/notifications`): events, targets (channels) and rules (matchers).
//!
//! Model (D-NOTIFY, Proxmox targets + matchers, TrueNAS stateful alerts):
//! - an **event** is emitted by a source with a `key`; `firing`/`resolved` events open and
//!   close a condition (notified only on transitions, with reminders while it stays open),
//!   `oneshot` events are single facts (deduplicated by key for a few minutes);
//! - a **target** is where notifications go (SMTP, ntfy, Telegram, Gotify, webhook, the web
//!   interface); its secrets are write-only;
//! - a **rule** selects events (severity, categories, source, fields) and sends them to
//!   targets, with quiet hours (never holding `critical`), a grouping window and a reminder
//!   interval.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// What an event is about.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum NotifyCategory {
    Storage,
    Hardware,
    Ups,
    #[default]
    System,
    Security,
    Update,
    Backup,
    App,
    Task,
}

impl NotifyCategory {
    pub const ALL: [NotifyCategory; 9] = [
        Self::Storage,
        Self::Hardware,
        Self::Ups,
        Self::System,
        Self::Security,
        Self::Update,
        Self::Backup,
        Self::App,
        Self::Task,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Storage => "storage",
            Self::Hardware => "hardware",
            Self::Ups => "ups",
            Self::System => "system",
            Self::Security => "security",
            Self::Update => "update",
            Self::Backup => "backup",
            Self::App => "app",
            Self::Task => "task",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == s)
    }
}

/// Ordered: `info < notice < warning < error < critical`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum NotifySeverity {
    #[default]
    Info,
    Notice,
    Warning,
    Error,
    Critical,
}

impl NotifySeverity {
    pub const ALL: [NotifySeverity; 5] = [
        Self::Info,
        Self::Notice,
        Self::Warning,
        Self::Error,
        Self::Critical,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Notice => "notice",
            Self::Warning => "warning",
            Self::Error => "error",
            Self::Critical => "critical",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == s)
    }
}

/// `firing`/`resolved` open and close the condition named by the key; `oneshot` is a fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EventState {
    Firing,
    Resolved,
    #[default]
    Oneshot,
}

impl EventState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Firing => "firing",
            Self::Resolved => "resolved",
            Self::Oneshot => "oneshot",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [Self::Firing, Self::Resolved, Self::Oneshot]
            .into_iter()
            .find(|c| c.as_str() == s)
    }
}

/// A link shown with a notification (UI page, documentation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct EventAction {
    pub label: String,
    pub url: String,
}

/// What a source emits. `source` is set by the emitter inside rynas (plugins get
/// `plugin:<id>` from the core, synodeb events `synodeb:<source>`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct NewEvent {
    /// e.g. "storage", "auth", "task", "hardware", "synodeb:smartd".
    pub source: String,
    pub category: NotifyCategory,
    pub severity: NotifySeverity,
    /// State/dedup key, e.g. "storage:md:md2:degraded". Required for firing/resolved; for
    /// oneshot events it deduplicates repeats within a few minutes (default: source + title).
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub state: EventState,
    /// One line (at most 200 characters are kept).
    pub title: String,
    /// Plain text (at most 4000 characters are kept).
    #[serde(default)]
    pub body: String,
    /// Machine-readable details (disk, array, user...), usable by rules and templates.
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
    #[serde(default)]
    pub actions: Vec<EventAction>,
}

/// `GET /api/v1/notifications`: one stored event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Notification {
    pub id: u64,
    pub ts_ms: u64,
    /// Last repetition of the same event (deduplicated while firing or within the oneshot
    /// window); equals `ts_ms` when it happened once.
    pub last_ms: u64,
    /// How many times it was emitted (1 = once).
    pub count: u32,
    pub source: String,
    pub category: NotifyCategory,
    pub severity: NotifySeverity,
    pub key: String,
    pub state: EventState,
    pub title: String,
    pub body: String,
    pub fields: BTreeMap<String, String>,
    pub actions: Vec<EventAction>,
    /// The key was silenced when it arrived: stored, not sent.
    pub silenced: bool,
    pub acked_ms: Option<u64>,
    pub acked_by: Option<String>,
    /// For a firing event: when its condition was resolved.
    pub resolved_ms: Option<u64>,
}

/// Change pushed on the `notification` event of `GET /api/v1/events`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NotificationChangeKind {
    /// A new notification.
    #[default]
    Created,
    /// An existing one changed (repeated, escalated, resolved, acknowledged).
    Updated,
    /// Every notification up to `notification.id` was acknowledged (`notification` absent
    /// when there was nothing to acknowledge).
    AckedAll,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct NotificationEvent {
    pub kind: NotificationChangeKind,
    pub notification: Option<Notification>,
    /// Unacknowledged notifications after the change (the bell counter).
    pub unacked: u64,
}

/// `POST /api/v1/notifications/silence`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SilenceRequest {
    pub key: String,
    /// Seconds; `null` = until removed.
    pub duration_s: Option<u64>,
}

/// `GET /api/v1/notifications/silences`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Silence {
    pub id: u64,
    pub key: String,
    pub created_ms: u64,
    pub created_by: String,
    /// `null` = until removed.
    pub until_ms: Option<u64>,
}

/// `POST /api/v1/notifications/ack-all`: answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AckAllResult {
    pub acked: u64,
}

/// Language of a target's messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NotifyLanguage {
    #[default]
    En,
    It,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SmtpSecurity {
    /// Plain connection upgraded with STARTTLS (port 587).
    #[default]
    Starttls,
    /// TLS from the start (port 465).
    Tls,
    /// No encryption (local relays only).
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WebhookMethod {
    Get,
    #[default]
    Post,
    Put,
}

/// One HTTP header of a webhook; `value` is a template (`{{ secrets.token }}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WebhookHeader {
    pub name: String,
    pub value: String,
}

/// Where notifications go. Secrets (passwords, tokens) are not part of it: they are set
/// with [`TargetRequest::secrets`] and never returned. Secret names per kind: smtp
/// `password`; ntfy `token` (or `password` with `username`); telegram `bot_token`; gotify
/// `token`; webhook: any name, used as `{{ secrets.<name> }}` in the URL, headers and body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetConfig {
    Smtp {
        host: String,
        port: u16,
        #[serde(default)]
        security: SmtpSecurity,
        #[serde(default)]
        username: Option<String>,
        from: String,
        to: Vec<String>,
    },
    Ntfy {
        /// e.g. "https://ntfy.sh".
        server: String,
        topic: String,
        #[serde(default)]
        username: Option<String>,
    },
    Telegram {
        chat_id: String,
        /// Bot API base URL; default "https://api.telegram.org".
        #[serde(default)]
        api_url: Option<String>,
    },
    Gotify {
        /// e.g. "https://gotify.example.org".
        server: String,
    },
    Webhook {
        /// Template, e.g. "https://hooks.example.org/{{ secrets.path }}".
        url: String,
        #[serde(default)]
        method: WebhookMethod,
        #[serde(default)]
        headers: Vec<WebhookHeader>,
        /// Body template (minijinja); `null` sends the default JSON document.
        #[serde(default)]
        body: Option<String>,
    },
    /// The notification center of the web interface: every event is listed there anyway;
    /// as a rule target it means "no external delivery".
    Ui {},
}

impl TargetConfig {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Smtp { .. } => "smtp",
            Self::Ntfy { .. } => "ntfy",
            Self::Telegram { .. } => "telegram",
            Self::Gotify { .. } => "gotify",
            Self::Webhook { .. } => "webhook",
            Self::Ui {} => "ui",
        }
    }
}

/// Delivery health of a target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TargetStatus {
    pub last_ok_ms: Option<u64>,
    pub last_error_ms: Option<u64>,
    pub last_error: Option<String>,
    /// Messages waiting in the outbox (grouping, quiet hours, retries).
    pub queued: u64,
}

/// `GET /api/v1/notifications/targets`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotifyTarget {
    pub id: u64,
    pub name: String,
    pub enabled: bool,
    pub language: NotifyLanguage,
    pub config: TargetConfig,
    /// Names of the secrets that are set (values are write-only).
    pub secrets: Vec<String>,
    pub status: TargetStatus,
}

/// `POST/PUT /api/v1/notifications/targets[/{id}]`. `secrets`: a string sets the secret,
/// `null` removes it, a missing name keeps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetRequest {
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub language: NotifyLanguage,
    pub config: TargetConfig,
    #[serde(default)]
    pub secrets: BTreeMap<String, Option<String>>,
}

fn yes() -> bool {
    true
}

/// `POST /api/v1/notifications/targets/{id}/test`: the outcome of a test message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TargetTestResult {
    pub ok: bool,
    /// Why it failed (server answer, connection error); never contains secrets.
    pub error: Option<String>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    /// Every condition that is set must match.
    #[default]
    All,
    /// At least one condition that is set must match.
    Any,
}

/// A condition on an event field (`fields[field]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FieldMatch {
    pub field: String,
    /// Exact value, or a regular expression when `regex` is true.
    pub value: String,
    #[serde(default)]
    pub regex: bool,
}

/// A weekly quiet window, local time. `end` before `start` crosses midnight
/// ("23:00"–"07:00"). `days` are the days the window *starts* on, 1 = Monday … 7 = Sunday;
/// empty = every day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct QuietWindow {
    #[serde(default)]
    pub days: Vec<u8>,
    /// "HH:MM".
    pub start: String,
    /// "HH:MM".
    pub end: String,
}

/// `GET /api/v1/notifications/rules`: a matcher. Conditions that are not set (lowest
/// severity, no categories, no source, no fields) do not take part in `mode`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct NotifyRule {
    #[serde(default)]
    pub id: u64,
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Events at least this severe.
    #[serde(default)]
    pub min_severity: NotifySeverity,
    /// Empty = any category.
    #[serde(default)]
    pub categories: Vec<NotifyCategory>,
    /// Regular expression on the event source.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub fields: Vec<FieldMatch>,
    #[serde(default)]
    pub mode: MatchMode,
    /// Negates the result of the conditions.
    #[serde(default)]
    pub invert: bool,
    /// Non-critical events matching during a quiet window are held and sent when it ends.
    #[serde(default)]
    pub quiet_hours: Vec<QuietWindow>,
    pub targets: Vec<u64>,
    /// Events for the same target within this window go out as one message (0 = at once).
    #[serde(default)]
    pub group_window_s: u64,
    /// Reminder interval while a condition stays firing (0 = no reminders).
    #[serde(default)]
    pub repeat_interval_s: u64,
}

/// Answer of synodeb's `notify` forward on `/run/rynas/notify.sock` (`POST /v1/notify`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct NotifyAccepted {
    /// The stored notification; `null` when it was a repeat within the dedup window.
    pub id: Option<u64>,
}
