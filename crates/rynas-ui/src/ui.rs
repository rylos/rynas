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

/// [`ErrorBox`] for a read whose failure is already a message ([`crate::api::ApiErr::text`]:
/// a plan, a listing asked by hand): the same look and Retry.
pub fn error_box_text(msg: String, on_retry: Callback<()>) -> AnyView {
    view! {
        <div class="error-state" role="alert">
            <span class="error-icon"><Icon icon=icons::TRIANGLE_ALERT size=22 /></span>
            <div class="error-body">
                <strong>{tr("error.load")}</strong>
                <p class="muted small">{err_view(msg)}</p>
            </div>
            <button class="btn ghost sm" on:click=move |_| on_retry.run(())>
                <Icon icon=icons::REFRESH_CW size=14 />{tr("err.retry")}
            </button>
        </div>
    }
    .into_any()
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

    /// Like [`Remote::new`] without the first fetch: nothing is asked until [`Remote::reload`].
    pub fn idle(url: impl Into<String>) -> Self {
        Self::make(url.into())
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
            Callback::new(move |text: String| match crate::api::decode::<T>(&text) {
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
                // a set notifies even when the value stays the same: views that read the error
                // (gates, `not_found` pages) would be built again on every successful poll
                if err.try_with_untracked(Option::is_some) == Some(true) {
                    let _ = err.try_set(None);
                }
                // the body counts as seen only once applied: two reloads in flight (a config
                // generation bump right after a dialog's own reload) dispose the first one's
                // `apply` with its effect run, and the second answer, equal to the first, must
                // still reach the data
                if src.try_with_value(|s| s.1 != text) == Some(true) && apply.try_run(text.clone()).is_some() {
                    let _ = src.try_update_value(|s| s.1 = text);
                }
            }
            // 401: the API layer shows the login screen
            Err(e) if e.status() == 401 => {}
            Err(e) => {
                let code = e.code().to_string();
                if err.try_with_untracked(|cur| cur.as_deref() != Some(code.as_str())) == Some(true) {
                    let _ = err.try_set(Some(code));
                }
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

/// Rows keyed by id (`<For>`): a row stays while its id is listed and moves with it. Not
/// generic: one compiled copy for every keyed list.
pub fn keyed(ids: Signal<Vec<String>>, row: Callback<String, AnyView>) -> AnyView {
    view! { <For each=move || ids.get() key=|id| id.clone() children=move |id| row.run(id) /> }.into_any()
}

/// Row hidden while `hidden` (a filter of a keyed table: rows are kept, not rebuilt).
pub fn row_hidden(cells: Vec<AnyView>, hidden: Signal<bool>) -> AnyView {
    view! { <tr hidden=move || hidden.get()>{cells}</tr> }.into_any()
}

/// A table in its scroll wrapper. `heads`: header keys; a key starting with `#` is a numeric
/// column (right-aligned), an empty key the actions column (named for screen readers only).
pub fn table(class: &'static str, heads: &[&'static str], rows: Vec<AnyView>) -> AnyView {
    table_body(class, heads, rows.into_any())
}

/// [`table`] around any body: a keyed `<For>` of rows that are kept while the list changes
/// (hover, focus and per-row state survive a refresh).
pub fn table_body(class: &'static str, heads: &[&'static str], body: AnyView) -> AnyView {
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
                <tbody>{body}</tbody>
            </table>
        </div>
    }
    .into_any()
}

/// Plain text as a view.
pub fn text(s: String) -> AnyView {
    s.into_any()
}

/// Text whose `backquoted` parts are commands, shown in the monospace font.
pub fn text_code(s: &str) -> AnyView {
    s.split('`')
        .enumerate()
        .map(|(i, p)| if i % 2 == 1 { view! { <code class="mono">{p.to_string()}</code> }.into_any() } else { p.to_string().into_any() })
        .collect_view()
        .into_any()
}

/// A plain text cell (one compiled copy for every such cell).
pub fn text_cell(label: &'static str, class: &'static str, s: String) -> AnyView {
    cell(label, class, text(s))
}

/// A main line and an optional smaller line under it (`cell-main` / `cell-sub`).
pub fn two_lines(main: AnyView, sub: Option<String>, sub_class: &'static str) -> AnyView {
    view! { <div class="cell-main">{main}{sub.map(|s| view! { <span class=sub_class>{s}</span> })}</div> }.into_any()
}

/// A long reference (image `registry/repo:tag`, path) that wraps after a `/` or before a `:`
/// or `@` (`<wbr>`), not inside `home-assistant` or `10.11.2`; a part wider than the cell
/// still breaks (`overflow-wrap: anywhere` of table cells), so nothing scrolls sideways (G7).
pub fn breakable(s: &str, class: &'static str) -> AnyView {
    let mut parts: Vec<AnyView> = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        if matches!(c, ':' | '@') && !cur.is_empty() {
            parts.push(std::mem::take(&mut cur).into_any());
            parts.push(view! { <wbr /> }.into_any());
        }
        cur.push(c);
        if c == '/' {
            parts.push(std::mem::take(&mut cur).into_any());
            parts.push(view! { <wbr /> }.into_any());
        }
    }
    if !cur.is_empty() {
        parts.push(cur.into_any());
    }
    view! { <span class=class>{parts}</span> }.into_any()
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
    /// [`DlgKind::Drawer`], 880 px wide (status drawers with tables: PC backups, shares,
    /// snapshot jobs, browsing a snapshot).
    WideDrawer,
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
    // a change in flight (its primary or danger button shows the spinner; a test or a preview
    // does not count) keeps the dialog open: Esc, × and the
    // backdrop would drop the callbacks its answer still needs (a password shown once, a reload)
    let busy = move || node.get_untracked().and_then(|d| d.query_selector(":is(.btn.primary, .btn.danger) .btn-spinner").ok().flatten()).is_some();
    let close = move || {
        if !busy() {
            on_close.run(());
        }
    };
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
        if let Some(Some(el)) = opener.try_update_value(Option::take) {
            // a list rebuilt meanwhile replaced the opener: the button with the same
            // `data-opener` takes the focus back (Esc returns to where the user was)
            let back = if el.is_connected() {
                Some(el)
            } else {
                el.get_attribute("data-opener")
                    .and_then(|key| document().query_selector(&format!("[data-opener=\"{key}\"]")).ok().flatten())
                    .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
            };
            if let Some(el) = back {
                let _ = el.focus();
            }
        }
    });
    let class = match kind {
        DlgKind::Normal => "modal",
        DlgKind::Wide => "modal wide",
        DlgKind::Drawer => "modal drawer",
        DlgKind::WideDrawer => "modal drawer drawer-wide",
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
                close();
            }
            on:pointerdown=move |e: web_sys::PointerEvent| press_on_backdrop.set_value(e.target() == e.current_target())
            on:click=move |e: web_sys::MouseEvent| {
                // the dialog element itself only receives clicks on its backdrop
                if e.target() == e.current_target() && press_on_backdrop.get_value() {
                    close();
                }
            }
        >
            <header class="dialog-head">
                {(kind == DlgKind::Alert).then(|| view! { <span class="dialog-icon"><Icon icon=icons::TRIANGLE_ALERT size=18 /></span> })}
                <h2 id=id>{move || title.get()}</h2>
                <button class="icon-btn sm" type="button" aria-label=tr("close") on:click=move |_| close()>
                    <Icon icon=icons::X size=16 />
                </button>
            </header>
            <div class="dialog-body">{body}</div>
            {foot.map(|f| view! { <footer class="dialog-foot">{f}</footer> })}
        </dialog>
    }
    .into_any()
}

/// After a refused submit: the first field marked invalid (in the open dialog, else the page)
/// takes the focus and scrolls into view, so its error is seen next to it. Runs on the next
/// frame, once the errors are rendered.
pub fn focus_invalid() {
    use wasm_bindgen::JsCast;
    request_animation_frame(|| {
        let doc = document();
        let sel = "[aria-invalid=\"true\"]";
        let el = doc
            .query_selector_all("dialog[open]")
            .ok()
            .and_then(|l| l.item(l.length().checked_sub(1)?))
            .and_then(|d| d.dyn_into::<web_sys::Element>().ok())
            .and_then(|d| d.query_selector(sel).ok().flatten())
            .or_else(|| doc.query_selector(sel).ok().flatten());
        // focusing scrolls the field into view
        if let Some(el) = el.and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok()) {
            let _ = el.focus();
        }
    });
}

/// [`dialog`] as a component (`<Modal title=… on_close=… foot=…>…</Modal>`), centered or,
/// with `drawer`, on the right-hand side.
#[component]
pub fn Modal(
    #[prop(into)] title: Signal<String>,
    on_close: Callback<()>,
    #[prop(optional)] drawer: bool,
    /// Wider dialog (diffs, logs, plans); with `drawer`, the wide drawer.
    #[prop(optional)]
    wide: bool,
    /// The action row (Cancel / the action), outside the scrolling body. Re-run when what
    /// it reads changes (a dialog with stages shows the buttons of the current one); a row
    /// with no buttons is hidden.
    #[prop(optional, into)]
    foot: Option<ViewFn>,
    children: Children,
) -> impl IntoView {
    let kind = match (drawer, wide) {
        (true, true) => DlgKind::WideDrawer,
        (true, false) => DlgKind::Drawer,
        (false, true) => DlgKind::Wide,
        (false, false) => DlgKind::Normal,
    };
    dialog(kind, title, on_close, children(), foot.map(|f| (move || f.run()).into_any()))
}

/// Side drawer (an editor): title, scrolling body, the action row at the bottom.
pub fn drawer(title: Signal<String>, close: Callback<()>, body: AnyView, foot: AnyView) -> AnyView {
    dialog(DlgKind::Drawer, title, close, body, Some(foot))
}

/// [`drawer`], 880 px wide (status drawers with tables).
pub fn drawer_wide(title: Signal<String>, close: Callback<()>, body: AnyView, foot: AnyView) -> AnyView {
    dialog(DlgKind::WideDrawer, title, close, body, Some(foot))
}

/// The ghost Close button, alone at the bottom right of a dialog that only shows something
/// (a log, a result, a status drawer without actions). Never primary: nothing is decided.
pub fn close_foot(close: Callback<()>) -> AnyView {
    view! { <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("close")}</button> }.into_any()
}

/// The destructive action of a status drawer (Remove, Delete): left in the footer, away from
/// the others ([`detail_drawer`]).
pub struct Danger {
    /// i18n key of the button.
    pub label: &'static str,
    pub icon: &'static str,
    pub disabled: Signal<bool>,
    /// Usually opens a [`confirm`].
    pub on_click: Callback<()>,
}

impl Danger {
    /// A Remove/Delete button with the bin icon.
    pub fn new(label: &'static str, on_click: Callback<()>) -> Self {
        Danger { label, icon: icons::TRASH_2, disabled: Signal::stored(false), on_click }
    }
}

/// The footer of a status drawer, the PC backup drawer's pattern: the danger action on the
/// left (`foot-start`), the secondary actions (Settings, Edit…) on the right, then Close
/// (ghost). Actions that act on one section sit on that section's title row, not here.
pub fn drawer_foot(danger: Option<Danger>, secondary: Option<AnyView>, close: Callback<()>) -> AnyView {
    let danger = danger.map(|d| {
        let (go, off) = (d.on_click, d.disabled);
        view! {
            <button type="button" class="btn danger foot-start" disabled=move || off.get() on:click=move |_| go.run(())>
                <Icon icon=d.icon size=16 />{tr(d.label)}
            </button>
        }
    });
    view! { {danger}{secondary}{close_foot(close)} }.into_any()
}

/// A status drawer (a PC, a stack, an app, a drive, an interface): status card and sections in
/// the body, [`drawer_foot`] at the bottom; `wide` for drawers with tables.
pub fn detail_drawer(title: Signal<String>, close: Callback<()>, wide: bool, body: AnyView, danger: Option<Danger>, secondary: Option<AnyView>) -> AnyView {
    let kind = if wide { DlgKind::WideDrawer } else { DlgKind::Drawer };
    dialog(kind, title, close, body, Some(drawer_foot(danger, secondary, close)))
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
        <button type="button" class="btn ghost" data-autofocus="" disabled=move || busy.get() on:click=move |_| cancel.run(())>{tr("cancel")}</button>
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

/// [`kv_row`] whose value is any view (a status pill, a link, a chip row).
pub fn kv(label: &'static str, value: AnyView) -> AnyView {
    view! { <div class="kv"><dt>{tr(label)}</dt><dd>{value}</dd></div> }.into_any()
}

/// [`kv_row`] of a fixed text (`class`: `mono`, `tnum`, `muted`…).
pub fn kv_text(label: &'static str, value: String, class: &'static str) -> AnyView {
    view! { <div class="kv"><dt>{tr(label)}</dt><dd class=class>{value}</dd></div> }.into_any()
}

/// What a missing value shows (never run, not measured, no address).
pub const DASH: &str = "—";

/// A date and time ([`crate::i18n::datetime`]), or [`DASH`] when there is none.
pub fn when(ms: Option<u64>) -> String {
    ms.map(crate::i18n::datetime).unwrap_or_else(|| DASH.to_string())
}

/// The one line of a section with nothing in it ("No snapshots yet"): small and muted. A page
/// or tab with nothing in it is an [`EmptyState`].
pub fn empty_line(key: &'static str) -> AnyView {
    view! { <p class="small muted empty-line">{tr(key)}</p> }.into_any()
}

/// A short tag next to a name (SMR, weak key, erases, inverted), coloured by `tone`. A state
/// (running, failed) is a [`Status`] pill, not a chip.
pub fn chip(tone: Tone, content: AnyView) -> AnyView {
    view! { <span class=format!("chip {}", tone.class())>{content}</span> }.into_any()
}

/// What an icon-only button does to what: the accessible name and tooltip of a row action,
/// "<Verb> <name>" ("Delete marco", "Stop portainer").
pub fn verb_name(verb: &'static str, name: &str) -> String {
    format!("{} {name}", crate::i18n::t(verb))
}

/// An icon-only row action. `label` is its tooltip and accessible name ([`verb_name`]);
/// `danger` for delete/remove (red on hover).
pub fn icon_btn(icon: &'static str, label: String, danger: bool, disabled: Signal<bool>, on_click: Callback<()>) -> AnyView {
    view! {
        <button type="button" class=if danger { "icon-btn sm danger" } else { "icon-btn sm" } title=label.clone() aria-label=label
            disabled=move || disabled.get() on:click=move |_| on_click.run(())>
            <Icon icon=icon size=16 />
        </button>
    }
    .into_any()
}

/// Copy to the clipboard: async Clipboard API when present (secure contexts), else the old
/// textarea + execCommand path (plain-HTTP LAN addresses).
pub fn copy_text(text: &str) -> bool {
    use wasm_bindgen::{JsCast, JsValue};
    let nav = window().navigator();
    let has_api = js_sys::Reflect::get(&nav, &JsValue::from_str("clipboard")).is_ok_and(|v| !v.is_undefined());
    if has_api && window().is_secure_context() {
        let _ = nav.clipboard().write_text(text);
        return true;
    }
    let doc = document();
    let Ok(ta) = doc.create_element("textarea") else { return false };
    let ta: web_sys::HtmlTextAreaElement = ta.unchecked_into();
    ta.set_value(text);
    let _ = ta.set_attribute("style", "position:fixed;opacity:0;top:0;left:0");
    let Some(body) = doc.body() else { return false };
    let _ = body.append_child(&ta);
    ta.select();
    let ok = doc.unchecked_ref::<web_sys::HtmlDocument>().exec_command("copy").unwrap_or(false);
    ta.remove();
    ok
}

/// Copy button with a short "Copied" feedback (one shared instantiation). `label`: i18n key
/// of its text.
pub fn copy_button(label: &'static str, text: String) -> AnyView {
    copy_button_aria(label, None, text)
}

/// [`copy_button`] with its own accessible name (`aria`), for views with several copy buttons
/// that would otherwise all read "Copy".
pub fn copy_button_for(label: &'static str, aria: String, text: String) -> AnyView {
    copy_button_aria(label, Some(aria), text)
}

fn copy_button_aria(label: &'static str, aria: Option<String>, text: String) -> AnyView {
    let copied = RwSignal::new(false);
    view! {
        <button class="btn ghost sm" type="button" aria-label=aria on:click=move |_| {
            if copy_text(&text) {
                copied.set(true);
                set_timeout(move || { let _ = copied.try_set(false); }, std::time::Duration::from_millis(1800));
            }
        }>
            {move || view! { <Icon icon=if copied.get() { icons::CHECK } else { icons::COPY } size=14 /> }}
            {move || if copied.get() { crate::i18n::t("copied") } else { crate::i18n::t(label) }}
        </button>
    }
    .into_any()
}

/// A titled command or config block with its own Copy button (named after the title for
/// screen readers); nothing when the text is empty.
pub fn code_block(title: &'static str, text: String) -> Option<AnyView> {
    if text.trim().is_empty() {
        return None;
    }
    let aria = crate::i18n::tf("pcb.copy_of", &[("what", crate::i18n::t(title))]);
    Some(
        view! {
            <h4 class="small"><strong>{tr(title)}</strong></h4>
            <pre class="app-code">{text.clone()}</pre>
            <div class="sec-actions">{copy_button_for("dns.copy", aria, text)}</div>
        }
        .into_any(),
    )
}

/// Which [`fold`]s of a view are open, by a stable key (a section's title, a run's
/// timestamp): what the user chose, kept while the content is built again (a poll, a reload
/// rebuilds a drawer's body); a fold never touched keeps its default.
#[derive(Clone, Copy)]
pub struct Folds(RwSignal<std::collections::HashMap<String, bool>>);

impl Default for Folds {
    fn default() -> Self {
        Self::new()
    }
}

impl Folds {
    pub fn new() -> Self {
        Folds(RwSignal::new(Default::default()))
    }

    /// The `open` attribute of the fold `key`.
    pub fn open(self, key: impl Into<String>, default: bool) -> impl Fn() -> bool + Send + Sync + Copy + 'static {
        let key = StoredValue::new(key.into());
        move || self.0.with(|m| key.with_value(|k| m.get(k).copied()).unwrap_or(default))
    }

    /// A `toggle` event of the fold `key`: remember its state.
    pub fn toggled(self, key: &str, ev: &web_sys::Event) {
        let open = event_target::<web_sys::Element>(ev).has_attribute("open");
        if self.0.with_untracked(|m| m.get(key).copied()) != Some(open) {
            self.0.update(|m| {
                m.insert(key.to_string(), open);
            });
        }
    }
}

/// A folded part of a view (advanced options, history, a log, rare actions): a `<details>`
/// with a chevron, the title (i18n key) and an optional `summary` after it (a state, a count).
/// With `folds` the open state survives a rebuild of the content.
pub fn fold(folds: Option<Folds>, title: &'static str, open: bool, summary: Option<AnyView>, body: AnyView) -> AnyView {
    let remembered = folds.map(|f| f.open(title, open));
    let is_open = move || remembered.map(|g| g()).unwrap_or(open);
    view! {
        <details class="fold" open=is_open on:toggle=move |ev| if let Some(f) = folds { f.toggled(title, &ev) }>
            <summary><Icon icon=icons::CHEVRON_RIGHT size=14 /><span class="fold-title">{tr(title)}</span>{summary}</summary>
            <div class="fold-body">{body}</div>
        </details>
    }
    .into_any()
}

/// [`fold`] whose title is built (a count: "3 steps", "2 known issues"), with what follows it.
pub fn fold_with(title: AnyView, open: bool, body: AnyView) -> AnyView {
    view! {
        <details class="fold" open=open>
            <summary><Icon icon=icons::CHEVRON_RIGHT size=14 /><span class="fold-title">{title}</span></summary>
            <div class="fold-body">{body}</div>
        </details>
    }
    .into_any()
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
        // the full window from the first sample on: never widened while the history fills
        assert_eq!(window_len(300), 300);
        assert_eq!(window_len(0), 2);
        assert_eq!(span_minutes(300), 5);
        assert_eq!(span_minutes(60), 1);
    }

    #[test]
    fn usage() {
        assert_eq!(usage_tone(59.0), Tone::Ok);
        assert_eq!(usage_tone(85.0), Tone::Warn);
        assert_eq!(usage_tone(95.0), Tone::Err);
    }
}
