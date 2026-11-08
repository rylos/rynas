//! The timed transaction seen from the browser (D-NET, research note §3.1).
//!
//! After Apply (or a restore, or a live adoption) the server runs the change and waits for
//! `POST /network/confirm {token}` arriving over the new network; without it the previous
//! configuration comes back by itself at the deadline. This module shows the big countdown,
//! Confirm and Roll back, and survives what usually goes wrong:
//!
//! - the browser loses the connection (the management address changed): the page keeps
//!   polling, says so, and lists the addresses where the NAS answers after the change, linked
//!   with the token in the fragment (`/network#confirm=<token>`): there the user signs in and
//!   confirms. They are not probed from here: the page's CSP (`connect-src 'self'`) forbids
//!   requests to other origins, and a guess would be worse than no answer;
//! - the tab is reloaded: the answer of Apply is kept in `sessionStorage` (this tab only);
//! - another tab or user started the test: the countdown and Roll back are shown, Confirm only
//!   where the token is known;
//! - an ifupdown migration planned for the next boot: shown to every session (who, when,
//!   Cancel); once the boot carried it out nobody has a token, so any administrator may keep
//!   it or go back to ifupdown.
//!
//! Polling: `GET /network/state` every 2 s while a test runs (an exception to the 5 s rule of
//! the guidelines: the user is racing a deadline) and not at all otherwise.

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::network::{NetApplyStarted, NetConfirmRequest, NetConfirmed, NetTrial, NetworkState, TrialKind};
use rynas_api_types::{TaskStarted, TaskStatus};
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsCast;

use super::Net;
use crate::api::{self, ApiErr};
use crate::i18n::{t, tf, tr};
use crate::icons::{self, Icon};
use crate::ui::{self, Meter, Tone};

const STORE_KEY: &str = "rynas.net.trial";
const TOKEN_KEY: &str = "rynas.net.token";

/// What Apply answered, kept for this tab.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    pub s: NetApplyStarted,
    /// The address this page was opened at (where to come back after a rollback).
    pub origin: String,
}

#[derive(Clone, Copy)]
pub struct Trial {
    pub started: RwSignal<Option<Saved>>,
    /// Confirmation token: from Apply in this tab, or from the `#confirm=` link.
    pub token: RwSignal<Option<String>>,
    /// The last poll got no answer at all.
    pub lost: RwSignal<bool>,
    /// This tab follows the task of the test (its end decides the outcome).
    following: RwSignal<bool>,
    /// Local clock (ms), ticking once a second while a test runs.
    now: RwSignal<f64>,
    /// Server clock minus local clock (ms).
    skew: RwSignal<f64>,
    /// Outcome of the last test, shown above the page.
    pub outcome: RwSignal<Option<(Tone, String)>>,
    busy: RwSignal<bool>,
    error: RwSignal<Option<String>>,
}

fn session() -> Option<web_sys::Storage> {
    window().session_storage().ok().flatten()
}

fn store_get(key: &str) -> Option<String> {
    session()?.get_item(key).ok().flatten()
}

fn store_set(key: &str, value: Option<&str>) {
    if let Some(s) = session() {
        let _ = match value {
            Some(v) => s.set_item(key, v),
            None => s.remove_item(key),
        };
    }
}

fn origin() -> String {
    window().location().origin().unwrap_or_default()
}

/// `#confirm=<token>`: remember the token for this tab and drop it from the address bar.
fn token_from_fragment() -> Option<String> {
    let hash = window().location().hash().ok()?;
    let tok = hash.strip_prefix("#confirm=")?.trim().to_string();
    if tok.is_empty() || !tok.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return None;
    }
    store_set(TOKEN_KEY, Some(&tok));
    if let Ok(h) = window().history() {
        let path = window().location().pathname().unwrap_or_default();
        let _ = h.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&path));
    }
    Some(tok)
}

impl Trial {
    /// The signals; [`Trial::init`] wires them once the page's [`Net`] exists.
    pub fn new() -> Self {
        Trial {
            started: RwSignal::new(None),
            token: RwSignal::new(None),
            lost: RwSignal::new(false),
            now: RwSignal::new(js_sys::Date::now()),
            skew: RwSignal::new(0.0),
            outcome: RwSignal::new(None),
            busy: RwSignal::new(false),
            error: RwSignal::new(None),
            following: RwSignal::new(false),
        }
    }

    pub fn init(self, net: Net) {
        let tl = self;
        let saved: Option<Saved> = store_get(STORE_KEY).and_then(|s| serde_json::from_str(&s).ok());
        let token = token_from_fragment().or_else(|| saved.as_ref().and_then(|s| s.s.token.clone())).or_else(|| store_get(TOKEN_KEY));
        tl.token.set(token);
        if let Some(s) = saved {
            let id = s.s.task_id.clone();
            tl.started.set(Some(s));
            tl.watch(net, id);
        }
        // server clock offset, from every state read
        Effect::new(move || {
            if let Some(ms) = net.state.data.with(|s| s.as_ref().map(|s| s.read_at_ms)) {
                tl.skew.set(ms as f64 - js_sys::Date::now());
            }
        });
        // a test started elsewhere ended: read everything again and say how it ended
        let seen = StoredValue::new(None::<NetTrial>);
        Effect::new(move || {
            let cur = net.state.data.with(|s| s.as_ref().and_then(|s| s.trial.clone()));
            let prev = seen.get_value();
            seen.set_value(cur.clone());
            if let (Some(p), None) = (prev, cur)
                && !tl.following.get_untracked()
            {
                tl.ended_elsewhere(net, &p);
            }
        });
        let tick = StoredValue::new(0u32);
        if let Ok(h) = set_interval_with_handle(
            move || {
                let active = tl.active(net);
                if !active {
                    return;
                }
                tl.now.set(js_sys::Date::now());
                let n = tick.get_value().wrapping_add(1);
                tick.set_value(n);
                if n.is_multiple_of(2) {
                    tl.poll(net);
                }
            },
            Duration::from_secs(1),
        ) {
            on_cleanup(move || h.clear());
        }
    }

    /// A timed test with a deadline is running (an adoption waiting for the reboot is not).
    fn active(self, net: Net) -> bool {
        self.started.with_untracked(Option::is_some) || net.state.data.with_untracked(|s| s.as_ref().and_then(|s| s.trial.as_ref()).is_some_and(|t| t.deadline_ms > 0))
    }

    fn poll(self, net: Net) {
        spawn_local(async move {
            match api::get::<NetworkState>("/api/v1/network/state").await {
                Ok(s) => {
                    self.lost.set(false);
                    net.state.data.set(Some(s));
                }
                Err(ApiErr::Network | ApiErr::Timeout) => self.lost.set(true),
                Err(_) => {}
            }
        });
    }

    /// Apply (or restore, adopt) answered 202: show the countdown and follow the task.
    pub fn begin(self, net: Net, s: NetApplyStarted) {
        let saved = Saved { s, origin: origin() };
        store_set(STORE_KEY, serde_json::to_string(&saved).ok().as_deref());
        store_set(TOKEN_KEY, None);
        self.token.set(saved.s.token.clone());
        self.outcome.set(None);
        self.error.set(None);
        self.lost.set(false);
        self.now.set(js_sys::Date::now());
        let id = saved.s.task_id.clone();
        self.started.set(Some(saved));
        self.watch(net, id);
        focus_confirm();
    }

    /// Follow the task to its end: it ends well when the change was confirmed (or applied
    /// without a test), with an error when it was rolled back.
    fn watch(self, net: Net, id: String) {
        self.following.set(true);
        spawn_local(async move {
            let r = crate::tasks::follow(&id).await;
            self.following.set(false);
            let skipped = self.started.with_untracked(|s| s.as_ref().is_some_and(|s| s.s.token.is_none()));
            match r {
                // after Confirm in this tab its own message (with the revision) stays
                Ok(task) if task.status == TaskStatus::Ok => {
                    if self.outcome.with_untracked(Option::is_none) {
                        let key = if skipped { "net.applied" } else { "net.kept" };
                        self.outcome.set(Some((Tone::Ok, t(key).to_string())));
                    }
                }
                Ok(task) => self.outcome.set(Some((Tone::Warn, format!("{} {}", t("net.not_kept"), crate::tasks::task_error(&task))))),
                // the session ended or the task is gone: the server's trial marker decides
                Err(_) => {
                    if net.state.data.with_untracked(|s| s.as_ref().is_some_and(|s| s.trial.is_some())) {
                        return;
                    }
                }
            }
            self.clear();
            net.reload();
        });
    }

    fn ended_elsewhere(self, net: Net, p: &NetTrial) {
        net.reload();
        if self.outcome.with_untracked(Option::is_some) {
            self.clear();
            return;
        }
        let rolled = net.state.data.with_untracked(|s| s.as_ref().and_then(|s| s.last_rollback.as_ref()).is_some_and(|r| r.ts_ms >= p.started_ms));
        // a migration planned for the next boot only disappears when it is cancelled
        let msg = if p.kind == TrialKind::AdoptAtBoot && p.deadline_ms == 0 {
            (Tone::Info, t("net.boot_cancelled").to_string())
        } else if rolled { (Tone::Warn, t("net.not_kept").to_string()) } else { (Tone::Ok, t("net.kept").to_string()) };
        self.clear();
        self.outcome.set(Some(msg));
    }

    fn clear(self) {
        store_set(STORE_KEY, None);
        store_set(TOKEN_KEY, None);
        self.started.set(None);
        self.token.set(None);
        self.lost.set(false);
        self.error.set(None);
    }

    fn confirm(self, net: Net) {
        let Some(token) = self.token.get_untracked() else { return };
        self.busy.set(true);
        self.error.set(None);
        spawn_local(async move {
            let r = api::post::<_, NetConfirmed>("/api/v1/network/confirm", Some(&NetConfirmRequest { token })).await;
            self.busy.set(false);
            match r {
                Ok(c) => {
                    self.outcome.set(Some((Tone::Ok, tf("net.confirmed", &[("rev", &c.revision.to_string())]))));
                    if !self.following.get_untracked() {
                        self.clear();
                    }
                    net.reload();
                }
                Err(e) if e.status() == 409 && e.code() == rynas_api_types::error_codes::CONFLICT => {
                    self.error.set(Some(t("net.confirm_conflict").to_string()))
                }
                Err(e) if e.code() == rynas_api_types::error_codes::INVALID_VALUE => self.error.set(Some(t("net.confirm_token").to_string())),
                Err(e) => self.error.set(Some(e.text())),
            }
        });
    }

    /// Keeps a migration done at boot now: no browser applied it, so there is no token and any
    /// administrator may confirm it (over an address of the NAS, like every confirmation).
    fn keep_boot(self, net: Net) {
        self.busy.set(true);
        self.error.set(None);
        spawn_local(async move {
            let r = api::post::<_, NetConfirmed>("/api/v1/network/confirm", Some(&NetConfirmRequest { token: String::new() })).await;
            self.busy.set(false);
            match r {
                Ok(_) => net.reload(),
                Err(e) if e.status() == 409 && e.code() == rynas_api_types::error_codes::CONFLICT => {
                    self.error.set(Some(t("net.confirm_conflict").to_string()))
                }
                Err(e) => self.error.set(Some(e.text())),
            }
        });
    }

    fn rollback(self, net: Net) {
        self.busy.set(true);
        self.error.set(None);
        spawn_local(async move {
            let r = api::post::<(), TaskStarted>("/api/v1/network/rollback", None).await;
            self.busy.set(false);
            match r {
                Ok(_) => net.state.reload(),
                Err(e) => self.error.set(Some(e.text())),
            }
        });
    }
}

/// `m:ss`.
fn clock(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

// ------------------------------------------------------------------ banner

pub fn banner(net: Net) -> AnyView {
    let tl = net.trial;
    let trial = Memo::new(move |_| net.state.data.with(|s| s.as_ref().and_then(|s| s.trial.clone())));
    let shown = move || tl.started.with(Option::is_some) || trial.with(Option::is_some);
    let at_boot = move || trial.with(|t| t.as_ref().is_some_and(|t| t.kind == TrialKind::AdoptAtBoot && t.deadline_ms == 0));
    // the planned migration ran at this boot and waits for its check
    let booted = move || trial.with(|t| t.as_ref().is_some_and(|t| t.kind == TrialKind::AdoptAtBoot && t.deadline_ms > 0));
    // deadline and length: the server's marker once it exists, Apply's answer before
    let deadline = move || trial.with(|t| t.as_ref().map(|t| t.deadline_ms)).filter(|d| *d > 0).or_else(|| tl.started.with(|s| s.as_ref().map(|s| s.s.deadline_ms))).unwrap_or(0);
    let timeout = move || trial.with(|t| t.as_ref().map(|t| t.timeout_s)).filter(|d| *d > 0).or_else(|| tl.started.with(|s| s.as_ref().map(|s| s.s.timeout_s))).unwrap_or(120);
    let left = Memo::new(move |_| {
        let d = deadline() as f64;
        ((d - (tl.now.get() + tl.skew.get())) / 1000.0).max(0.0).ceil() as u64
    });
    let pct = Signal::derive(move || left.get() as f64 * 100.0 / timeout().max(1) as f64);
    let tone = Signal::derive(move || if left.get() <= 30 { Tone::Err } else { Tone::Warn });
    // screen readers: every 30 s, not every second
    let spoken = Memo::new(move |_| left.get().div_ceil(30) * 30);
    let lkg = move || trial.with(|t| t.as_ref().map(|t| t.lkg_revision)).or_else(|| net.state.data.with(|s| s.as_ref().and_then(|s| s.lkg_revision)));
    let has_token = move || tl.token.with(Option::is_some);
    let mine = move || tl.started.with(Option::is_some);
    let skip = move || tl.started.with(|s| s.as_ref().is_some_and(|s| s.s.token.is_none()));
    let pending_marker = move || mine() && trial.with(Option::is_none) && !skip();

    let title = move || {
        if skip() {
            t("net.trial_applying").to_string()
        } else if left.get() == 0 {
            t("net.trial_deadline").to_string()
        } else {
            t("net.trial_title").to_string()
        }
    };
    let text = move || {
        if skip() {
            return t("net.trial_applying_text").to_string();
        }
        let rev = lkg().map(|r| format!("#{r}")).unwrap_or_else(|| t("net.previous").to_string());
        if left.get() == 0 {
            tf("net.trial_deadline_text", &[("rev", &rev)])
        } else {
            tf("net.trial_text", &[("n", &left.get().to_string()), ("rev", &rev)])
        }
    };
    let who = move || {
        trial.with(|t| t.as_ref().map(|t| (t.author.clone(), t.interfaces.join(", ")))).map(|(a, i)| tf("net.trial_who", &[("user", &a), ("ifaces", &i)]))
    };

    let lost = move || {
        tl.lost.get().then(|| {
            let back = tl.started.with(|s| s.as_ref().map(|s| s.origin.clone())).unwrap_or_else(origin);
            view! {
                {ui::note(Tone::Err, icons::UNPLUG, view! { <strong>{tr("net.lost_title")}</strong>" "
                        {move || if left.get() == 0 { tf("net.lost_after", &[("url", &back)]) } else { t("net.lost_text").to_string() }} }.into_any())}
            }
        })
    };

    let candidates = move || {
        let s = tl.started.get()?;
        let token = s.s.token.clone()?;
        if !s.s.management_touched || s.s.candidate_urls.is_empty() {
            return None;
        }
        let items = s
            .s
            .candidate_urls
            .iter()
            .map(|u| {
                let href = format!("{u}network#confirm={token}");
                view! {
                    <li class="net-cand">
                        <a class="mono" href=href target="_blank" rel="noopener">{u.clone()}<Icon icon=icons::EXTERNAL_LINK size=12 /></a>
                    </li>
                }
            })
            .collect_view();
        Some(view! {
            <div class="net-cands">
                <p class="small"><strong>{tr("net.cands_title")}</strong>" "<span class="muted">{tr("net.cands_text")}</span></p>
                <ul class="plain-list">{items}</ul>
            </div>
        })
    };

    // who planned the migration, and when (it may be days before the reboot)
    let planned_by = move || {
        trial.with(|t| t.as_ref().map(|t| (t.author.clone(), t.started_ms))).map(|(a, ms)| tf("net.boot_who", &[("user", &a), ("when", &crate::i18n::datetime(ms))]))
    };

    let booted_banner = move || {
        let (ifaces, many) = trial.with(|t| t.as_ref().map(|t| (t.interfaces.join(", "), t.interfaces.len() > 1))).unwrap_or_default();
        // the seconds are in the clock next to the text (Italian: plural agreement)
        let text = tf(if many { "net.booted_text_n" } else { "net.booted_text" }, &[("ifaces", &ifaces)]);
        view! {
            <section class="card net-trial calm" aria-labelledby="net-trial-h">
                <div class="net-trial-head">
                    <span class="net-trial-icon"><Icon icon=icons::TIMER size=22 /></span>
                    <div class="net-trial-text">
                        <h2 id="net-trial-h">{tr("net.booted_title")}</h2>
                        <p>{text}</p>
                        <p class="muted small">{move || trial.with(|t| t.as_ref().map(|t| tf("net.booted_planned", &[("user", &t.author)])))}</p>
                    </div>
                    <div class="net-trial-count tnum" aria-hidden="true">{move || clock(left.get())}</div>
                    <span class="sr-only" aria-live="polite">{move || tf("net.booted_left_sr", &[("n", &spoken.get().to_string())])}</span>
                </div>
                {crate::form::form_error(tl.error)}
                <div class="net-trial-actions">
                    <button class="btn primary lg" disabled=move || tl.busy.get() on:click=move |_| tl.keep_boot(net)>
                        {crate::form::spinner(tl.busy)}<Icon icon=icons::CHECK size=18 />{tr("net.booted_keep")}
                    </button>
                    <button class="btn ghost lg" disabled=move || tl.busy.get() on:click=move |_| tl.rollback(net)>
                        <Icon icon=icons::ROTATE_CCW size=18 />{tr("net.booted_back")}
                    </button>
                </div>
            </section>
        }
    };

    let boot_banner = move || {
        let ifaces = trial.with(|t| t.as_ref().map(|t| t.interfaces.join(", "))).unwrap_or_default();
        view! {
            <section class="card net-trial calm" aria-labelledby="net-trial-h">
                <div class="net-trial-head">
                    <span class="net-trial-icon"><Icon icon=icons::TIMER size=22 /></span>
                    <div class="net-trial-text">
                        <h2 id="net-trial-h">{tr("net.boot_title")}</h2>
                        <p>{tf("net.boot_text", &[("ifaces", &ifaces)])}</p>
                        <p class="muted small">{planned_by}</p>
                    </div>
                </div>
                {crate::form::form_error(tl.error)}
                <div class="card-actions">
                    <button class="btn ghost" disabled=move || tl.busy.get() on:click=move |_| tl.rollback(net)>
                        {crate::form::spinner(tl.busy)}<Icon icon=icons::ROTATE_CCW size=16 />{tr("net.boot_cancel")}
                    </button>
                </div>
            </section>
        }
    };

    let main_banner = move || {
        view! {
            <section class="card net-trial" tabindex="-1" aria-labelledby="net-trial-h">
                <div class="net-trial-head">
                    <span class="net-trial-icon"><Icon icon=icons::TIMER size=22 /></span>
                    <div class="net-trial-text">
                        <h2 id="net-trial-h">{title}</h2>
                        <p>{text}</p>
                        <p class="muted small">{who}</p>
                    </div>
                    <div class="net-trial-count tnum" aria-hidden="true" hidden=skip>{move || clock(left.get())}</div>
                    <span class="sr-only" aria-live="polite">{move || (!skip()).then(|| tf("net.left_sr", &[("n", &spoken.get().to_string())]))}</span>
                </div>
                <div hidden=skip><Meter pct=pct tone=tone /></div>
                {lost}
                {move || pending_marker().then(|| view! { <p class="sec-note"><span class="btn-spinner"></span>{tr("net.trial_starting")}</p> })}
                {crate::form::form_error(tl.error)}
                <div class="net-trial-actions" hidden=skip>
                    {move || if has_token() {
                        view! {
                            <button class="btn primary lg" disabled=move || tl.busy.get() || trial.with(Option::is_none) on:click=move |_| tl.confirm(net)>
                                {crate::form::spinner(tl.busy)}<Icon icon=icons::CHECK size=18 />{tr("net.confirm")}
                            </button>
                        }.into_any()
                    } else {
                        view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("net.no_token")}</p> }.into_any()
                    }}
                    <button class="btn ghost lg" disabled=move || tl.busy.get() || trial.with(Option::is_none) on:click=move |_| tl.rollback(net)>
                        <Icon icon=icons::ROTATE_CCW size=18 />{tr("net.rollback_now")}
                    </button>
                </div>
                {candidates}
            </section>
        }
    };

    (move || {
        if !shown() {
            return ().into_any();
        }
        if at_boot() {
            boot_banner().into_any()
        } else if booted() {
            booted_banner().into_any()
        } else {
            main_banner().into_any()
        }
    })
    .into_any()
}

/// Move the focus to Confirm when the countdown appears (keyboard users).
fn focus_confirm() {
    request_animation_frame(|| {
        if let Ok(Some(el)) = document().query_selector(".net-trial") {
            let _ = el.unchecked_into::<web_sys::HtmlElement>().focus();
        }
    });
}

