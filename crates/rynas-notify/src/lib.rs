//! rynas notifications (D-NOTIFY).
//!
//! Two layers:
//! - always: the [`Notifier`] interface every crate emits through ([`Notifier::emit`] is
//!   fire-and-forget and never blocks), event builders ([`event`]) and the stateful health
//!   [`checks`] (hardware, storage, tasks) that turn readings into firing/resolved events;
//! - feature `engine` (on by default, used by rynasd, the single owner): the engine itself
//!   ([`engine::Engine`]): events, targets and rules in SQLite (`<state>/notify/notify.db`),
//!   state per key, dedup, grouping, quiet hours, reminders, per-target rate limit, a
//!   persistent outbox with retries, minijinja templates in English and Italian, SMTP
//!   (lettre), ntfy, Telegram, Gotify and webhook channels, and the one-time import of
//!   another tool's settings ([`ImportedSettings`]).
//!
//! Other processes (rynas-web, plugins) do not link the engine: they send [`NewEvent`]s to
//! rynasd (`POST /v1/notifications/events` on the internal socket, or JSON-RPC for plugins)
//! through their own [`Notifier`] implementation.

pub mod checks;
#[cfg(feature = "engine")]
pub mod engine;

pub use rynas_api_types::notify::{
    BodyFormat, EventAction, EventState, NewEvent, NotifyCategory, NotifySeverity,
};

/// Notification settings of another tool (a hardware module's system layer,
/// `rynas_module::Module::notify_import`), imported once into targets and one rule.
#[derive(Debug, Clone, Default)]
pub struct ImportedSettings {
    /// Engine meta key recording that the import ran (never twice).
    pub key: String,
    /// Name of the rule created for the imported targets.
    pub rule_name: String,
    pub targets: Vec<rynas_api_types::notify::TargetRequest>,
    pub min_severity: NotifySeverity,
}

/// Where events go. Implementations queue and return at once: emitting never blocks the
/// caller and never fails (a full queue drops the event with a log line).
pub trait Notifier: Send + Sync {
    fn emit(&self, event: NewEvent);
}

/// Drops every event (tests, tools without a daemon).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullNotifier;

impl Notifier for NullNotifier {
    fn emit(&self, _event: NewEvent) {}
}

/// Collects events in memory (tests).
#[derive(Debug, Default)]
pub struct MemoryNotifier(pub std::sync::Mutex<Vec<NewEvent>>);

impl MemoryNotifier {
    pub fn take(&self) -> Vec<NewEvent> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

impl Notifier for MemoryNotifier {
    fn emit(&self, event: NewEvent) {
        self.0.lock().unwrap().push(event);
    }
}

/// Event builders: `event::firing(Category::Storage, Severity::Critical, "storage", key, title)`
/// then set `body`, `fields`, `actions`.
pub mod event {
    use super::*;

    fn base(
        state: EventState,
        category: NotifyCategory,
        severity: NotifySeverity,
        source: &str,
        key: Option<String>,
        title: impl Into<String>,
    ) -> NewEvent {
        NewEvent {
            source: source.into(),
            category,
            severity,
            key,
            state,
            title: title.into(),
            ..Default::default()
        }
    }

    /// A condition that opens (or stays open).
    pub fn firing(
        category: NotifyCategory,
        severity: NotifySeverity,
        source: &str,
        key: impl Into<String>,
        title: impl Into<String>,
    ) -> NewEvent {
        base(
            EventState::Firing,
            category,
            severity,
            source,
            Some(key.into()),
            title,
        )
    }

    /// The condition `key` is over. Ignored by the engine when it was not firing.
    pub fn resolved(
        category: NotifyCategory,
        source: &str,
        key: impl Into<String>,
        title: impl Into<String>,
    ) -> NewEvent {
        base(
            EventState::Resolved,
            category,
            NotifySeverity::Info,
            source,
            Some(key.into()),
            title,
        )
    }

    /// A single fact; repeats of the same key within a few minutes are counted, not sent.
    pub fn oneshot(
        category: NotifyCategory,
        severity: NotifySeverity,
        source: &str,
        key: impl Into<String>,
        title: impl Into<String>,
    ) -> NewEvent {
        base(
            EventState::Oneshot,
            category,
            severity,
            source,
            Some(key.into()),
            title,
        )
    }

    /// Adds a field.
    pub fn field(mut ev: NewEvent, k: &str, v: impl ToString) -> NewEvent {
        ev.fields.insert(k.into(), v.to_string());
        ev
    }

    /// Sets the stable event code: clients translate the title and body from it and the
    /// fields (code table in `docs/phase1/plan.md`, "Event codes").
    pub fn code(mut ev: NewEvent, code: &str) -> NewEvent {
        ev.code = code.into();
        ev
    }

    /// A resolution kept in the list but sent to no target ([`NewEvent::quiet`]).
    pub fn quietly(mut ev: NewEvent) -> NewEvent {
        ev.quiet = true;
        ev
    }
}
