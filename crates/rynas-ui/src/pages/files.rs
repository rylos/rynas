//! The file manager (decision D-FM, phase 1): the SynoFile interface ported as a Web Component,
//! `<rynas-files>`, served from `/files-app/` (trunk copies `files-app/` into the build).
//!
//! - The component's JavaScript is loaded only when this page opens (a module `<script>`
//!   added once), so it adds nothing to the wasm bundle or to the other pages.
//! - `/files/<share>/<folder>` opens that folder (attribute `path`); `?window` is the file
//!   manager in a browser window of its own (the shell's bars hidden, see `Shell`).
//! - The session's CSRF token goes to the component as the property `csrf`; the session cookie
//!   is HttpOnly and travels with its requests as with ours.
//! - The component talks back with window events ([`bridge`]): a 401 (session over → login
//!   screen), a page of the shell to open (share settings and permissions), a toast for a
//!   transfer that ended while another page was shown.

use std::cell::Cell;

use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_params_map};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

use crate::auth::auth;
use crate::i18n::t;
use crate::ui::{ErrorBox, Skeleton, Tone};

/// The component's entry module: the build's folder with a content hash (`hash-files-app.sh`,
/// `<meta name="rynas-files-app">`), or the plain folder of an unhashed build.
fn script_url() -> String {
    document()
        .query_selector("meta[name=rynas-files-app]")
        .ok()
        .flatten()
        .and_then(|m| m.get_attribute("content"))
        .unwrap_or_else(|| "/files-app/rynas-files.js".to_string())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Load {
    Idle,
    Loading,
    Ready,
    Failed,
}

thread_local! {
    /// State of the component's script for the life of the page (it is loaded once).
    static LOAD: Cell<Option<RwSignal<Load>>> = const { Cell::new(None) };
}

/// The script's state, made on first use under the app's root owner. Made under the Files
/// page's own owner it was disposed when the page was left, and coming back panicked
/// ("reactive value already disposed"), leaving the previous page on screen.
fn load_state() -> RwSignal<Load> {
    LOAD.with(|c| match c.get() {
        Some(s) => s,
        None => {
            let mut owner = Owner::current().expect("the files page needs a reactive owner");
            while let Some(p) = owner.parent() {
                owner = p;
            }
            let s = owner.with(|| RwSignal::new(Load::Idle));
            c.set(Some(s));
            s
        }
    })
}

/// Adds the component's module script (once; again after a failure).
fn load_script(state: RwSignal<Load>) {
    if matches!(state.get_untracked(), Load::Loading | Load::Ready) {
        return;
    }
    let doc = document();
    if let Some(old) = doc.query_selector("script[data-rynas-files]").ok().flatten() {
        old.remove();
    }
    let Ok(script) = doc.create_element("script") else {
        state.set(Load::Failed);
        return;
    };
    let _ = script.set_attribute("type", "module");
    let _ = script.set_attribute("src", &script_url());
    let _ = script.set_attribute("data-rynas-files", "");
    // the closures live as long as the page: the script is added once (or again after a
    // failure, which replaces them with new ones on a new element)
    let ok = Closure::<dyn Fn()>::new(move || state.set(Load::Ready));
    let err = Closure::<dyn Fn()>::new(move || state.set(Load::Failed));
    let _ = script.add_event_listener_with_callback("load", ok.as_ref().unchecked_ref());
    let _ = script.add_event_listener_with_callback("error", err.as_ref().unchecked_ref());
    ok.forget();
    err.forget();
    state.set(Load::Loading);
    if let Some(head) = doc.head() {
        let _ = head.append_child(&script);
    }
}

#[component]
pub fn Files() -> impl IntoView {
    let state = load_state();
    load_script(state);
    let params = use_params_map();
    let path = move || params.with(|p| p.get("path").unwrap_or_default());
    let a = auth();
    let csrf = move || a.session.with(|s| s.as_ref().map(|s| s.csrf_token.clone()).unwrap_or_default());
    let waiting = move || matches!(state.get(), Load::Idle | Load::Loading);

    view! {
        <div class="page files-page">
            <Show when=waiting>
                <div class="card files-wait"><Skeleton lines=6 /></div>
            </Show>
            <Show when=move || state.get() == Load::Failed>
                <div class="card files-wait">
                    // a reload, not a new attempt at the same URL: after an upgrade the old
                    // folder is gone and only the new page knows the new one
                    <ErrorBox code=Signal::derive(|| "network".to_string()) on_retry=Callback::new(move |_| { let _ = window().location().reload(); }) />
                </div>
            </Show>
            <rynas-files
                class=move || if state.get() == Load::Ready { "files-app" } else { "files-app files-hidden" }
                path=path
                aria-label=move || t("nav.files")
                prop:csrf=csrf
            ></rynas-files>
        </div>
    }
}

/// Listens to the component's window events for the whole life of the shell (a transfer may
/// end while another page is shown). Called once from `Shell`, inside the router.
pub fn bridge() {
    let navigate = use_navigate();
    let _ = window_event_listener_untyped("rynas-files:unauth", |_| crate::auth::session_lost());
    let _ = window_event_listener_untyped("rynas-files:navigate", move |ev| {
        // only paths of this interface, never a URL: the event comes from page scripts
        if let Some(p) = detail_str(&ev, "path").filter(|p| p.starts_with('/') && !p.starts_with("//")) {
            navigate(&p, NavigateOptions::default());
        }
    });
    let _ = window_event_listener_untyped("rynas-files:toast", |ev| {
        if let Some(text) = detail_str(&ev, "text") {
            let tone = match detail_str(&ev, "kind").as_deref() {
                Some("error") => Tone::Err,
                Some("info") => Tone::Info,
                _ => Tone::Ok,
            };
            crate::toast::toast(tone, text);
        }
    });
}

/// A string field of a `CustomEvent`'s `detail`.
fn detail_str(ev: &web_sys::Event, field: &str) -> Option<String> {
    let detail = js_sys::Reflect::get(ev, &"detail".into()).ok()?;
    js_sys::Reflect::get(&detail, &field.into()).ok()?.as_string()
}
