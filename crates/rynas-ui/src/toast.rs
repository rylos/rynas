//! Toasts: short confirmations of finished work ("saved, pending apply", "task finished") in
//! a live region at the bottom right (`aria-live="polite"`; errors go to an assertive region).
//! They disappear by themselves — not while the pointer or the focus is on them (WCAG 2.2.1) —
//! and can be closed. Errors inside a form or dialog are `form::form_error`; results the user
//! must read are `ui::Notice`.

use std::cell::Cell;
use std::time::Duration;

use leptos::prelude::*;

use crate::i18n::{t, tr};
use crate::icons::{self, Icon};
use crate::ui::{Tone, err_view};

#[derive(Clone, PartialEq)]
struct Toast {
    id: u32,
    tone: Tone,
    text: String,
}

thread_local! {
    static LIST: Cell<Option<RwSignal<Vec<Toast>>>> = const { Cell::new(None) };
    static HELD: Cell<Option<RwSignal<bool>>> = const { Cell::new(None) };
    static NEXT: Cell<u32> = const { Cell::new(1) };
}

/// At most this many toasts on screen; the oldest goes first.
const MAX: usize = 4;

fn list() -> RwSignal<Vec<Toast>> {
    LIST.with(|c| match c.get() {
        Some(s) => s,
        None => {
            let s = RwSignal::new(Vec::new());
            c.set(Some(s));
            s
        }
    })
}

/// The pointer or the focus is inside the stack: timers wait.
fn held() -> RwSignal<bool> {
    HELD.with(|c| match c.get() {
        Some(s) => s,
        None => {
            let s = RwSignal::new(false);
            c.set(Some(s));
            s
        }
    })
}

/// Creates the toast list outside any reactive owner (called from `main`).
pub fn init() {
    list();
    held();
}

/// Remove toast `id` after `ms`, later if the stack is held at that moment.
fn expire(id: u32, ms: u64) {
    set_timeout(
        move || {
            if held().get_untracked() {
                expire(id, 1500);
            } else {
                list().update(|l| l.retain(|x| x.id != id));
            }
        },
        Duration::from_millis(ms),
    );
}

/// Show a toast; errors stay longer. `text` may carry a server detail
/// ([`crate::api::ApiErr::text`]).
pub fn toast(tone: Tone, text: String) {
    let id = NEXT.with(|n| {
        let v = n.get();
        n.set(v.wrapping_add(1));
        v
    });
    list().update(|l| {
        if l.len() >= MAX {
            l.remove(0);
        }
        l.push(Toast { id, tone, text });
    });
    expire(id, if tone == Tone::Err { 8000 } else { 4000 });
}

/// "Saved, pending apply": a configuration write was staged.
pub fn saved_pending() {
    toast(Tone::Ok, t("toast.saved_pending").to_string());
}

/// "Saved and applied": the Save of a dialog or editor staged and applied its change
/// ([`crate::config::apply_now`]).
pub fn saved_applied() {
    toast(Tone::Ok, t("toast.saved_applied").to_string());
}

#[component]
pub fn Toasts() -> impl IntoView {
    let l = list();
    let h = held();
    let region = move |errors: bool| {
        view! {
            <div class="toasts" role=if errors { "alert" } else { "status" } aria-live=if errors { "assertive" } else { "polite" }>
                <For
                    each=move || l.with(|v| v.iter().filter(|x| (x.tone == Tone::Err) == errors).cloned().collect::<Vec<_>>())
                    key=|x| x.id
                    children=move |x| {
                        let id = x.id;
                        view! {
                            <div class=format!("toast {}", x.tone.class())>
                                <Icon icon=x.tone.icon() size=16 />
                                <span class="toast-text">{err_view(x.text)}</span>
                                <button class="icon-btn sm" aria-label=tr("close") on:click=move |_| l.update(|v| v.retain(|y| y.id != id))>
                                    <Icon icon=icons::X size=14 />
                                </button>
                            </div>
                        }
                    }
                />
            </div>
        }
    };
    view! {
        <div
            class="toast-stack"
            on:pointerenter=move |_| h.set(true)
            on:pointerleave=move |_| h.set(false)
            on:focusin=move |_| h.set(true)
            on:focusout=move |_| h.set(false)
        >
            {region(false)}
            {region(true)}
        </div>
    }
}
