//! Pending configuration changes (step-6 model): the store behind the global pending bar,
//! the bar itself (Review / Apply / Discard) and the readable diff shared with the
//! Configuration page.
//!
//! Pages that stage a change (`PUT /api/v1/<domain>`, a restore, a drift import) call
//! [`changed`] afterwards; the bar also re-reads `/config/pending` when a `config.*` task
//! ends (events), when the tab becomes visible again and every 30 s while visible (there is
//! no event for staging, and another user's apply can turn a change into a conflict).

use std::cell::Cell;
use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::{ApplyRequest, DiffEntry, DiffOp, PendingChanges, TaskStatus};
use serde_json::Value;

use crate::api;
use crate::auth::auth;
use crate::form::spinner;
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::tasks::{self, task_error, tasks};
use crate::ui::{self, Confirm, DlgKind, Skeleton, Status, Tone, cell, confirm, dialog, row, status, table, text};

/// Re-read of the pending set while the tab is visible.
const POLL: Duration = Duration::from_secs(30);

/// Outcome of the last apply, shown in the bar.
#[derive(Clone, PartialEq)]
pub enum Outcome {
    Applied,
    Failed { task: Option<String>, text: String },
}

#[derive(Clone, Copy)]
pub struct Config {
    /// Compact pending set (keys, conflicts, diff sizes), polled for the bar.
    pub pending: RwSignal<Option<PendingChanges>>,
    /// The same with documents and diffs (`?detail=1`), read while the review dialog is open.
    pub detail: RwSignal<Option<PendingChanges>>,
    /// Task id of the apply in progress.
    pub applying: RwSignal<Option<String>>,
    /// Task id of an apply started by the Save of a dialog or editor ([`apply_now`]) and the
    /// error signal of that form: its progress shows inside the form ([`apply_line`]).
    pub inline: RwSignal<Option<(String, RwSignal<Option<String>>)>>,
    pub outcome: RwSignal<Option<Outcome>>,
    pub review_open: RwSignal<bool>,
    pub discard_open: RwSignal<bool>,
    /// A discard or apply request is on its way.
    pub busy: RwSignal<bool>,
    /// Comment stored with the revision (review dialog).
    pub comment: RwSignal<String>,
    /// Failed reads of the pending set in a row: from two on the bar says its count may be
    /// out of date (it keeps the last known set, never an invented one).
    pub failures: RwSignal<u8>,
}

thread_local! {
    static CONFIG: Cell<Option<Config>> = const { Cell::new(None) };
    /// Bumped after every successful configuration write (stage, apply, discard) and when a
    /// `config.*` task ends: pages re-read what they show ([`crate::ui::Remote::with_config`]).
    static GENERATION: Cell<Option<RwSignal<u64>>> = const { Cell::new(None) };
}

pub fn config() -> Config {
    CONFIG.with(Cell::get).expect("Config::new not called")
}

fn generation_sig() -> RwSignal<u64> {
    GENERATION.with(|c| match c.get() {
        Some(s) => s,
        None => {
            let s = RwSignal::new(0);
            c.set(Some(s));
            s
        }
    })
}

/// Creates the generation signal outside any reactive owner (called from `main` before
/// mounting: a signal created lazily inside an effect would die with it).
pub fn init() {
    generation_sig();
}

/// Generation counter of the configuration: read it in a reactive scope to reload after a
/// change.
pub fn generation() -> u64 {
    generation_sig().get()
}

/// Something staged or applied configuration (call it after every successful write): the
/// pending-changes bar re-reads the pending set and the pages reload what they show.
pub fn changed() {
    generation_sig().update(|n| *n += 1);
    if let Some(c) = CONFIG.with(Cell::get) {
        c.refresh();
    }
}

impl Config {
    pub fn new() -> Self {
        let c = Config {
            pending: RwSignal::new(None),
            detail: RwSignal::new(None),
            applying: RwSignal::new(None),
            inline: RwSignal::new(None),
            outcome: RwSignal::new(None),
            review_open: RwSignal::new(false),
            discard_open: RwSignal::new(false),
            busy: RwSignal::new(false),
            comment: RwSignal::new(String::new()),
            failures: RwSignal::new(0),
        };
        CONFIG.with(|x| x.set(Some(c)));
        set_interval(
            move || {
                if auth().is_ready() && !document().hidden() {
                    c.refresh();
                }
            },
            POLL,
        );
        let _ = window_event_listener(leptos::ev::visibilitychange, move |_| {
            if auth().is_ready() && !document().hidden() {
                c.refresh();
            }
        });
        c
    }

    pub fn refresh(self) {
        spawn_local(async move {
            // a failed read keeps the last known set (the bar never shows invented state)
            match api::get::<PendingChanges>("/api/v1/config/pending").await {
                Ok(p) => {
                    if self.failures.get_untracked() != 0 {
                        self.failures.set(0);
                    }
                    if self.pending.with_untracked(|cur| cur.as_ref() != Some(&p)) {
                        self.pending.set(Some(p));
                    }
                }
                Err(e) if e.status() == 401 => {}
                Err(_) => self.failures.update(|n| *n = n.saturating_add(1)),
            }
        });
    }

    /// Read the full pending set for the review dialog.
    fn load_detail(self) {
        spawn_local(async move {
            if let Ok(p) = api::get::<PendingChanges>("/api/v1/config/pending?detail=1").await {
                self.detail.set(Some(p));
            }
        });
    }

    /// Signed out: forget the user's changeset view.
    pub fn clear(self) {
        self.pending.set(None);
        self.detail.set(None);
        self.outcome.set(None);
        self.review_open.set(false);
        self.discard_open.set(false);
        self.failures.set(0);
    }

    pub fn count(self) -> usize {
        self.pending.with(|p| p.as_ref().map_or(0, |p| p.changes.len()))
    }

    pub fn conflicts(self) -> bool {
        self.pending.with(|p| p.as_ref().is_some_and(|p| p.changes.iter().any(|c| c.conflict)))
    }

    /// Run `config.apply` with the review comment; the bar follows the task.
    pub fn apply(self) {
        if self.busy.get_untracked() || self.applying.get_untracked().is_some() {
            return;
        }
        let body = serde_json::to_string(&ApplyRequest { comment: self.comment.get_untracked().trim().to_string(), keys: Vec::new() }).ok();
        self.busy.set(true);
        self.outcome.set(None);
        self.review_open.set(false);
        spawn_local(async move {
            let started = api::start_task("/api/v1/config/apply", body).await;
            self.busy.set(false);
            let id = match started {
                Ok(id) => id,
                Err(e) => {
                    self.outcome.set(Some(Outcome::Failed { task: None, text: e.text() }));
                    self.refresh();
                    return;
                }
            };
            self.applying.set(Some(id.clone()));
            let res = tasks::follow(&id).await;
            self.applying.set(None);
            match res {
                Ok(t) if t.status == TaskStatus::Ok => {
                    self.comment.set(String::new());
                    self.outcome.set(Some(Outcome::Applied));
                    set_timeout(
                        move || {
                            if self.outcome.get_untracked() == Some(Outcome::Applied) {
                                self.outcome.set(None);
                            }
                        },
                        Duration::from_secs(5),
                    );
                }
                Ok(t) => self.outcome.set(Some(Outcome::Failed { task: Some(id), text: task_error(&t) })),
                Err(e) if e.status() == 401 => {}
                Err(e) => self.outcome.set(Some(Outcome::Failed { task: Some(id), text: e.text() })),
            }
            self.refresh();
        });
    }

    pub fn discard(self) {
        self.busy.set(true);
        self.review_open.set(false);
        spawn_local(async move {
            let r = api::post_empty::<()>("/api/v1/config/discard", None).await;
            self.busy.set(false);
            match r {
                Ok(()) => self.outcome.set(None),
                Err(e) if e.status() == 401 => {}
                Err(e) => self.outcome.set(Some(Outcome::Failed { task: None, text: e.text() })),
            }
            self.refresh();
        });
    }
}

/// Configuration keys the forms stage (the server's domain keys), for [`apply_now`].
pub mod keys {
    pub const USERS: &str = "users";
    pub const SHARES: &str = "shares";
    pub const SNAPSHOTS: &str = "snapshots";
    pub const SCHEDULER: &str = "scheduler";
    pub const POWER: &str = "power.schedule";
    pub const UPS: &str = "ups.safemode";
    pub const UPDATES: &str = "updates.auto";
    pub const DISKS: &str = "storage.disks";
}

/// The Save of a dialog or a single-object editor (guidelines A19b). Call it once the form's
/// `PUT`/`POST` has staged the change: it applies only `keys` (every configuration key that
/// endpoint can stage; keys with nothing staged are ignored, other staged keys stay in the
/// pending bar) and waits for the `config.apply` task, whose progress [`apply_line`] shows in
/// the form's [`crate::form::form_error`] (the one of `error`). `true` when it was applied;
/// otherwise `error` holds the task's failure and the note that the previous settings are
/// back in place while the change stays pending, and the caller keeps the dialog open.
pub async fn apply_now(keys: &[&str], error: RwSignal<Option<String>>) -> bool {
    let c = config();
    let req = ApplyRequest { comment: String::new(), keys: keys.iter().map(|k| k.to_string()).collect() };
    let started = api::start_task("/api/v1/config/apply", serde_json::to_string(&req).ok()).await;
    let id = match started {
        Ok(id) => id,
        Err(e) => {
            changed();
            let _ = error.try_set(Some(e.text()));
            return false;
        }
    };
    c.applying.set(Some(id.clone()));
    c.inline.set(Some((id.clone(), error)));
    let res = tasks::follow(&id).await;
    c.applying.set(None);
    c.inline.set(None);
    changed();
    let fail = match res {
        Ok(task) if task.status == TaskStatus::Ok => return true,
        Ok(task) => format!("{} {}", task_error(&task), t("cfg.apply_kept")),
        Err(e) => e.text(),
    };
    let _ = error.try_set(Some(fail));
    false
}

/// Progress of the [`apply_now`] in flight, shown by the form error box of the form that
/// started it (`error`): "Applying the change… 40%" and a link to the task's log.
pub fn apply_line(error: RwSignal<Option<String>>) -> impl IntoView {
    let c = config();
    move || {
        c.inline.get().filter(|(_, e)| *e == error).map(|(id, _)| {
            let pid = id.clone();
            let progress = move || tasks().list.with(|l| l.iter().find(|x| x.id == pid).and_then(|x| x.progress)).map(|p| i18n::pct(p as f64));
            view! {
                <p class="apply-line small muted" role="status" aria-live="polite">
                    <span class="btn-spinner" aria-hidden="true"></span>
                    <span>{tr("cfg.applying_now")}</span>
                    <span class="tnum">{progress}</span>
                    <button class="link-btn" type="button" on:click=move |_| tasks().show(&id)>{tr("cfg.apply_log")}</button>
                </p>
            }
        })
    }
}

/// Readable name of a configuration key (`cfg.key.<key>`), the key itself when unknown.
pub fn key_label(key: &str) -> String {
    i18n::try_t(&format!("cfg.key.{key}")).map(str::to_string).unwrap_or_else(|| key.to_string())
}

/// A JSON value as one short line: strings bare, `—` for none, documents compacted.
pub fn value_text(v: Option<&Value>) -> String {
    let s = match v {
        None | Some(Value::Null) => return "—".into(),
        Some(Value::String(s)) if s.is_empty() => return format!("({})", t("diff.empty")),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    };
    if s.chars().count() > 160 { format!("{}…", s.chars().take(160).collect::<String>()) } else { s }
}

/// Path of a diff entry inside its key: `system.hostname.hostname` → `hostname`.
fn rel_path<'a>(path: &'a str, key: Option<&str>) -> &'a str {
    let rest = match key {
        Some(k) => path.strip_prefix(k).unwrap_or(path),
        None => path,
    };
    rest.trim_start_matches('.')
}

/// Semantic diff as a table: field, kind of change, old → new. `key` strips that key from the
/// paths (the caller shows it as a heading).
pub fn diff_table(entries: &[DiffEntry], key: Option<&str>) -> AnyView {
    if entries.is_empty() {
        return view! { <p class="muted small">{tr("diff.none")}</p> }.into_any();
    }
    let rows = entries
        .iter()
        .map(|d| {
            let (tone, op) = match d.op {
                DiffOp::Added => (Tone::Ok, "diff.added"),
                DiffOp::Removed => (Tone::Err, "diff.removed"),
                DiffOp::Changed => (Tone::Info, "diff.changed"),
            };
            let p = rel_path(&d.path, key);
            let field = if p.is_empty() { t("diff.whole").to_string() } else { p.to_string() };
            let old = (d.op != DiffOp::Added).then(|| value_text(d.old.as_ref()));
            let new = (d.op != DiffOp::Removed).then(|| value_text(d.new.as_ref()));
            let value = view! {
                {old.map(|o| { let tip = o.clone(); view! { <del class="diff-old mono" title=tip>{o}</del> } })}
                {(d.op == DiffOp::Changed).then(|| view! { <span class="diff-arrow" aria-label=tr("diff.to")>"→"</span> })}
                {new.map(|n| { let tip = n.clone(); view! { <ins class="diff-new mono" title=tip>{n}</ins> } })}
            }
            .into_any();
            row(vec![
                cell("diff.field", "mono", text(field)),
                cell("diff.kind", "", status(tone, t(op).to_string())),
                cell("diff.value", "diff-val", value),
            ])
        })
        .collect();
    table("diff", &["diff.field", "diff.kind", "diff.value"], rows)
}

/// "N pending changes" bar above the task panel (Proxmox/OMV style): Review, Apply, Discard;
/// follows the apply task and shows its outcome. Hidden when there is nothing to say.
#[component]
pub fn PendingBar() -> impl IntoView {
    let c = config();
    let n = Memo::new(move |_| c.count());
    let visible = move || n.get() > 0 || c.applying.with(Option::is_some) || c.outcome.with(Option::is_some);
    let keys = move || {
        c.pending.with(|p| {
            let mut ks: Vec<String> = p.as_ref().map(|p| p.changes.iter().map(|ch| key_label(&ch.key)).collect()).unwrap_or_default();
            ks.dedup();
            ks.join(", ")
        })
    };
    let progress = move || {
        c.applying.get().and_then(|id| tasks().list.with(|l| l.iter().find(|t| t.id == id).and_then(|t| t.progress)))
    };
    let line = move || {
        if c.applying.with(Option::is_some) {
            let p = progress().map(|p| format!(" {}", i18n::pct(p as f64))).unwrap_or_default();
            return view! {
                <span class="btn-spinner" aria-hidden="true"></span>
                <span class="pending-text"><strong>{t("pending.applying")}</strong><span class="tnum">{p}</span></span>
            }
            .into_any();
        }
        match c.outcome.get() {
            Some(Outcome::Applied) => view! {
                <span class="tone-text ok"><Icon icon=icons::CIRCLE_CHECK size=16 /></span>
                <span class="pending-text"><strong>{t("pending.applied")}</strong></span>
            }
            .into_any(),
            Some(Outcome::Failed { task, text }) => view! {
                <span class="tone-text err"><Icon icon=icons::CIRCLE_X size=16 /></span>
                <span class="pending-text"><strong>{t("pending.failed")}</strong>" "{crate::ui::err_view(text)}</span>
                {task.map(|id| view! {
                    <button class="btn ghost sm" on:click=move |_| tasks().show(&id)>
                        <Icon icon=icons::SCROLL_TEXT size=14 />{tr("pending.show_log")}
                    </button>
                })}
                <button class="icon-btn sm" aria-label=tr("close") on:click=move |_| c.outcome.set(None)>
                    <Icon icon=icons::X size=14 />
                </button>
            }
            .into_any(),
            None => view! {
                <Icon icon=icons::FILE_DIFF size=16 />
                <span class="pending-text">
                    <strong class="tnum">{move || n.get()}</strong>" "
                    {move || if n.get() == 1 { t("pending.one") } else { t("pending.changes") }}
                    <span class="pending-keys">{keys}</span>
                </span>
                {move || c.conflicts().then(|| view! { <Status tone=Tone::Warn text=t("pending.conflict").to_string() /> })}
                {move || (c.failures.get() >= 2).then(|| view! {
                    <span class="tone-text warn" title=tr("pending.stale") role="img" aria-label=tr("pending.stale")>
                        <Icon icon=icons::TRIANGLE_ALERT size=16 />
                    </span>
                })}
            }
            .into_any(),
        }
    };
    let can_act = move || n.get() > 0 && c.applying.with(Option::is_none);
    view! {
        <Show when=visible>
            <div class=move || match c.outcome.get() {
                Some(Outcome::Failed { .. }) => "pending failed",
                Some(Outcome::Applied) => "pending done",
                None => "pending",
            } role="status">
                {line}
                <Show when=can_act>
                    <div class="pending-actions">
                        <button class="btn ghost sm" disabled=move || c.busy.get() on:click=move |_| c.discard_open.set(true)>
                            {tr("pending.discard")}
                        </button>
                        <button class="btn ghost sm" on:click=move |_| c.review_open.set(true)>
                            {tr("pending.review")}
                        </button>
                        <button class="btn primary sm" disabled=move || c.busy.get() || c.conflicts() on:click=move |_| c.apply()
                            title=move || if c.conflicts() { t("pending.conflict_text") } else { "" }>
                            {spinner(c.busy)}{tr("pending.apply")}
                        </button>
                    </div>
                </Show>
            </div>
        </Show>
        {move || c.review_open.get().then(review_dialog)}
        {move || c.discard_open.get().then(|| confirm(Confirm::new(
            t("pending.discard_title").to_string(),
            tf("pending.discard_text", &[("n", &n.get_untracked().to_string())]),
            "pending.discard",
            true,
            Callback::new(move |_| {
                c.discard_open.set(false);
                c.discard();
            }),
            Callback::new(move |_| c.discard_open.set(false)),
        )))}
    }
}

/// Every staged change with its diff, the revision comment and the actions.
fn review_dialog() -> AnyView {
    let c = config();
    // the diffs are read when the dialog opens and again whenever the pending set changes
    Effect::new(move || {
        c.pending.track();
        c.load_detail();
    });
    let body = view! {
        {move || {
            let Some(p) = c.detail.get() else {
                return view! { <Skeleton lines=3 /> }.into_any();
            };
            if p.changes.is_empty() {
                return view! { <p class="muted">{t("err.nothing_pending")}</p> }.into_any();
            }
            p.changes
                .iter()
                .map(|ch| {
                    view! {
                        <section class="review-key">
                            <div class="review-head">
                                <h3>{key_label(&ch.key)}</h3>
                                <span class="mono small muted">{ch.key.clone()}</span>
                                {ch.conflict.then(|| view! { <Status tone=Tone::Warn text=t("pending.conflict").to_string() /> })}
                                <span class="small muted review-time">{format!("{} {}", t("pending.staged"), i18n::datetime(ch.staged_ms))}</span>
                            </div>
                            {diff_table(&ch.diff, Some(&ch.key))}
                        </section>
                    }
                })
                .collect_view()
                .into_any()
        }}
        <Show when=move || c.conflicts()>
            {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tr("pending.conflict_text")} }.into_any())}
        </Show>
    }
    .into_any();
    let foot = view! {
        <form class="review-foot" on:submit=move |e| { e.prevent_default(); c.apply(); }>
            <label class="field grow">
                <span class="field-label">{tr("pending.comment")}</span>
                <input class="input" type="text" maxlength="200" placeholder=tr("pending.comment_hint")
                    prop:value=move || c.comment.get()
                    on:input=move |e| c.comment.set(event_target_value(&e)) />
            </label>
            <div class="btn-row">
                <button type="button" class="btn ghost" on:click=move |_| c.discard_open.set(true)>{tr("pending.discard")}</button>
                <button type="submit" class="btn primary" disabled=move || c.busy.get() || c.conflicts() || c.count() == 0>
                    {tr("pending.apply")}
                </button>
            </div>
        </form>
    }
    .into_any();
    dialog(
        DlgKind::Wide,
        Signal::derive(|| t("pending.review_title").to_string()),
        Callback::new(move |_| c.review_open.set(false)),
        body,
        Some(foot),
    )
}
