//! Form pieces shared by every page: labelled fields with help and inline errors, switches,
//! selects, segmented controls, tabs, the form error box, the button spinner and the save bar
//! of an editor. Loading is [`crate::ui::Remote`]; dialogs are [`crate::ui::dialog`].
//!
//! Everything here is non-generic where possible (`Callback`, `Signal`, `AnyView`) so that
//! each page adds its markup only once to the wasm.

use std::cell::Cell;

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::i18n::{t, tr};
use crate::icons::{self, Icon};

thread_local! {
    static NEXT_ID: Cell<u32> = const { Cell::new(0) };
}

/// A fresh DOM id (`f12`) for label/help/error wiring.
pub fn fid() -> String {
    NEXT_ID.with(|n| {
        let v = n.get() + 1;
        n.set(v);
        format!("f{v}")
    })
}

// ------------------------------------------------------------------ feedback

/// Spinner inside a button while `busy` (one shared instantiation for every button).
pub fn spinner(busy: RwSignal<bool>) -> impl IntoView {
    move || busy.get().then(|| view! { <span class="btn-spinner" aria-hidden="true"></span> })
}

/// Error box of a form or dialog, announced to screen readers. The message is usually
/// [`crate::api::ApiErr::text`] (translated text + server detail, see [`crate::ui::err_view`]).
pub fn form_error(error: RwSignal<Option<String>>) -> impl IntoView {
    view! {
        {crate::config::apply_line(error)}
        <div class="form-errors" role="alert" aria-live="assertive">
            {move || error.get().map(|msg| view! {
                <p class="form-error">
                    <Icon icon=icons::CIRCLE_X size=16 />
                    {crate::ui::err_view(msg)}
                </p>
            })}
        </div>
    }
}

// ------------------------------------------------------------------ drafts

/// The draft of a settings form (SMB, NFS, Synology hardware…) next to the server's value it
/// started from. When the server's value changes (a save, an apply, another administrator), a
/// clean draft follows it; an edited draft is kept and [`Draft::stale`] turns on, so the form
/// can offer "changed elsewhere — reload" ([`stale_notice`]) instead of dropping the edits.
pub struct Draft<T: Send + Sync + 'static> {
    /// The server's value the draft is compared with (Save is enabled when they differ).
    pub base: RwSignal<T>,
    /// What the form edits.
    pub d: RwSignal<T>,
    /// The server changed while the draft had edits.
    pub stale: RwSignal<bool>,
    newer: StoredValue<Option<T>>,
}

impl<T: Send + Sync + 'static> Clone for Draft<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: Send + Sync + 'static> Copy for Draft<T> {}

impl<T: Clone + PartialEq + Send + Sync + 'static> Draft<T> {
    /// A draft of `init` that follows `server` (read in an effect).
    pub fn new(init: T, server: impl Fn() -> Option<T> + Send + Sync + 'static) -> Self {
        let dr = Draft { base: RwSignal::new(init.clone()), d: RwSignal::new(init), stale: RwSignal::new(false), newer: StoredValue::new(None) };
        Effect::new(move |_| {
            let Some(v) = server() else { return };
            if dr.base.with_untracked(|b| *b == v) {
                return;
            }
            if dr.dirty_untracked() && dr.d.with_untracked(|d| *d != v) {
                dr.newer.set_value(Some(v));
                dr.stale.set(true);
            } else {
                dr.base.set(v.clone());
                dr.d.set(v);
                dr.stale.set(false);
            }
        });
        dr
    }

    fn dirty_untracked(self) -> bool {
        self.base.with_untracked(|b| self.d.with_untracked(|d| d != b))
    }

    /// The draft differs from the server's value (tracked).
    pub fn dirty(self) -> Signal<bool> {
        Signal::derive(move || self.base.with(|b| self.d.with(|d| d != b)))
    }

    /// Back to the server's value, the newest one when it changed meanwhile.
    pub fn reset(self) {
        if let Some(v) = self.newer.try_update_value(Option::take).flatten() {
            self.base.set(v);
        }
        self.d.set(self.base.get_untracked());
        self.stale.set(false);
    }
}

/// "Changed elsewhere since you started editing" with Reload (drops the edits) — shown while
/// a [`Draft`] is stale.
pub fn stale_notice(stale: RwSignal<bool>, reload: Callback<()>) -> AnyView {
    view! {
        <Show when=move || stale.get()>
            {crate::ui::note(crate::ui::Tone::Warn, icons::TRIANGLE_ALERT, view! {
                {tr("form.changed_elsewhere")}" "
                <button type="button" class="link-btn" on:click=move |_| reload.run(())>{tr("form.reload")}</button>
            }.into_any())}
        </Show>
    }
    .into_any()
}

// ------------------------------------------------------------------ lenses

/// Text view of one field of a draft and its setter.
pub fn lens<T: Send + Sync + 'static>(
    d: RwSignal<T>,
    get: fn(&T) -> String,
    set: fn(&mut T, String),
) -> (Signal<String>, Callback<String>) {
    (Signal::derive(move || d.with(get)), Callback::new(move |v| d.update(|x| set(x, v))))
}

/// Boolean field of a draft and its setter.
pub fn lens_bool<T: Send + Sync + 'static>(
    d: RwSignal<T>,
    get: fn(&T) -> bool,
    set: fn(&mut T, bool),
) -> (Signal<bool>, Callback<bool>) {
    (Signal::derive(move || d.with(get)), Callback::new(move |v| d.update(|x| set(x, v))))
}

// ------------------------------------------------------------------ fields

/// Options of a text field.
#[derive(Clone, Copy)]
pub struct In {
    /// `text`, `email`, `number`, `password`, `date`.
    pub kind: &'static str,
    pub placeholder: &'static str,
    /// i18n key of the help line under the control.
    pub help: Option<&'static str>,
    pub mono: bool,
    pub disabled: bool,
    pub autocomplete: &'static str,
    pub maxlength: u32,
}

impl Default for In {
    fn default() -> Self {
        In { kind: "text", placeholder: "", help: None, mono: false, disabled: false, autocomplete: "off", maxlength: 256 }
    }
}

fn describe(help: &Option<String>, err: &Option<String>, has_err: bool) -> Option<String> {
    let mut v = Vec::new();
    if has_err && let Some(e) = err {
        v.push(e.clone());
    }
    if let Some(h) = help {
        v.push(h.clone());
    }
    (!v.is_empty()).then(|| v.join(" "))
}

fn help_and_error(help: Option<&'static str>, hid: Option<String>, eid: String, err: Signal<Option<String>>) -> AnyView {
    view! {
        {help.zip(hid).map(|(h, id)| view! { <span class="field-help" id=id>{tr(h)}</span> })}
        <span class="field-error" id=eid aria-live="polite">
            {move || err.get().map(|e| view! { <Icon icon=icons::CIRCLE_X size=12 />{crate::ui::err_view(e)} })}
        </span>
    }
    .into_any()
}

/// Labelled text input with help and an inline error (`aria-invalid`, `aria-describedby`).
/// The error shows once the field has been left (not on every keystroke of the first entry);
/// an error that arrives while the field is not focused (server answer) shows at once.
pub fn text_field(label: &'static str, value: Signal<String>, set: Callback<String>, err: Signal<Option<String>>, o: In) -> AnyView {
    let id = fid();
    let touched = RwSignal::new(false);
    let focused = RwSignal::new(false);
    let err = Signal::derive(move || err.get().filter(|_| touched.get() || !focused.get()));
    let hid = o.help.map(|_| format!("{id}h"));
    let eid = format!("{id}e");
    let desc = {
        let (hid, eid) = (hid.clone(), Some(eid.clone()));
        move || describe(&hid, &eid, err.with(Option::is_some))
    };
    view! {
        <div class="field">
            <label class="field-label" for=id.clone()>{tr(label)}</label>
            <input
                id=id
                class=if o.mono { "input mono" } else { "input" }
                type=o.kind
                placeholder=o.placeholder
                autocomplete=o.autocomplete
                maxlength=o.maxlength
                spellcheck="false"
                disabled=o.disabled
                aria-invalid=move || err.with(Option::is_some).then_some("true")
                aria-describedby=desc
                prop:value=move || value.get()
                on:input=move |ev| set.run(event_target_value(&ev))
                on:focus=move |_| {
                    // an error already on screen (a submit that went to this field) stays
                    if err.with_untracked(Option::is_some) {
                        touched.set(true);
                    }
                    focused.set(true);
                }
                on:blur=move |_| {
                    focused.set(false);
                    touched.set(true);
                }
            />
            {help_and_error(o.help, hid, eid, err)}
        </div>
    }
    .into_any()
}

/// Labelled multi-line text (extra options, lists).
pub fn area_field(label: &'static str, value: Signal<String>, set: Callback<String>, help: Option<&'static str>, placeholder: &'static str) -> AnyView {
    let id = fid();
    let hid = help.map(|_| format!("{id}h"));
    view! {
        <div class="field">
            <label class="field-label" for=id.clone()>{tr(label)}</label>
            <textarea
                id=id
                class="input area mono"
                rows="4"
                spellcheck="false"
                placeholder=placeholder
                aria-describedby=hid.clone()
                prop:value=move || value.get()
                on:input=move |ev| set.run(event_target_value(&ev))
            ></textarea>
            {help.zip(hid).map(|(h, id)| view! { <span class="field-help" id=id>{tr(h)}</span> })}
        </div>
    }
    .into_any()
}

/// Labelled code editor (compose files, `.env`): a monospace textarea with line numbers, no
/// wrapping (indentation matters), an inline error under it. Plain text, no highlighting:
/// nothing extra in the wasm. With `upload` (the file picker's `accept`), a "Load from a file"
/// button next to the label reads a file of the computer into it, in the browser.
pub fn code_field(label: &'static str, value: Signal<String>, set: Callback<String>, err: Signal<Option<String>>, rows: u32, upload: Option<&'static str>) -> AnyView {
    let id = fid();
    let eid = format!("{id}e");
    let gutter = NodeRef::<leptos::html::Pre>::new();
    let lines = Memo::new(move |_| value.with(|v| v.split('\n').count().max(1)));
    let numbers = move || (1..=lines.get()).map(|n| n.to_string()).collect::<Vec<_>>().join("\n");
    let picker = upload.map(|accept| {
        let pid = format!("{id}f");
        let load = move |ev: leptos::ev::Event| {
            let Some(input) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()) else { return };
            let Some(file) = input.files().and_then(|f| f.get(0)) else { return };
            let p = file.text();
            // the same file can be picked again after an edit
            input.set_value("");
            leptos::task::spawn_local(async move {
                if let Ok(v) = wasm_bindgen_futures::JsFuture::from(p).await
                    && let Some(s) = v.as_string()
                {
                    set.run(s);
                }
            });
        };
        view! {
            <label class="btn ghost sm code-file" for=pid.clone()><Icon icon=icons::FILE_INPUT size=14 />{tr("load_file")}</label>
            <input id=pid class="sr-only" type="file" accept=accept on:change=load />
        }
    });
    view! {
        <div class="field">
            <div class="code-head">
                <label class="field-label" for=id.clone()>{tr(label)}</label>
                {picker}
            </div>
            <div class="code-edit" style=format!("--rows:{rows}")>
                <pre class="code-gutter" node_ref=gutter aria-hidden="true">{numbers}</pre>
                <textarea
                    id=id
                    class="code-area"
                    wrap="off"
                    spellcheck="false"
                    autocapitalize="off"
                    autocomplete="off"
                    aria-invalid=move || err.with(Option::is_some).then_some("true")
                    aria-describedby=eid.clone()
                    prop:value=move || value.get()
                    on:input=move |ev| set.run(event_target_value(&ev))
                    on:scroll=move |ev| {
                        if let Some(g) = gutter.get_untracked() {
                            g.set_scroll_top(event_target::<web_sys::HtmlElement>(&ev).scroll_top());
                        }
                    }
                ></textarea>
            </div>
            <span class="field-error" id=eid aria-live="polite">
                {move || err.get().map(|e| view! { <Icon icon=icons::CIRCLE_X size=12 />{crate::ui::err_view(e)} })}
            </span>
        </div>
    }
    .into_any()
}

/// Switch with a label and an optional help line.
pub fn switch(label: &'static str, help: Option<&'static str>, value: Signal<bool>, set: Callback<bool>) -> AnyView {
    view! {
        <label class="switch-row">
            <input type="checkbox" role="switch" class="switch" prop:checked=move || value.get() on:change=move |ev| set.run(event_target_checked(&ev)) />
            <span class="switch-text">
                <span class="switch-label">{tr(label)}</span>
                {help.map(|h| view! { <span class="field-help">{tr(h)}</span> })}
            </span>
        </label>
    }
    .into_any()
}

/// Labelled select; `opts` are (value, i18n key) pairs.
pub fn select_field(
    label: &'static str,
    help: Option<&'static str>,
    opts: &'static [(&'static str, &'static str)],
    value: Signal<String>,
    set: Callback<String>,
) -> AnyView {
    let id = fid();
    let hid = help.map(|_| format!("{id}h"));
    view! {
        <div class="field">
            <label class="field-label" for=id.clone()>{tr(label)}</label>
            <select id=id class="input" aria-describedby=hid.clone() on:change=move |ev| set.run(event_target_value(&ev))>
                {opts
                    .iter()
                    .map(|(v, k)| view! { <option value=*v selected=move || value.with(|x| x == v)>{tr(k)}</option> })
                    .collect_view()}
            </select>
            {help.zip(hid).map(|(h, id)| view! { <span class="field-help" id=id>{tr(h)}</span> })}
        </div>
    }
    .into_any()
}

/// Labelled segmented control (radio group, arrow keys move the choice); `opts` are
/// (value, i18n key) pairs.
pub fn segmented(label: &'static str, opts: &'static [(&'static str, &'static str)], value: Signal<String>, set: Callback<String>) -> AnyView {
    let id = fid();
    let n = opts.len();
    view! {
        <div class="field">
            <span class="field-label" id=id.clone()>{tr(label)}</span>
            <div class="segmented" role="radiogroup" aria-labelledby=id>
                {opts
                    .iter()
                    .enumerate()
                    .map(|(i, (v, k))| {
                        let on = move || value.with(|x| x == v);
                        view! {
                            <button
                                type="button"
                                role="radio"
                                aria-checked=move || on().to_string()
                                tabindex=move || if on() { "0" } else { "-1" }
                                class:on=on
                                on:click=move |_| set.run(v.to_string())
                                on:keydown=move |ev| {
                                    let next = match ev.key().as_str() {
                                        "ArrowRight" | "ArrowDown" => (i + 1) % n,
                                        "ArrowLeft" | "ArrowUp" => (i + n - 1) % n,
                                        _ => return,
                                    };
                                    ev.prevent_default();
                                    set.run(opts[next].0.to_string());
                                    if let Some(p) = event_target::<web_sys::HtmlElement>(&ev).parent_element()
                                        && let Some(b) = p.children().item(next as u32)
                                    {
                                        let _ = b.unchecked_into::<web_sys::HtmlElement>().focus();
                                    }
                                }
                            >
                                {tr(k)}
                            </button>
                        }
                    })
                    .collect_view()}
            </div>
        </div>
    }
    .into_any()
}

/// Number input bound to text (empty allowed: optional values).
pub fn number_field(label: &'static str, help: Option<&'static str>, value: Signal<String>, set: Callback<String>, min: i64, max: i64) -> AnyView {
    let id = fid();
    let hid = help.map(|_| format!("{id}h"));
    view! {
        <div class="field">
            <label class="field-label" for=id.clone()>{tr(label)}</label>
            <input id=id class="input tnum" type="number" inputmode="numeric" min=min max=max aria-describedby=hid.clone()
                prop:value=move || value.get() on:input=move |ev| set.run(event_target_value(&ev)) />
            {help.zip(hid).map(|(h, id)| view! { <span class="field-help" id=id>{tr(h)}</span> })}
        </div>
    }
    .into_any()
}

/// Tab strip (`role="tablist"`, arrow keys move between tabs). Panels use
/// `id="{prefix}-p{i}"` and `aria-labelledby="{prefix}-t{i}"`.
pub fn tabs(prefix: &'static str, keys: &'static [&'static str], sel: RwSignal<usize>, label: &'static str) -> AnyView {
    tabs_shown(prefix, keys, sel, label, |_| true)
}

/// [`tabs`] where `shown(i)` decides (reactively) whether tab `i` exists: a hidden tab is not in
/// the DOM and the arrow keys skip it (platform tabs that only exist with their hardware, G8d).
pub fn tabs_shown(
    prefix: &'static str,
    keys: &'static [&'static str],
    sel: RwSignal<usize>,
    label: &'static str,
    shown: impl Fn(usize) -> bool + Copy + Send + Sync + 'static,
) -> AnyView {
    let n = keys.len();
    // the next shown tab after `i` in direction `step` (1 or n - 1), wrapping; `i` when alone
    let step_from = move |i: usize, step: usize| (1..=n).map(|k| (i + k * step) % n).find(|&j| shown(j)).unwrap_or(i);
    let focus_tab = move |i: usize| {
        if let Some(el) = document().get_element_by_id(&format!("{prefix}-t{i}")) {
            let _ = el.unchecked_into::<web_sys::HtmlElement>().focus();
        }
    };
    view! {
        <div class="tabs" role="tablist" aria-label=tr(label)>
            {keys
                .iter()
                .enumerate()
                .map(|(i, k)| {
                    view! {
                        <Show when=move || shown(i)>
                        <button
                            type="button"
                            role="tab"
                            id=format!("{prefix}-t{i}")
                            class="tab"
                            aria-selected=move || (sel.get() == i).to_string()
                            aria-controls=format!("{prefix}-p{i}")
                            tabindex=move || if sel.get() == i { "0" } else { "-1" }
                            on:click=move |_| sel.set(i)
                            on:keydown=move |ev| {
                                let next = match ev.key().as_str() {
                                    "ArrowRight" => step_from(i, 1),
                                    "ArrowLeft" => step_from(i, n - 1),
                                    "Home" => step_from(n - 1, 1),
                                    "End" => step_from(0, n - 1),
                                    _ => return,
                                };
                                ev.prevent_default();
                                sel.set(next);
                                focus_tab(next);
                            }
                        >
                            {tr(k)}
                        </button>
                        </Show>
                    }
                })
                .collect_view()}
        </div>
    }
    .into_any()
}

/// Search box for tables.
pub fn search_box(label: &'static str, q: RwSignal<String>) -> AnyView {
    view! {
        <div class="table-search">
            <Icon icon=icons::SEARCH size=16 />
            <input class="input" type="search" placeholder=tr(label) aria-label=tr(label) prop:value=move || q.get() on:input=move |ev| q.set(event_target_value(&ev)) />
        </div>
    }
    .into_any()
}

/// Footer of an editor: the form error (always in sight), Cancel and Save (with spinner),
/// Save disabled while busy or invalid.
pub fn save_bar(
    busy: RwSignal<bool>,
    can_save: Signal<bool>,
    save_label: &'static str,
    cancel: Callback<()>,
    save: Callback<()>,
    error: RwSignal<Option<String>>,
) -> AnyView {
    view! {
        {form_error(error)}
        <button type="button" class="btn ghost" on:click=move |_| cancel.run(())>{tr("cancel")}</button>
        <button type="button" class="btn primary" disabled=move || busy.get() || !can_save.get() on:click=move |_| save.run(())>
            {spinner(busy)}
            {tr(save_label)}
        </button>
    }
    .into_any()
}

// ------------------------------------------------------------------ password strength

/// 0 (unusable) ..= 4 (strong): length, character classes, repeats and a few common words.
/// The server's rules are checked separately ([`password_problem`]).
pub fn strength(pw: &str, user: &str) -> u8 {
    let n = pw.chars().count();
    if n < 8 {
        return 0;
    }
    let lower = pw.to_lowercase();
    const COMMON: [&str; 10] = ["password", "123456", "qwerty", "letmein", "admin", "welcome", "abc123", "iloveyou", "monkey", "dragon"];
    if COMMON.iter().any(|c| lower.contains(c)) || (!user.is_empty() && lower.contains(&user.to_lowercase())) {
        return 1;
    }
    let classes = [
        pw.chars().any(|c| c.is_lowercase()),
        pw.chars().any(|c| c.is_uppercase()),
        pw.chars().any(|c| c.is_ascii_digit()),
        pw.chars().any(|c| !c.is_alphanumeric()),
    ]
    .iter()
    .filter(|b| **b)
    .count();
    let mut distinct: Vec<char> = pw.chars().collect();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.len() * 2 < n.min(16) {
        return 1;
    }
    let score = match n {
        8..=9 => 1,
        10..=13 => 2,
        14..=19 => 3,
        _ => 4,
    } + usize::from(classes >= 3);
    score.min(4) as u8
}

/// The server's password rules (8–256 characters, no control characters, not the user
/// name): i18n key of the first broken rule.
pub fn password_problem(pw: &str, user: &str) -> Option<&'static str> {
    let n = pw.chars().count();
    if n < 8 {
        Some("pw.too_short")
    } else if n > 256 {
        Some("pw.too_long")
    } else if pw.chars().any(char::is_control) {
        Some("pw.control")
    } else if pw == user {
        Some("pw.is_user")
    } else {
        None
    }
}

/// Four-segment strength meter with its label.
pub fn strength_meter(score: Signal<Option<u8>>) -> AnyView {
    const LABELS: [&str; 5] = ["pw.s0", "pw.s1", "pw.s2", "pw.s3", "pw.s4"];
    const TONES: [&str; 5] = ["err", "err", "warn", "ok", "ok"];
    view! {
        <div class="pw-meter" aria-live="polite">
            <div class="pw-bars" aria-hidden="true">
                {(1..=4u8).map(|i| view! {
                    <span class=move || match score.get() {
                        Some(s) if s >= i => format!("pw-bar on {}", TONES[s as usize]),
                        _ => "pw-bar".to_string(),
                    }></span>
                }).collect_view()}
            </div>
            <span class="pw-label">{move || score.get().map(|s| t(LABELS[s as usize])).unwrap_or("")}</span>
        </div>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_strength() {
        assert_eq!(strength("short", "alice"), 0);
        assert_eq!(strength("password123", "alice"), 1);
        assert_eq!(strength("alice2026!!", "alice"), 1);
        assert_eq!(strength("aaaaaaaaaaaa", "alice"), 1);
        assert_eq!(strength("tulip-river", "bob"), 2);
        assert!(strength("Correct-Horse-Battery-9", "bob") >= 4);
        assert_eq!(password_problem("abc", "bob"), Some("pw.too_short"));
        assert_eq!(password_problem("bobbobbob", "bobbobbob"), Some("pw.is_user"));
        assert_eq!(password_problem("tulip\u{7}river", "bob"), Some("pw.control"));
        assert_eq!(password_problem("tulip-river", "bob"), None);
    }
}
