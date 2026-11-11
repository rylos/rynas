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

/// How a notification body is meant to be shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BodyFormat {
    /// Prose: may be reflowed.
    #[default]
    Text,
    /// Command output or a log excerpt: monospace, line breaks kept.
    Log,
}

impl BodyFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Log => "log",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [Self::Text, Self::Log]
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
    /// Stable event code (`hw.fan_stopped`, `synodeb.unit_failed`; empty = none). Clients
    /// translate the title and body from the code and `fields` (code table in
    /// `docs/phase1/plan.md`, "Event codes"); `title` and `body` stay the English fallback
    /// and are what rules see.
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub body_format: BodyFormat,
    /// A resolution stored without a message to any target: the condition ended by the
    /// administrator's own choice (automatic time sync turned off), nothing to tell.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub quiet: bool,
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
    /// Stable event code (see [`NewEvent::code`]); empty = none.
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub body_format: BodyFormat,
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

// ------------------------------------------------------------------------- event texts

/// Variants of a text, most specific first: `nev.<code>.title`, `….title_2`, `….title_3`.
/// The first one whose placeholders all have a non-empty field wins, so an optional field
/// gets a shorter variant without it.
const VARIANTS: [&str; 3] = ["", "_2", "_3"];

/// `[a-z0-9_]`, not empty: a value that can be part of a key.
fn key_word(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// One placeholder value as shown: `nev.<code>.<field>.<value in lowercase>` when the
/// dictionary has it (closed sets: SMART fail types, mdadm events, UPS reasons…), the task
/// name `task.<value>` for a field `type`, a decimal number with a comma when `comma`,
/// otherwise the value itself.
fn shown_value(
    code: &str,
    name: &str,
    v: &str,
    lookup: &dyn Fn(&str) -> Option<&'static str>,
    comma: bool,
) -> String {
    let lower = v.to_ascii_lowercase();
    if key_word(&lower)
        && let Some(s) = lookup(&format!("nev.{code}.{name}.{lower}"))
    {
        return s.to_string();
    }
    if name == "type"
        && let Some(s) = lookup(&format!("task.{v}"))
    {
        return s.to_string();
    }
    let decimal = v.split_once('.').is_some_and(|(i, d)| {
        let i = i.strip_prefix('-').unwrap_or(i);
        !i.is_empty() && !d.is_empty() && i.bytes().chain(d.bytes()).all(|b| b.is_ascii_digit())
    });
    if comma && decimal {
        v.replace('.', ",")
    } else {
        v.to_string()
    }
}

/// `tpl` with every `{field}` replaced; `None` when a field is missing or empty.
fn fill(
    tpl: &str,
    code: &str,
    fields: &BTreeMap<String, String>,
    lookup: &dyn Fn(&str) -> Option<&'static str>,
    comma: bool,
) -> Option<String> {
    let mut out = String::with_capacity(tpl.len() + 32);
    let mut rest = tpl;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let close = after.find('}')?;
        let name = &after[..close];
        let v = fields.get(name).filter(|v| !v.trim().is_empty())?;
        out.push_str(&shown_value(code, name, v, lookup, comma));
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// The translated `part` (`"title"` or `"body"`) of an event: the first variant of
/// `nev.<code>.<part>` that `lookup` knows and whose placeholders all have a non-empty field.
/// `None` when there is no code, no key or a field is missing: the caller shows the English
/// `title`/`body` of the event. Shared by the web interface and the channel templates, so
/// both read the dictionary the same way.
pub fn event_text(
    code: &str,
    part: &str,
    fields: &BTreeMap<String, String>,
    lookup: &dyn Fn(&str) -> Option<&'static str>,
    comma: bool,
) -> Option<String> {
    if code.is_empty() {
        return None;
    }
    VARIANTS.iter().find_map(|v| {
        lookup(&format!("nev.{code}.{part}{v}"))
            .and_then(|tpl| fill(tpl, code, fields, lookup, comma))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(k: &str) -> Option<&'static str> {
        Some(match k {
            "nev.x.y.title" => "RAID {array}: {event} ({device})",
            "nev.x.y.title_2" => "RAID {array}: {event}",
            "nev.x.y.event.fail" => "device failed",
            "nev.x.y.body" => "{pct}% of {total}",
            "nev.t.f.title" => "Failed: {type}",
            "task.smart.test" => "SMART test",
            _ => return None,
        })
    }

    fn fields(kv: &[(&str, &str)]) -> BTreeMap<String, String> {
        kv.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn variants_values_and_fallback() {
        let f = fields(&[("array", "md2"), ("event", "Fail"), ("device", "sda3")]);
        assert_eq!(
            event_text("x.y", "title", &f, &dict, false).as_deref(),
            Some("RAID md2: device failed (sda3)")
        );
        // empty device: the shorter variant; unknown value: as sent
        let f = fields(&[("array", "md2"), ("event", "Rebuild20"), ("device", "")]);
        assert_eq!(
            event_text("x.y", "title", &f, &dict, false).as_deref(),
            Some("RAID md2: Rebuild20")
        );
        // a field every variant needs is missing: the caller falls back
        assert_eq!(
            event_text("x.y", "title", &fields(&[("event", "Fail")]), &dict, false),
            None
        );
        // no code, unknown code, no body key
        assert_eq!(event_text("", "title", &f, &dict, false), None);
        assert_eq!(event_text("x.z", "title", &f, &dict, false), None);
        assert_eq!(event_text("t.f", "body", &f, &dict, false), None);
        // decimals with the language's separator, only for numbers
        let f = fields(&[("pct", "85.3"), ("total", "192.0.2.1")]);
        assert_eq!(
            event_text("x.y", "body", &f, &dict, true).as_deref(),
            Some("85,3% of 192.0.2.1")
        );
        assert_eq!(
            event_text("x.y", "body", &f, &dict, false).as_deref(),
            Some("85.3% of 192.0.2.1")
        );
        // task types through their names
        assert_eq!(
            event_text(
                "t.f",
                "title",
                &fields(&[("type", "smart.test")]),
                &dict,
                false
            )
            .as_deref(),
            Some("Failed: SMART test")
        );
        assert_eq!(
            event_text("t.f", "title", &fields(&[("type", "x.y")]), &dict, false).as_deref(),
            Some("Failed: x.y")
        );
    }
}
