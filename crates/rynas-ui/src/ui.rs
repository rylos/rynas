//! Small shared components of the design system.

use std::cell::Cell;

use leptos::prelude::*;

use crate::i18n::{t, tr};
use crate::icons::{self, Icon};
use crate::live::{Res, use_live};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    Ok,
    Warn,
    Err,
    Info,
    Unknown,
}

impl Tone {
    pub fn class(self) -> &'static str {
        match self {
            Tone::Ok => "ok",
            Tone::Warn => "warn",
            Tone::Err => "err",
            Tone::Info => "info",
            Tone::Unknown => "unknown",
        }
    }
    pub fn icon(self) -> &'static str {
        match self {
            Tone::Ok => icons::CIRCLE_CHECK,
            Tone::Warn => icons::TRIANGLE_ALERT,
            Tone::Err => icons::CIRCLE_X,
            Tone::Info => icons::INFO,
            Tone::Unknown => icons::CIRCLE_DASHED,
        }
    }
}

#[component]
pub fn Status(#[prop(into)] tone: Signal<Tone>, #[prop(into)] text: Signal<String>) -> impl IntoView {
    // the pill keeps its DOM node: only class and icon change, and only when the tone changes
    let tone = Memo::new(move |_| tone.get());
    view! {
        <span class=move || format!("status {}", tone.get().class())>
            {move || view! { <Icon icon=tone.get().icon() size=14 /> }}
            <span>{text}</span>
        </span>
    }
}

/// Horizontal usage bar; `pct` 0..=100. Animated with `transform` only (no layout).
#[component]
pub fn Meter(#[prop(into)] pct: Signal<f64>, #[prop(optional)] tone: Option<Signal<Tone>>) -> impl IntoView {
    let cls = move || format!("meter {}", tone.map(|t| t.get().class()).unwrap_or("accent"));
    view! {
        <div class=cls role="presentation">
            <div class="meter-fill" style:transform=move || format!("scaleX({})", (pct.get() / 100.0).clamp(0.0, 1.0))></div>
        </div>
    }
}

pub fn reduced_motion() -> bool {
    window()
        .match_media("(prefers-reduced-motion: reduce)")
        .ok()
        .flatten()
        .is_some_and(|m| m.matches())
}

/// Tone for a usage percentage.
pub fn usage_tone(pct: f64) -> Tone {
    if pct >= 95.0 {
        Tone::Err
    } else if pct >= 85.0 {
        Tone::Warn
    } else {
        Tone::Ok
    }
}

pub fn temp_tone(id: &str, c: f32) -> Tone {
    let (warn, crit) = temp_limits(id, cpu_offset());
    if c >= crit {
        Tone::Err
    } else if c >= warn {
        Tone::Warn
    } else {
        Tone::Ok
    }
}

thread_local! {
    static CPU_OFFSET: Cell<f32> = const { Cell::new(0.0) };
}

pub fn cpu_offset() -> f32 {
    CPU_OFFSET.with(Cell::get)
}

/// Remember the CPU model: some AMD parts report `Tctl` with a fixed offset over the real die
/// temperature, and the CPU thresholds move up by the same amount.
pub fn set_cpu_model(model: &str) {
    CPU_OFFSET.with(|c| c.set(tctl_offset(model)));
}

/// AMD's documented `Tctl` offsets (the k10temp driver subtracts them only for `Tdie`):
/// +20 °C on Ryzen 1600X/1700X/1800X, +10 °C on 2700X, +27 °C on Threadripper 1900X/1920X/1950X.
pub fn tctl_offset(cpu_model: &str) -> f32 {
    const TABLE: [(&str, f32); 7] =
        [("1600X", 20.0), ("1700X", 20.0), ("1800X", 20.0), ("2700X", 10.0), ("1900X", 27.0), ("1920X", 27.0), ("1950X", 27.0)];
    if !cpu_model.contains("AMD") {
        return 0.0;
    }
    TABLE.iter().find(|(m, _)| cpu_model.split_whitespace().any(|w| w == *m)).map(|(_, o)| *o).unwrap_or(0.0)
}

/// (warning, critical) thresholds in °C for a sensor (`Temperature::id`).
pub fn temp_limits(id: &str, cpu_offset: f32) -> (f32, f32) {
    if id.starts_with("cpu") {
        // package / Tctl: most desktop and embedded parts throttle at 95-105 °C
        (80.0 + cpu_offset, 90.0 + cpu_offset)
    } else if id.starts_with("nvme") {
        // NVMe composite: drives usually throttle around 80-85 °C
        (70.0, 80.0)
    } else if id.starts_with("nic:") {
        // 10GbE PHYs (Aquantia AQC) idle at 60-70 °C and are rated for ~100 °C
        (90.0, 100.0)
    } else {
        // hard disks and SATA SSDs ("bay3", "sda"): vendors rate them for 60-65 °C
        (55.0, 60.0)
    }
}

#[component]
pub fn Skeleton(#[prop(default = 1)] lines: usize, #[prop(default = "")] class: &'static str) -> impl IntoView {
    view! {
        <div class=format!("skeleton-group {class}") aria-busy="true" aria-label=tr("loading")>
            {(0..lines).map(|i| view! { <div class="skeleton" style:width=format!("{}%", 92 - (i * 17) % 40)></div> }).collect_view()}
        </div>
    }
}

#[component]
pub fn EmptyState(
    icon: &'static str,
    #[prop(into)] title: Signal<String>,
    #[prop(into)] text: Signal<String>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    view! {
        <div class="empty">
            <div class="empty-icon"><Icon icon=icon size=28 /></div>
            <h2 class="empty-title">{title}</h2>
            <p class="empty-text">{text}</p>
            {children.map(|c| c())}
        </div>
    }
}

/// Page title row; `children` go to the right-hand actions slot (Refresh, Create…).
#[component]
pub fn PageHeader(
    icon: &'static str,
    title: &'static str,
    subtitle: &'static str,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    view! {
        <header class="page-head">
            <div class="page-icon"><Icon icon=icon size=20 /></div>
            <div class="page-text">
                <h1>{move || t(title)}</h1>
                <p class="page-sub">{move || t(subtitle)}</p>
            </div>
            {children.map(|c| view! { <div class="page-actions">{c()}</div> })}
        </header>
    }
}

/// Card header used by widgets: tinted icon, title, optional right-hand content.
#[component]
pub fn WidgetHead(icon: &'static str, title: &'static str, #[prop(optional)] children: Option<Children>) -> impl IntoView {
    view! {
        <div class="w-head">
            <span class="w-icon"><Icon icon=icon size=16 /></span>
            <h2 class="w-title">{move || t(title)}</h2>
            <div class="w-aside">{children.map(|c| c())}</div>
        </div>
    }
}

/// What went wrong (from the `ApiError` code, `err.<code>` in i18n) and a Retry button.
/// Replaces the skeleton of a card whose fetch failed (rule G2).
#[component]
pub fn ErrorBox(#[prop(into)] code: Signal<String>, on_retry: Callback<()>) -> impl IntoView {
    view! {
        <div class="error-state" role="alert">
            <span class="error-icon"><Icon icon=icons::TRIANGLE_ALERT size=22 /></span>
            <div class="error-body">
                <strong>{tr("error.load")}</strong>
                <p class="muted small">{move || crate::i18n::err_text(&code.get())}</p>
            </div>
            <button class="btn ghost sm" on:click=move |_| on_retry.run(())>
                <Icon icon=icons::REFRESH_CW size=14 />{tr("err.retry")}
            </button>
        </div>
    }
}

/// Skeleton while nothing has arrived, the [`ErrorBox`] once the read failed. The one loading
/// placeholder of the UI ([`Remote`], [`Pending`], the storage store).
pub fn loading(err: Signal<Option<String>>, retry: Callback<()>, lines: usize) -> AnyView {
    (move || match err.get() {
        Some(code) => view! { <ErrorBox code=code on_retry=retry /> }.into_any(),
        None => view! { <Skeleton lines=lines /> }.into_any(),
    })
    .into_any()
}

/// A message built by [`crate::api::ApiErr::text`] (or any plain text): the translated text,
/// then the server's English detail, if any, as [`tech_detail`].
pub fn err_view(msg: String) -> AnyView {
    let (main, detail) = crate::api::split_detail(&msg);
    let detail = detail.map(|d| tech_detail(d.to_string()));
    view! { <span class="err-text">{main.to_string()}{detail}</span> }.into_any()
}

/// Text a program or the system wrote (command output, a server's English message): on its
/// own line, smaller, after "Technical details" and marked `lang="en"`. Never the main text.
pub fn tech_detail(text: String) -> AnyView {
    view! { <span class="err-detail">{t("tech_details")}" "<span lang="en">{text}</span></span> }.into_any()
}

/// A value one page fetches for itself (the shared ones live in [`crate::live::Live`]):
/// `data` stays while a reload runs, `err` holds the code of the last failed fetch.
///
/// - [`Remote::new`] reads once (Retry and [`Remote::reload`] read again);
///   [`Remote::with_config`] also re-reads after every configuration change
///   ([`crate::config::changed`]).
/// - `data` is set only when the answer differs from the last one, so an unrelated change
///   does not re-run what depends on it (form drafts, M3 of the wave-A review).
/// - The fetch itself is not generic (one compiled copy): only the JSON parse is per type.
///
/// Render with [`Remote::view`] (skeleton → content | [`ErrorBox`]) or [`Remote::gate`].
pub struct Remote<T: Send + Sync + 'static> {
    pub data: RwSignal<Option<T>>,
    pub err: RwSignal<Option<String>>,
    /// URL and the text of the last answer.
    src: StoredValue<(String, String)>,
    /// Something has arrived (changes once: views that only gate on it are not re-run when
    /// the data changes).
    loaded: Memo<bool>,
}

impl<T: Send + Sync + 'static> Clone for Remote<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: Send + Sync + 'static> Copy for Remote<T> {}

impl<T: serde::de::DeserializeOwned + Send + Sync + 'static> Remote<T> {
    fn make(url: String) -> Self {
        let data = RwSignal::new(None::<T>);
        Remote { data, err: RwSignal::new(None), src: StoredValue::new((url, String::new())), loaded: Memo::new(move |_| data.with(Option::is_some)) }
    }

    /// Starts the first fetch of `url` right away.
    pub fn new(url: impl Into<String>) -> Self {
        let r = Self::make(url.into());
        r.reload();
        r
    }

    /// Like [`Remote::new`], and read again after every configuration change.
    pub fn with_config(url: impl Into<String>) -> Self {
        let r = Self::make(url.into());
        Effect::new(move |_| {
            crate::config::generation();
            r.reload();
        });
        r
    }

    pub fn reload(self) {
        let data = self.data;
        let err = self.err;
        fetch_text(
            self.src,
            err,
            Callback::new(move |text: String| match serde_json::from_str::<T>(&text) {
                Ok(v) => {
                    let _ = data.try_set(Some(v));
                }
                Err(_) => {
                    let _ = err.try_set(Some("decode".into()));
                }
            }),
        );
    }

    pub fn retry(self) -> Callback<()> {
        Callback::new(move |_| self.reload())
    }

    /// Something has been loaded (tracked; changes only when the first answer arrives).
    pub fn ready(self) -> bool {
        self.loaded.get()
    }

    /// [`loading`] until the first answer, `None` once there is data.
    pub fn gate(self, lines: usize) -> Option<AnyView> {
        (!self.ready()).then(|| loading(self.err.into(), self.retry(), lines))
    }

    /// Skeleton until the first answer, the error box if the last fetch failed and nothing
    /// was loaded, `content` otherwise.
    pub fn view(self, lines: usize, content: impl Fn() -> AnyView + Send + Sync + 'static) -> AnyView {
        remote_state(self.loaded.into(), self.err, self.retry(), lines, std::sync::Arc::new(content))
    }
}

/// The request of [`Remote::reload`], not generic. `apply` gets the body when it differs from
/// the last one.
fn fetch_text(src: StoredValue<(String, String)>, err: RwSignal<Option<String>>, apply: Callback<String>) {
    let Some(url) = src.try_with_value(|s| s.0.clone()) else { return };
    leptos::task::spawn_local(async move {
        match crate::api::get_text(&url).await {
            Ok(text) => {
                let _ = err.try_set(None);
                if src.try_with_value(|s| s.1 != text) == Some(true) {
                    src.update_value(|s| s.1.clone_from(&text));
                    apply.try_run(text);
                }
            }
            // 401: the API layer shows the login screen
            Err(e) if e.status() == 401 => {}
            Err(e) => {
                let _ = err.try_set(Some(e.code().to_string()));
            }
        }
    });
}

/// Body of [`Remote::view`], not generic: one compiled copy for every remote value.
fn remote_state(
    loaded: Signal<bool>,
    err: RwSignal<Option<String>>,
    retry: Callback<()>,
    lines: usize,
    content: std::sync::Arc<dyn Fn() -> AnyView + Send + Sync>,
) -> AnyView {
    (move || if loaded.get() { content() } else { loading(err.into(), retry, lines) }).into_any()
}

/// Table cell with its label for the card layout on phones (`data-label`). Tables are built
/// from these non-generic helpers ([`cell`], [`actions`], [`row`], [`table`]) so that one
/// compiled copy serves every table (wasm size, guidelines 2.1).
pub fn cell(label: &'static str, class: &'static str, content: AnyView) -> AnyView {
    view! { <td data-label=tr(label) class=class>{content}</td> }.into_any()
}

/// The actions cell at the end of a row (buttons, right-aligned; no label on phones).
pub fn actions(content: AnyView) -> AnyView {
    view! { <td class="actions">{content}</td> }.into_any()
}

pub fn row(cells: Vec<AnyView>) -> AnyView {
    view! { <tr>{cells}</tr> }.into_any()
}

/// Row with a class (`muted-row`, `watched`…).
pub fn row_class(cells: Vec<AnyView>, class: &'static str) -> AnyView {
    view! { <tr class=class>{cells}</tr> }.into_any()
}

/// Row with `class` while `on` (`selected`: the one whose details or log are open).
pub fn row_flag(cells: Vec<AnyView>, class: &'static str, on: Signal<bool>) -> AnyView {
    view! { <tr class=(class, move || on.get())>{cells}</tr> }.into_any()
}

/// A table in its scroll wrapper. `heads`: header keys; a key starting with `#` is a numeric
/// column (right-aligned), an empty key the actions column (named for screen readers only).
pub fn table(class: &'static str, heads: &[&'static str], rows: Vec<AnyView>) -> AnyView {
    let heads = heads
        .iter()
        .map(|&h| {
            let (key, num) = match h.strip_prefix('#') {
                Some(k) => (k, true),
                None => (h, false),
            };
            if key.is_empty() {
                view! { <th class="actions-col"><span class="sr-only">{tr("col.actions")}</span></th> }.into_any()
            } else {
                view! { <th scope="col" class=if num { "num" } else { "" }>{tr(key)}</th> }.into_any()
            }
        })
        .collect::<Vec<_>>();
    view! {
        <div class="table-wrap">
            <table class=format!("table {class}")>
                <thead><tr>{heads}</tr></thead>
                <tbody>{rows}</tbody>
            </table>
        </div>
    }
    .into_any()
}

/// Plain text as a view.
pub fn text(s: String) -> AnyView {
    s.into_any()
}

/// [`Status`] pill as a view (tone + translated text).
pub fn status(tone: Tone, text: String) -> AnyView {
    view! { <Status tone=tone text=text /> }.into_any()
}

/// Pill for a staged change of a configuration object (add / change / delete).
pub fn pending_status(op: Option<rynas_api_types::PendingOp>) -> Option<AnyView> {
    use rynas_api_types::PendingOp;
    op.map(|op| {
        let key = match op {
            PendingOp::Add => "pending.add",
            PendingOp::Change => "pending.change",
            PendingOp::Delete => "pending.delete",
        };
        status(Tone::Info, t(key).to_string())
    })
}

// ------------------------------------------------------------------ dialogs

/// Kind of a [`dialog`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DlgKind {
    /// Centered, 560 px.
    Normal,
    /// Centered, 920 px (diffs, logs, plans).
    Wide,
    /// Right-hand side panel, full height (editors).
    Drawer,
    /// Centered confirmation (`role="alertdialog"`).
    Alert,
}

thread_local! {
    static DIALOG_IDS: Cell<u32> = const { Cell::new(0) };
}

/// The one dialog of the UI, on the native `<dialog>` element opened with `showModal()`: the
/// browser puts it in the top layer, makes the page behind inert (also for screen readers),
/// keeps Tab inside and turns `Esc` into `cancel`. On open the focus goes to the element marked
/// `autofocus`/`data-autofocus`, else the first field of the body (or of the action row); on
/// close it goes back to the element that opened it. A click on the backdrop closes it (only when the press also
/// started there: a text selection dragged out of a field keeps it open).
///
/// It is open while mounted: render it inside a `Show`/`Option` and let `on_close` remove it.
/// `foot` is a non-scrolling action row (Cancel / Save); without it the body may end with its
/// own `.dialog-foot`. Pages use [`Modal`] (component), [`drawer`] or [`confirm`].
pub fn dialog(kind: DlgKind, title: Signal<String>, on_close: Callback<()>, body: AnyView, foot: Option<AnyView>) -> AnyView {
    use wasm_bindgen::JsCast;
    let node = NodeRef::<leptos::html::Dialog>::new();
    let id = DIALOG_IDS.with(|c| {
        c.set(c.get() + 1);
        format!("dlg{}", c.get())
    });
    let opener = document().active_element().and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok());
    let opener = StoredValue::new_local(opener);
    let press_on_backdrop = StoredValue::new(false);
    node.on_load(move |d| {
        if !d.open() {
            let _ = d.show_modal();
        }
        let target = d
            .query_selector("[autofocus], [data-autofocus]")
            .ok()
            .flatten()
            .or_else(|| d.query_selector(".dialog-body :is(input, select, textarea):not([disabled]), .dialog-foot input:not([disabled])").ok().flatten());
        if let Some(el) = target.and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok()) {
            let _ = el.focus();
        }
    });
    on_cleanup(move || {
        if let Some(d) = node.get_untracked()
            && d.open()
        {
            d.close();
        }
        if let Some(Some(el)) = opener.try_update_value(Option::take)
            && el.is_connected()
        {
            let _ = el.focus();
        }
    });
    let class = match kind {
        DlgKind::Normal => "modal",
        DlgKind::Wide => "modal wide",
        DlgKind::Drawer => "modal drawer",
        DlgKind::Alert => "modal alert",
    };
    let label_id = id.clone();
    view! {
        <dialog
            node_ref=node
            class=class
            role=(kind == DlgKind::Alert).then_some("alertdialog")
            aria-labelledby=label_id
            on:cancel=move |e: web_sys::Event| {
                e.prevent_default();
                on_close.run(());
            }
            on:pointerdown=move |e: web_sys::PointerEvent| press_on_backdrop.set_value(e.target() == e.current_target())
            on:click=move |e: web_sys::MouseEvent| {
                // the dialog element itself only receives clicks on its backdrop
                if e.target() == e.current_target() && press_on_backdrop.get_value() {
                    on_close.run(());
                }
            }
        >
            <header class="dialog-head">
                {(kind == DlgKind::Alert).then(|| view! { <span class="dialog-icon"><Icon icon=icons::TRIANGLE_ALERT size=18 /></span> })}
                <h2 id=id>{move || title.get()}</h2>
                <button class="icon-btn sm" type="button" aria-label=tr("close") on:click=move |_| on_close.run(())>
                    <Icon icon=icons::X size=16 />
                </button>
            </header>
            <div class="dialog-body">{body}</div>
            {foot.map(|f| view! { <footer class="dialog-foot">{f}</footer> })}
        </dialog>
    }
    .into_any()
}

/// [`dialog`] as a component (`<Modal title=… on_close=…>…</Modal>`), centered or, with
/// `drawer`, on the right-hand side.
#[component]
pub fn Modal(
    #[prop(into)] title: Signal<String>,
    on_close: Callback<()>,
    #[prop(optional)] drawer: bool,
    /// Wider dialog (diffs, logs, plans).
    #[prop(optional)]
    wide: bool,
    children: Children,
) -> impl IntoView {
    let kind = if drawer {
        DlgKind::Drawer
    } else if wide {
        DlgKind::Wide
    } else {
        DlgKind::Normal
    };
    dialog(kind, title, on_close, children(), None)
}

/// Side drawer (an editor): title, scrolling body, the action row at the bottom.
pub fn drawer(title: Signal<String>, close: Callback<()>, body: AnyView, foot: AnyView) -> AnyView {
    dialog(DlgKind::Drawer, title, close, body, Some(foot))
}

/// What a [`confirm`] dialog asks.
pub struct Confirm {
    pub title: String,
    pub text: String,
    /// i18n key of the confirm button.
    pub action: &'static str,
    pub danger: bool,
    /// Extra content (options such as "also remove the home folder").
    pub extra: Option<AnyView>,
    /// When it holds a text, the confirm button stays disabled until that text is typed
    /// ([`typed_confirm`]; it may depend on an option of `extra`).
    pub typed: Signal<Option<String>>,
    /// The action is running: the button shows a spinner and the dialog stays open.
    pub busy: RwSignal<bool>,
    /// Why the action failed (shown in the dialog, which stays open).
    pub error: RwSignal<Option<String>>,
    pub on_confirm: Callback<()>,
    pub on_cancel: Callback<()>,
}

impl Confirm {
    /// A plain question: no extra content, no typed word, its own busy/error state.
    pub fn new(title: String, text: String, action: &'static str, danger: bool, on_confirm: Callback<()>, on_cancel: Callback<()>) -> Self {
        Confirm {
            title,
            text,
            action,
            danger,
            extra: None,
            typed: Signal::stored(None),
            busy: RwSignal::new(false),
            error: RwSignal::new(None),
            on_confirm,
            on_cancel,
        }
    }
}

/// The one confirmation of the UI ("are you sure?"): a [`dialog`] of kind `Alert` with the
/// question, optional extra content and typed confirmation, the error of a failed attempt,
/// Cancel (focused first) and the confirm button. `on_confirm` decides when it closes.
pub fn confirm(c: Confirm) -> AnyView {
    let typed_in = RwSignal::new(String::new());
    let typed = c.typed;
    let ok = move || typed.with(|w| w.as_ref().is_none_or(|w| typed_in.with(|x| x.trim() == w)));
    let (busy, cancel, go) = (c.busy, c.on_cancel, c.on_confirm);
    let body = view! {
        <p class="dialog-text">{c.text}</p>
        {c.extra}
        {move || typed.get().map(|w| typed_confirm(w, typed_in))}
        {crate::form::form_error(c.error)}
    }
    .into_any();
    let foot = view! {
        <button type="button" class="btn ghost" data-autofocus="" on:click=move |_| cancel.run(())>{tr("cancel")}</button>
        <button
            type="button"
            class=if c.danger { "btn danger" } else { "btn primary" }
            disabled=move || busy.get() || !ok()
            on:click=move |_| go.run(())
        >
            {crate::form::spinner(busy)}
            {tr(c.action)}
        </button>
    }
    .into_any();
    let kind = if c.danger { DlgKind::Alert } else { DlgKind::Normal };
    dialog(kind, Signal::stored(c.title), cancel, body, Some(foot))
}

/// Typed confirmation of a destructive action: the user types `expected` (a volume, share or
/// user name, or a word); `value` holds what was typed.
pub fn typed_confirm(expected: String, value: RwSignal<String>) -> AnyView {
    let prompt = crate::i18n::tf("confirm.type_name", &[("name", &expected)]);
    view! {
        <label class="field confirm-field">
            <span class="field-label">{prompt}</span>
            <input
                class="input mono"
                autocomplete="off"
                autocapitalize="off"
                spellcheck="false"
                prop:value=move || value.get()
                on:input=move |ev| value.set(event_target_value(&ev))
            />
        </label>
    }
    .into_any()
}

/// Outcome of an action, shown above the content it concerns (`role="status"`, or `alert` for
/// errors) with a dismiss button. For results the user must read (restore, drift, LUKS
/// header); short confirmations are toasts, errors inside a form are `form_error`.
#[component]
pub fn Notice(msg: RwSignal<Option<(Tone, String)>>) -> impl IntoView {
    move || {
        msg.get().map(|(tone, text)| {
            view! {
                <div class=format!("notice {}", tone.class()) role=if tone == Tone::Err { "alert" } else { "status" }>
                    <Icon icon=tone.icon() size=16 />
                    <span class="notice-text">{err_view(text)}</span>
                    <button class="icon-btn sm" aria-label=tr("close") on:click=move |_| msg.set(None)>
                        <Icon icon=icons::X size=14 />
                    </button>
                </div>
            }
        })
    }
}

/// A static note above or inside the content it concerns (a warning, a hint, a state):
/// tone + icon + text, `role="alert"` for errors, `status` otherwise. Not dismissable; the
/// outcome of an action is a [`Notice`].
pub fn note(tone: Tone, icon: &'static str, content: AnyView) -> AnyView {
    note_class(tone, icon, "", content)
}

/// [`note`] with an extra class (layout of a specific place).
pub fn note_class(tone: Tone, icon: &'static str, class: &'static str, content: AnyView) -> AnyView {
    view! {
        <div class=format!("notice {} {class}", tone.class()) role=if tone == Tone::Err { "alert" } else { "status" }>
            <Icon icon=icon size=16 />
            <span class="notice-text">{content}</span>
        </div>
    }
    .into_any()
}

/// Placeholder of a resource: skeleton while loading, error state when the last fetch failed.
#[component]
pub fn Pending(lines: usize, res: Res) -> impl IntoView {
    let live = use_live();
    loading(live.err(res).into(), Callback::new(move |_| live.refresh(res)), lines)
}

/// Form field: label and control (children; extra notes may follow the control).
#[component]
pub fn Field(label: &'static str, children: Children) -> impl IntoView {
    view! {
        <label class="field">
            <span class="field-label">{tr(label)}</span>
            {children()}
        </label>
    }
}

/// One row of a key/value list (`<dl class="kvs">`), shared by every page.
pub fn kv_row(label: &'static str, value: Signal<String>, class: &'static str) -> AnyView {
    view! { <div class="kv"><dt>{tr(label)}</dt><dd class=class>{move || value.get()}</dd></div> }.into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ryzen_1800x_tctl_at_63_is_fine() {
        // regression: 63 °C Tctl on a Ryzen 7 1800X (k10temp) must not be an error
        assert_eq!(temp_tone("cpu", 63.0), Tone::Ok);
        let off = tctl_offset("AMD Ryzen 7 1800X Eight-Core Processor");
        assert_eq!(off, 20.0);
        let (warn, crit) = temp_limits("cpu", off);
        assert_eq!((warn, crit), (100.0, 110.0));
        assert_eq!(tctl_offset("AMD Ryzen Embedded R1600"), 0.0);
        assert_eq!(tctl_offset("Intel(R) Celeron(R) J4125 CPU @ 2.00GHz"), 0.0);
    }

    #[test]
    fn cpu_thresholds() {
        assert_eq!(temp_tone("cpu", 79.9), Tone::Ok);
        assert_eq!(temp_tone("cpu", 80.0), Tone::Warn);
        assert_eq!(temp_tone("cpu", 90.0), Tone::Err);
    }

    #[test]
    fn drive_thresholds() {
        assert_eq!(temp_tone("nvme0", 63.0), Tone::Ok);
        assert_eq!(temp_tone("nvme0", 72.0), Tone::Warn);
        assert_eq!(temp_tone("nvme0", 81.0), Tone::Err);
        assert_eq!(temp_tone("bay2", 41.0), Tone::Ok);
        assert_eq!(temp_tone("sda", 56.0), Tone::Warn);
        assert_eq!(temp_tone("bay1", 61.0), Tone::Err);
        assert_eq!(temp_tone("nic:eth0", 68.0), Tone::Ok);
    }

    #[test]
    fn chart_window() {
        use crate::chart::{span_minutes, window_len};
        assert_eq!(window_len(3, 300), 60);
        assert_eq!(window_len(150, 300), 150);
        assert_eq!(window_len(900, 300), 300);
        assert_eq!(span_minutes(3, 300), 1);
        assert_eq!(span_minutes(150, 300), 3);
        assert_eq!(span_minutes(600, 300), 5);
    }

    #[test]
    fn usage() {
        assert_eq!(usage_tone(59.0), Tone::Ok);
        assert_eq!(usage_tone(85.0), Tone::Warn);
        assert_eq!(usage_tone(95.0), Tone::Err);
    }
}
