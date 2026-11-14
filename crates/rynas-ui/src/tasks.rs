//! Background tasks: the store behind the task panel, the [`run_task`] helper every page uses
//! for long work, and the Proxmox-style panel itself (list + live log + cancel).
//!
//! The list comes from `GET /api/v1/tasks` and is kept current by the `task.*` events of the
//! multiplexed stream (`/api/v1/events`, see [`crate::live`]); a `resync` re-reads it.

use std::cell::Cell;
use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::{TaskEvent, TaskInfo, TaskStatus};

use crate::api::{self, ApiErr};
use crate::auth::auth;
use crate::i18n::{self, t, tr};
use crate::icons::{self, Icon};
use crate::toast::toast;
use crate::ui::{ErrorBox, Meter, Status, Tone, cell, row_flag, status, table, text};

/// Tasks kept in the panel (newest first): the server keeps 500, the panel shows the latest.
pub const CAP: usize = 50;

#[derive(Clone, Copy)]
pub struct Tasks {
    /// Newest first, at most [`CAP`].
    pub list: RwSignal<Vec<TaskInfo>>,
    pub loaded: RwSignal<bool>,
    pub err: RwSignal<Option<String>>,
    /// Task whose log the panel shows.
    pub selected: RwSignal<Option<String>>,
    /// Panel open (the shell's `Ui::tasks_open`).
    pub open: RwSignal<bool>,
    /// Wall clock in ms, ticking every second while a task runs (durations).
    pub now_ms: RwSignal<f64>,
    /// The event stream is connected: no need to poll a followed task.
    pub stream_live: Signal<bool>,
}

thread_local! {
    static TASKS: Cell<Option<Tasks>> = const { Cell::new(None) };
}

/// The app's task store (also reachable from async code and stream callbacks).
pub fn tasks() -> Tasks {
    TASKS.with(Cell::get).expect("Tasks::new not called")
}

impl Tasks {
    pub fn new(open: RwSignal<bool>, stream_live: Signal<bool>) -> Self {
        let s = Tasks {
            list: RwSignal::new(Vec::new()),
            loaded: RwSignal::new(false),
            err: RwSignal::new(None),
            selected: RwSignal::new(None),
            open,
            now_ms: RwSignal::new(js_sys::Date::now()),
            stream_live,
        };
        TASKS.with(|c| c.set(Some(s)));
        set_interval(
            move || {
                if s.list.with_untracked(|l| l.iter().any(|t| !t.status.is_finished())) {
                    s.now_ms.set(js_sys::Date::now());
                }
            },
            Duration::from_secs(1),
        );
        s
    }

    /// Read the list again (sign-in, `resync`, Retry).
    pub fn reload(self) {
        spawn_local(async move {
            match api::get::<Vec<TaskInfo>>(&format!("/api/v1/tasks?limit={CAP}")).await {
                Ok(mut l) => {
                    l.truncate(CAP);
                    self.list.set(l);
                    self.loaded.set(true);
                    self.err.set(None);
                }
                Err(e) if e.status() == 401 => {}
                Err(e) => self.err.set(Some(e.code().to_string())),
            }
        });
    }

    /// Signed out: the next user reads the list again.
    pub fn clear(self) {
        self.list.set(Vec::new());
        self.loaded.set(false);
        self.selected.set(None);
        self.open.set(false);
    }

    /// A `task.*` event (or a fresh `GET /tasks/{id}`): insert or replace by id.
    pub fn upsert(self, task: TaskInfo) {
        let finished_config = task.status.is_finished() && task.kind.starts_with("config.");
        let finished_docker = task.status.is_finished() && (task.kind.starts_with("docker.") || task.kind.starts_with("apps."));
        let known = finished_docker && self.list.with_untracked(|l| l.iter().any(|t| t.id == task.id && t.status.is_finished()));
        self.list.update(|l| match l.iter_mut().find(|t| t.id == task.id) {
            Some(t) => *t = task,
            None => {
                l.insert(0, task);
                l.truncate(CAP);
            }
        });
        self.now_ms.set(js_sys::Date::now());
        // an apply or overwrite changed the pending set and the revisions
        if finished_config {
            crate::config::changed();
        }
        // an install, a migration, a stack or app action changed what the Docker and Apps pages show
        if finished_docker && !known {
            crate::docker::docker().resync();
        }
    }

    pub fn on_event(self, ev: TaskEvent) {
        self.upsert(ev.task);
    }

    pub fn find(self, id: &str) -> Option<TaskInfo> {
        self.list.with_untracked(|l| l.iter().find(|t| t.id == id).cloned())
    }

    /// Open the panel on the log of task `id`.
    pub fn show(self, id: &str) {
        self.selected.set(Some(id.to_string()));
        self.open.set(true);
    }
}

/// Wait until task `id` ends and return its final state. Events update the store; without a
/// live stream (or as a safety net every 10 s) the task is asked for directly.
pub async fn follow(id: &str) -> Result<TaskInfo, ApiErr> {
    let s = tasks();
    let url = format!("/api/v1/tasks/{}", api::encode(id));
    let mut waited = 0u32;
    loop {
        if let Some(t) = s.find(id)
            && t.status.is_finished()
        {
            return Ok(t);
        }
        if !auth().is_ready() {
            return Err(ApiErr::Http { status: 401, code: rynas_api_types::error_codes::UNAUTHENTICATED.into(), retry_after: None, message: String::new(), field: None });
        }
        if waited == 0 || waited.is_multiple_of(10_000) || (!s.stream_live.get_untracked() && waited.is_multiple_of(2_000)) {
            match api::get::<TaskInfo>(&url).await {
                Ok(t) if t.status.is_finished() => {
                    s.upsert(t.clone());
                    return Ok(t);
                }
                Ok(t) => s.upsert(t),
                Err(e) if e.status() == 404 => return Err(e),
                Err(_) => {}
            }
        }
        api::sleep(500).await;
        waited += 500;
    }
}

/// Start an action that answers 202 `TaskStarted` (`POST` with an optional JSON `body`) and
/// follow it to its end; the task shows in the panel meanwhile.
///
/// ```ignore
/// spawn_local(async move {
///     match run_task("/api/v1/storage/volumes/data/scrub", None).await {
///         Ok(t) if t.status == TaskStatus::Ok => notice.set(Some((Tone::Ok, t("scrub.done").into()))),
///         Ok(t) => notice.set(Some((Tone::Err, task_error(&t)))),  // failed or cancelled
///         Err(e) => notice.set(Some((Tone::Err, e.text()))),           // refused
///     }
/// });
/// ```
/// Use [`api::start_task`] + [`follow`] when the task id is needed while it runs (progress).
pub async fn run_task(url: &str, body: Option<String>) -> Result<TaskInfo, ApiErr> {
    let id = api::start_task(url, body).await?;
    follow(&id).await
}

/// The one way to start a task from a button and say how it ended: `busy` while it runs, a
/// refusal (the task never started) in `error` (the dialog's `form_error`, which stays open),
/// the outcome in a toast (`ok` on success, the task's error otherwise) and then `done`
/// (close the dialog, reload). Row actions without a dialog show `error` as a toast.
pub fn run_and_report(start: impl std::future::Future<Output = Result<String, ApiErr>> + 'static, ok: String, busy: RwSignal<bool>, error: RwSignal<Option<String>>, done: Callback<()>) {
    busy.set(true);
    error.set(None);
    leptos::task::spawn_local(async move {
        let r = match start.await {
            Ok(id) => follow(&id).await,
            Err(e) => Err(e),
        };
        let _ = busy.try_set(false);
        match r {
            Ok(task) if task.status == TaskStatus::Ok => {
                toast(Tone::Ok, ok);
                done.try_run(());
            }
            Ok(task) => {
                toast(Tone::Err, task_error(&task));
                done.try_run(());
            }
            Err(e) => {
                let _ = error.try_set(Some(e.text()));
            }
        }
    });
}


/// Translated name of a task type (`task.<type>` in i18n), the type itself when unknown.
pub fn task_label(kind: &str) -> String {
    i18n::try_t(&format!("task.{kind}")).map(str::to_string).unwrap_or_else(|| kind.to_string())
}

/// Outcome of a finished task that did not succeed, for [`crate::ui::err_view`]: the
/// translated status, then the task's own error message (system output, English) as the
/// technical detail.
pub fn task_error(task: &TaskInfo) -> String {
    match task.status {
        TaskStatus::Cancelled => t("task.st.cancelled").to_string(),
        _ => match &task.error {
            Some(e) if !e.is_empty() => format!("{}{}{e}", t("task.failed"), crate::api::DETAIL_SEP),
            _ => t("task.failed").to_string(),
        },
    }
}

pub fn status_tone(s: TaskStatus) -> Tone {
    match s {
        TaskStatus::Ok => Tone::Ok,
        TaskStatus::Error => Tone::Err,
        TaskStatus::Cancelled => Tone::Warn,
        TaskStatus::Running => Tone::Info,
        TaskStatus::Queued => Tone::Unknown,
    }
}

pub fn status_key(s: TaskStatus) -> &'static str {
    match s {
        TaskStatus::Ok => "task.st.ok",
        TaskStatus::Error => "task.st.error",
        TaskStatus::Cancelled => "task.st.cancelled",
        TaskStatus::Running => "task.st.running",
        TaskStatus::Queued => "task.st.queued",
    }
}

fn elapsed(task: &TaskInfo, now_ms: f64) -> String {
    let end = task.end_ms.map(|e| e as f64).unwrap_or(now_ms);
    i18n::elapsed(((end - task.start_ms as f64).max(0.0) / 1000.0) as u64)
}

/// Bottom panel: summary row (always visible), list of recent tasks and the live log of the
/// selected one.
#[component]
pub fn TaskPanel() -> impl IntoView {
    let s = tasks();
    let open = s.open;
    let running = Memo::new(move |_| s.list.with(|l| l.iter().filter(|t| !t.status.is_finished()).count()));
    let summary = move || {
        let n = running.get();
        if n > 0 {
            let first = s.list.with(|l| l.iter().find(|t| !t.status.is_finished()).cloned());
            let what = first.map(|t| {
                let p = t.progress.map(|p| format!(" · {p}%")).unwrap_or_default();
                format!("{}{p}", task_label(&t.kind))
            });
            let lead = if n == 1 { t("tasks.running_1").to_string() } else { i18n::tf("tasks.running_n", &[("n", &n.to_string())]) };
            view! {
                <span class="btn-spinner" aria-hidden="true"></span>
                <span class="tasks-summary">{lead}{what.map(|w| format!(": {w}"))}</span>
            }
            .into_any()
        } else {
            match s.list.with(|l| l.first().cloned()) {
                Some(last) => {
                    let tone = status_tone(last.status);
                    view! {
                        <span class="tasks-summary">{format!("{}: {}", t("tasks.last"), task_label(&last.kind))}</span>
                        <Status tone=tone text=t(status_key(last.status)).to_string() />
                    }
                    .into_any()
                }
                None => view! { <span class="tasks-summary">{t("tasks.none")}</span> }.into_any(),
            }
        }
    };
    view! {
        <section class="tasks" aria-label=tr("tasks.title")>
            <button class="tasks-head" aria-expanded=move || open.get().to_string() aria-controls="tasks-body" on:click=move |_| open.update(|o| *o = !*o)>
                <Icon icon=icons::LIST_CHECKS size=16 />
                <span class="tasks-title">{tr("tasks.title")}</span>
                {summary}
                <span class="tasks-chev"><Icon icon=icons::CHEVRON_UP size=16 /></span>
            </button>
            <Show when=move || open.get()>
                <div id="tasks-body" class="tasks-body" class:with-log=move || s.selected.with(Option::is_some)>
                    <TaskList />
                    {move || s.selected.get().map(|id| view! { <LogView id=id /> })}
                </div>
            </Show>
        </section>
    }
}

#[component]
fn TaskList() -> impl IntoView {
    let s = tasks();
    move || {
        if let Some(code) = s.err.get().filter(|_| !s.loaded.get()) {
            return view! { <div class="tasks-pad"><ErrorBox code=code on_retry=Callback::new(move |_| s.reload()) /></div> }.into_any();
        }
        if s.list.with(Vec::is_empty) {
            return view! {
                <div class="tasks-empty">
                    <Icon icon=icons::HISTORY size=22 />
                    <div>
                        <strong>{tr("tasks.none")}</strong>
                        <p>{tr("tasks.none_text")}</p>
                    </div>
                </div>
            }
            .into_any();
        }
        let rows = s.list.with(|l| {
            l.iter()
                .map(|task| {
                    let id = task.id.clone();
                    let id2 = id.clone();
                    let selected = Signal::derive(move || s.selected.with(|x| x.as_deref() == Some(id.as_str())));
                    let st = task.status;
                    let pct = task.progress.filter(|_| !st.is_finished()).map(|p| format!(" {p}%")).unwrap_or_default();
                    let link = view! {
                        <button class="row-link" title=task.id.clone() on:click=move |_| s.selected.set(Some(id2.clone()))>
                            {task_label(&task.kind)}
                        </button>
                    }
                    .into_any();
                    let t2 = task.clone();
                    row_flag(
                        vec![
                            cell("tasks.col_task", "", link),
                            cell("col.status", "", status(status_tone(st), format!("{}{pct}", t(status_key(st))))),
                            cell("tasks.col_user", "", text(task.user.clone())),
                            cell("tasks.col_start", "tnum", text(i18n::datetime(task.start_ms))),
                            cell("tasks.col_duration", "num", (move || elapsed(&t2, s.now_ms.get())).into_any()),
                        ],
                        "selected",
                        selected,
                    )
                })
                .collect::<Vec<_>>()
        });
        view! {
            <div class="task-list">
                {table("tasks-table", &["tasks.col_task", "col.status", "tasks.col_user", "tasks.col_start", "#tasks.col_duration"], rows)}
            </div>
        }
        .into_any()
    }
}

/// Ask the server to stop task `id`; `busy` while the request runs, `err` when it was refused
/// (a task that already ended is not an error).
pub fn cancel_task(id: String, busy: RwSignal<bool>, err: RwSignal<Option<String>>) {
    busy.set(true);
    err.set(None);
    spawn_local(async move {
        let url = format!("/api/v1/tasks/{}/cancel", api::encode(&id));
        match api::post_empty::<()>(&url, None).await {
            Ok(()) => {}
            Err(e) if e.code() == rynas_api_types::error_codes::TASK_FINISHED => {}
            Err(e) => {
                let _ = err.try_set(Some(e.text()));
            }
        }
        let _ = busy.try_set(false);
    });
}

/// Characters of a log kept in the view (the server caps a task log at 1 MiB).
const LOG_KEEP: usize = 64 * 1024;

/// Live tail of a task log, shared by the task panel and the pages that show a task in place:
/// reads `/tasks/{id}/log` from the last offset until the server says it is complete. While
/// nothing new arrives it waits 2 s between reads, and it does not read while the tab is
/// hidden; it stops when unmounted. The task's state comes from the event stream, not from
/// here ([`follow`]).
#[component]
pub fn LogTail(id: String, #[prop(into)] running: Signal<bool>, #[prop(optional)] class: &'static str) -> impl IntoView {
    let text = RwSignal::new(String::new());
    let err = RwSignal::new(None::<String>);
    let complete = RwSignal::new(false);
    let pre = NodeRef::<leptos::html::Pre>::new();
    let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let gen_ = StoredValue::new(0u32);
    let id = StoredValue::new(id);

    let start = {
        let alive = alive.clone();
        move || {
            gen_.update_value(|g| *g += 1);
            let my = gen_.get_value();
            let id = id.get_value();
            let alive = alive.clone();
            err.set(None);
            spawn_local(async move {
                let mut offset = 0u64;
                let live = move || alive.load(std::sync::atomic::Ordering::Relaxed) && gen_.try_get_value() == Some(my) && auth().is_ready();
                loop {
                    if !live() {
                        return;
                    }
                    if document().hidden() {
                        api::sleep(2000).await;
                        continue;
                    }
                    match api::task_log(&id, offset).await {
                        Ok(c) => {
                            if !live() {
                                return;
                            }
                            let got = !c.text.is_empty();
                            if got {
                                let follow = pre.get_untracked().is_none_or(|p| p.scroll_height() - p.scroll_top() - p.client_height() < 24);
                                text.update(|t| {
                                    t.push_str(&c.text);
                                    if t.len() > LOG_KEEP {
                                        let cut = t.len() - LOG_KEEP;
                                        let cut = (cut..t.len()).find(|i| t.is_char_boundary(*i)).unwrap_or(t.len());
                                        t.drain(..cut);
                                    }
                                });
                                if follow && let Some(p) = pre.get_untracked() {
                                    request_animation_frame(move || p.set_scroll_top(p.scroll_height()));
                                }
                            }
                            offset = c.next;
                            if c.complete {
                                complete.set(true);
                                return;
                            }
                            if !got {
                                api::sleep(2000).await;
                            }
                        }
                        Err(e) => {
                            if e.status() != 401 && live() {
                                err.set(Some(e.code().to_string()));
                            }
                            return;
                        }
                    }
                }
            });
        }
    };
    let start = StoredValue::new(start);
    start.with_value(|f| f());
    on_cleanup(move || alive.store(false, std::sync::atomic::Ordering::Relaxed));
    move || match err.get() {
        Some(code) => view! {
            <div class="tasks-pad">
                <ErrorBox code=code on_retry=Callback::new(move |_| {
                    text.set(String::new());
                    start.with_value(|f| f());
                }) />
            </div>
        }
        .into_any(),
        None => view! {
            <pre class=format!("log {class}") node_ref=pre tabindex="0" aria-label=tr("tasks.log") aria-live="off">
                {move || text.get()}
                {move || (!complete.get() && running.get()).then(|| view! { <span class="log-cursor" aria-hidden="true"></span> })}
            </pre>
        }
        .into_any(),
    }
}

/// Log of one task in the panel: title, status, Stop, and the [`LogTail`].
#[component]
fn LogView(id: String) -> impl IntoView {
    let s = tasks();
    let cancel_err = RwSignal::new(None::<String>);
    let cancelling = RwSignal::new(false);
    let task = {
        let id = id.clone();
        Memo::new(move |_| s.list.with(|l| l.iter().find(|t| t.id == id).cloned()))
    };
    let running = Signal::derive(move || task.with(|t| t.as_ref().is_some_and(|t| !t.status.is_finished())));
    let id_full = id.clone();
    let cancel_id = id.clone();
    view! {
        <div class="task-log" role="region" aria-label=tr("tasks.log")>
            <div class="task-log-head">
                <button class="icon-btn sm only-narrow" aria-label=tr("back") on:click=move |_| s.selected.set(None)>
                    <Icon icon=icons::ARROW_LEFT size=16 />
                </button>
                <div class="task-log-title">
                    <strong class="ellipsis">{move || task.with(|t| t.as_ref().map(|t| task_label(&t.kind)).unwrap_or_default())}</strong>
                    <span class="mono small muted ellipsis" title=id_full.clone()>{id_full.clone()}</span>
                </div>
                {move || task.get().map(|tk| view! { <Status tone=status_tone(tk.status) text=t(status_key(tk.status)).to_string() /> })}
                <Show when=move || running.get()>
                    <button class="btn ghost sm" disabled=move || cancelling.get() on:click={
                        let id = cancel_id.clone();
                        move |_| cancel_task(id.clone(), cancelling, cancel_err)
                    }>
                        <Icon icon=icons::CIRCLE_X size=14 />{tr("tasks.cancel")}
                    </button>
                </Show>
                <button class="icon-btn sm" aria-label=tr("tasks.close_log") on:click=move |_| s.selected.set(None)>
                    <Icon icon=icons::X size=16 />
                </button>
            </div>
            {crate::form::form_error(cancel_err)}
            <LogTail id=id running=running />
        </div>
    }
}

/// A task shown where it was started (storage plans): status, progress, error, the live log
/// and Stop. The state comes from the task store (events of `/events`, [`follow`] as the
/// fallback); `on_done` runs once when the task ends, if the view is still there.
#[component]
pub fn TaskRun(id: String, on_done: Callback<TaskInfo>) -> impl IntoView {
    let s = tasks();
    let lost = RwSignal::new(None::<String>);
    let cancelling = RwSignal::new(false);
    let cancel_err = RwSignal::new(None::<String>);
    let task = {
        let id = id.clone();
        Memo::new(move |_| s.list.with(|l| l.iter().find(|t| t.id == id).cloned()))
    };
    {
        let id = id.clone();
        spawn_local(async move {
            match follow(&id).await {
                Ok(t) => {
                    if lost.try_get_untracked().is_some() {
                        on_done.run(t);
                    }
                }
                Err(e) if e.status() == 401 => {}
                Err(e) => {
                    let _ = lost.try_set(Some(e.text()));
                }
            }
        });
    }
    let status = Signal::derive(move || task.with(|t| t.as_ref().map(|t| t.status).unwrap_or(TaskStatus::Queued)));
    let running = Signal::derive(move || !status.get().is_finished());
    let cancel_id = StoredValue::new(id.clone());
    view! {
        <div class="task-run" aria-live="polite">
            <div class="task-run-head">
                {move || view! { <Status tone=status_tone(status.get()) text=t(status_key(status.get())).to_string() /> }}
                <span class="small muted ellipsis">{move || task.with(|t| t.as_ref().map(|t| task_label(&t.kind)).unwrap_or_default())}</span>
                <span class="row-end tnum small">{move || task.with(|t| t.as_ref().and_then(|t| t.progress)).map(|p| i18n::pct(p as f64))}</span>
            </div>
            <Meter
                pct=Signal::derive(move || {
                    task.with(|t| t.as_ref().map(|t| if t.status.is_finished() { 100.0 } else { t.progress.unwrap_or(0) as f64 }).unwrap_or(0.0))
                })
                tone=Signal::derive(move || status_tone(status.get()))
            />
            {move || task.with(|t| t.as_ref().filter(|t| t.status.is_finished() && t.status != TaskStatus::Ok).map(task_error)).map(|e| view! {
                <p class="form-error" role="alert"><Icon icon=icons::CIRCLE_X size=16 />{crate::ui::err_view(e)}</p>
            })}
            {crate::form::form_error(lost)}
            {crate::form::form_error(cancel_err)}
            <LogTail id=id running=running class="run-log" />
            {move || running.get().then(|| view! {
                <div class="dialog-foot">
                    <span class="muted small">{tr("tasks.background")}</span>
                    <button class="btn ghost sm" type="button" disabled=move || cancelling.get()
                        on:click=move |_| cancel_task(cancel_id.get_value(), cancelling, cancel_err)>
                        {tr("tasks.cancel")}
                    </button>
                </div>
            })}
        </div>
    }
}
