//! Manual restart and shut down of the NAS (`/api/v1/system/power`).
//!
//! - [`ask`] opens the one confirmation (user menu, "Restart now" of the updates page): the
//!   action, the delay (now, 1 or 5 minutes), what it would interrupt (the server's warnings),
//!   a danger button. Warnings never block: the administrator reads them and decides.
//! - An immediate action shows [`PowerHost`]'s full-page overlay: it waits for the server to
//!   go away (following the task meanwhile: a failure closes the overlay with its error), then
//!   for it to come back (restart) and reloads to the sign-in page; a shut down ends on "The
//!   NAS is off".
//! - A delayed action shows in the dock ([`PendingPower`]) with its time and Cancel, for every
//!   administrator who opens the interface meanwhile, also when it was set over SSH.

use std::cell::Cell;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::power::{PendingSystemPower, PowerWarning, SystemPowerAction, SystemPowerRequest, SystemPowerState};
use rynas_api_types::{TaskInfo, TaskStatus};
use wasm_bindgen_futures::JsFuture;

use crate::api;
use crate::i18n::{self, t, tf, tr, try_t};
use crate::icons::{self, Icon};
use crate::ui::{self, Confirm, Tone};

const URL: &str = "/api/v1/system/power";

/// Where the overlay is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting for the server to go away.
    Going,
    /// Gone; waiting for it to come back (restart only).
    Coming,
    /// Shut down: the server is gone for good.
    Off,
}

#[derive(Clone, Copy)]
struct Power {
    ask: RwSignal<Option<SystemPowerAction>>,
    pending: RwSignal<Option<PendingSystemPower>>,
    overlay: RwSignal<Option<(SystemPowerAction, Phase)>>,
    loaded: RwSignal<bool>,
}

thread_local! {
    static POWER: Cell<Option<Power>> = const { Cell::new(None) };
}

fn power() -> Power {
    POWER.with(|c| match c.get() {
        Some(p) => p,
        None => {
            let p = Power { ask: RwSignal::new(None), pending: RwSignal::new(None), overlay: RwSignal::new(None), loaded: RwSignal::new(false) };
            c.set(Some(p));
            p
        }
    })
}

/// Opens the confirmation of `action`.
pub fn ask(action: SystemPowerAction) {
    power().ask.set(Some(action));
}

fn refresh_pending() {
    let p = power();
    spawn_local(async move {
        // only the scheduled action: the warnings are expensive and belong to the dialog
        if let Ok(s) = api::get::<SystemPowerState>(&format!("{URL}?pending_only=true")).await {
            p.pending.set(s.pending);
        }
        p.loaded.set(true);
    });
}


/// A warning in the current language; the server's English text for a code this build lacks.
fn warning_text(w: &PowerWarning) -> AnyView {
    let code = serde_json::to_value(w.code).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
    let Some(tpl) = try_t(&format!("sysp.w.{code}")) else {
        return view! { <span lang="en">{w.text.clone()}</span> }.into_any();
    };
    let subject = match w.code {
        rynas_api_types::power::PowerWarningCode::TaskRunning | rynas_api_types::power::PowerWarningCode::ScrubRunning => {
            w.subject.as_deref().map(crate::tasks::task_label).unwrap_or_default()
        }
        _ => w.subject.clone().unwrap_or_default(),
    };
    let op = w.op.as_deref().and_then(|o| try_t(&format!("sysp.op.{o}"))).unwrap_or_default();
    let pct = w.pct.map(|p| format!(" {}", i18n::pct(p as f64))).unwrap_or_default();
    let n = w.n.map(|n| n.to_string()).unwrap_or_default();
    let s = tpl.replace("{subject}", &subject).replace("{op}", op).replace("{pct}", &pct).replace("{n}", &n);
    view! { <span>{s}</span> }.into_any()
}

/// `fetch` of the session endpoint: any HTTP answer means the server is up.
async fn server_up() -> bool {
    let init = web_sys::RequestInit::new();
    init.set_method("GET");
    init.set_signal(Some(&web_sys::AbortSignal::timeout_with_u32(4_000)));
    let Some(w) = web_sys::window() else { return false };
    JsFuture::from(w.fetch_with_str_and_init("/api/v1/auth/session", &init)).await.is_ok()
}

/// Follows an immediate action: the task while the server answers, then the server itself.
fn watch(action: SystemPowerAction, task: String) {
    let p = power();
    p.overlay.set(Some((action, Phase::Going)));
    spawn_local(async move {
        let url = format!("/api/v1/tasks/{}", api::encode(&task));
        let mut misses = 0;
        // the server goes away: the event stream with it, so the task is read directly
        loop {
            api::sleep(1_500).await;
            match api::get::<TaskInfo>(&url).await {
                Ok(t) if matches!(t.status, TaskStatus::Error | TaskStatus::Cancelled) => {
                    p.overlay.set(None);
                    let why = t.error.unwrap_or_default();
                    crate::toast::toast(Tone::Err, format!("{} {why}", t_failed(action)).trim_end().to_string());
                    return;
                }
                Ok(_) => misses = 0,
                // no answer, 401 (sessions end with the server), 502/503 (rynasd stopped): going down
                Err(_) => misses += 1,
            }
            if misses >= 2 {
                break;
            }
        }
        if action == SystemPowerAction::Shutdown {
            // make sure it stays gone (not a restart of the web service)
            for _ in 0..3 {
                api::sleep(2_000).await;
                if server_up().await {
                    p.overlay.set(Some((action, Phase::Coming)));
                    reload_when_up().await;
                    return;
                }
            }
            p.overlay.set(Some((action, Phase::Off)));
            return;
        }
        p.overlay.set(Some((action, Phase::Coming)));
        reload_when_up().await;
    });
}

async fn reload_when_up() {
    loop {
        api::sleep(3_000).await;
        if server_up().await {
            if let Some(w) = web_sys::window() {
                let _ = w.location().assign("/");
            }
            return;
        }
    }
}

fn t_failed(a: SystemPowerAction) -> &'static str {
    match a {
        SystemPowerAction::Restart => t("sysp.restart_failed"),
        SystemPowerAction::Shutdown => t("sysp.shutdown_failed"),
    }
}

/// The confirmation dialog and the overlay; mounted once by the shell.
#[component]
pub fn PowerHost() -> impl IntoView {
    let p = power();
    if !p.loaded.get_untracked() {
        refresh_pending();
    }
    view! {
        {move || p.ask.get().map(dialog)}
        {move || p.overlay.get().map(|(a, ph)| overlay(a, ph))}
    }
}

fn dialog(action: SystemPowerAction) -> AnyView {
    let p = power();
    let delay = RwSignal::new("0".to_string());
    let state = RwSignal::new(None::<Result<SystemPowerState, String>>);
    spawn_local(async move {
        let r = api::get::<SystemPowerState>(URL).await.map_err(|e| e.text());
        if let Ok(s) = &r {
            p.pending.set(s.pending.clone());
        }
        state.set(Some(r));
    });
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let warnings = move || match state.get() {
        None => view! { <p class="muted small">{tr("sysp.checking")}</p> }.into_any(),
        Some(Err(e)) => ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {t("sysp.check_failed")}" "<span class="muted small" lang="en">{e}</span> }.into_any()),
        Some(Ok(s)) if s.warnings.is_empty() => ui::note(Tone::Ok, icons::CIRCLE_CHECK, view! { {tr("sysp.no_warnings")} }.into_any()),
        Some(Ok(s)) => view! {
            <div class="sysp-warnings" role="group" aria-label=tr("sysp.warnings")>
                {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { <strong>{tr("sysp.warnings")}</strong> }.into_any())}
                <ul class="sysp-list">{s.warnings.iter().map(|w| view! { <li>{warning_text(w)}</li> }).collect_view()}</ul>
                <p class="muted small">{tr("sysp.anyway")}</p>
            </div>
        }
        .into_any(),
    };
    let pending_note = move || {
        p.pending.get().map(|q| {
            ui::note(Tone::Info, icons::CALENDAR_CLOCK, view! { {pending_text(&q)} }.into_any())
        })
    };
    let delays: &'static [(&'static str, &'static str)] = &[("0", "sysp.now"), ("1", "sysp.in_1"), ("5", "sysp.in_5")];
    let extra = view! {
        {crate::form::segmented("sysp.when", delays, delay.into(), Callback::new(move |v| delay.set(v)))}
        {pending_note}
        {warnings}
    }
    .into_any();
    let close = Callback::new(move |_| p.ask.set(None));
    let go = Callback::new(move |_| {
        let delay_min: u32 = delay.get_untracked().parse().unwrap_or(0);
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let body = serde_json::to_string(&SystemPowerRequest { action, delay_min }).unwrap_or_default();
            if delay_min == 0 {
                match api::start_task(URL, Some(body)).await {
                    Ok(id) => {
                        p.ask.set(None);
                        watch(action, id);
                    }
                    Err(e) => error.set(Some(e.text())),
                }
                busy.set(false);
                return;
            }
            match crate::tasks::run_task(URL, Some(body)).await {
                Ok(task) if task.status == TaskStatus::Ok => {
                    p.ask.set(None);
                    refresh_pending();
                    crate::toast::toast(Tone::Info, t(match action {
                        SystemPowerAction::Restart => "sysp.restart_scheduled",
                        SystemPowerAction::Shutdown => "sysp.shutdown_scheduled",
                    }).to_string());
                }
                Ok(task) => error.set(Some(task.error.unwrap_or_else(|| t_failed(action).to_string()))),
                Err(e) => error.set(Some(e.text())),
            }
            busy.set(false);
        });
    });
    let (title, text, btn) = match action {
        SystemPowerAction::Restart => (t("sysp.restart_title"), t("sysp.restart_text"), "sysp.restart"),
        SystemPowerAction::Shutdown => (t("sysp.shutdown_title"), t("sysp.shutdown_text"), "sysp.shutdown"),
    };
    ui::confirm(Confirm {
        title: title.to_string(),
        text: text.to_string(),
        action: btn,
        danger: true,
        extra: Some(extra),
        typed: Signal::stored(None),
        busy,
        error,
        on_confirm: go,
        on_cancel: close,
    })
}

fn pending_text(q: &PendingSystemPower) -> String {
    let at = i18n::short_when(q.at_ms);
    let key = match (q.action, q.by.is_some()) {
        (SystemPowerAction::Restart, true) => "sysp.pending_restart_by",
        (SystemPowerAction::Restart, false) => "sysp.pending_restart",
        (SystemPowerAction::Shutdown, true) => "sysp.pending_shutdown_by",
        (SystemPowerAction::Shutdown, false) => "sysp.pending_shutdown",
    };
    tf(key, &[("at", &at), ("user", q.by.as_deref().unwrap_or_default())])
}

fn overlay(action: SystemPowerAction, phase: Phase) -> AnyView {
    let (icon, title, text) = match (action, phase) {
        (_, Phase::Off) => (icons::POWER, "sysp.off_title", "sysp.off_text"),
        (SystemPowerAction::Restart, Phase::Going) => (icons::ROTATE_CCW, "sysp.restarting", "sysp.going_text"),
        (SystemPowerAction::Restart, Phase::Coming) | (SystemPowerAction::Shutdown, Phase::Coming) => (icons::ROTATE_CCW, "sysp.restarting", "sysp.coming_text"),
        (SystemPowerAction::Shutdown, Phase::Going) => (icons::POWER, "sysp.shutting_down", "sysp.going_off_text"),
    };
    let busy = phase != Phase::Off;
    let reload = move |_| {
        if let Some(w) = web_sys::window() {
            let _ = w.location().assign("/");
        }
    };
    view! {
        <div class="sysp-overlay" role="alertdialog" aria-modal="true" aria-labelledby="sysp-ov-title" aria-describedby="sysp-ov-text">
            <div class="sysp-card">
                <span class="sysp-icon"><Icon icon=icon size=22 /></span>
                <h2 id="sysp-ov-title">{tr(title)}</h2>
                {busy.then(|| view! { <span class="btn-spinner sysp-spinner" aria-hidden="true"></span> })}
                <p id="sysp-ov-text" class="muted" role="status" aria-live="polite">{tr(text)}</p>
                <button type="button" class="btn ghost sm" on:click=reload>{tr("sysp.reload")}</button>
            </div>
        </div>
    }
    .into_any()
}

/// The delayed action in the dock, with Cancel.
#[component]
pub fn PendingPower() -> impl IntoView {
    let p = power();
    let busy = RwSignal::new(false);
    let cancel = move |_| {
        busy.set(true);
        spawn_local(async move {
            match api::post_empty::<()>(&format!("{URL}/cancel"), None).await {
                Ok(()) => crate::toast::toast(Tone::Ok, t("sysp.cancelled").to_string()),
                Err(e) => crate::toast::toast(Tone::Err, e.text()),
            }
            busy.set(false);
            refresh_pending();
        });
    };
    // the moment passes: read again (the NAS is going down, or someone cancelled)
    Effect::new(move || {
        if let Some(q) = p.pending.get() {
            let wait = (q.at_ms as f64 - js_sys::Date::now()).clamp(5_000.0, 600_000.0) as u32;
            spawn_local(async move {
                api::sleep(wait).await;
                refresh_pending();
            });
        }
    });
    view! {
        {move || p.pending.get().map(|q| view! {
            <div class="pending sysp-pending" role="status">
                <Icon icon=icons::CALENDAR_CLOCK size=16 />
                <span class="pending-text">{pending_text(&q)}</span>
                <div class="pending-actions">
                    <button class="btn ghost sm" disabled=move || busy.get() on:click=cancel>
                        {crate::form::spinner(busy)}{tr("sysp.cancel")}
                    </button>
                </div>
            </div>
        })}
    }
}

/// "Restart now" next to a "restart required" note.
pub fn restart_button() -> AnyView {
    view! {
        <button type="button" class="btn ghost sm" on:click=move |_| ask(SystemPowerAction::Restart)>
            <Icon icon=icons::ROTATE_CCW size=16 />{tr("sysp.restart_now")}
        </button>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use rynas_api_types::power::PowerWarningCode as C;

    /// Every warning code of the server has a text (`sysp.w.<code>`), every RAID operation too.
    #[test]
    fn every_warning_has_a_text() {
        for c in [C::RaidBusy, C::ScrubRunning, C::TaskRunning, C::SchedulerTaskRunning, C::SmbOpenFiles, C::AptRunning, C::UpsOnBattery, C::BootTrial] {
            let code = serde_json::to_value(c).unwrap();
            let key = format!("sysp.w.{}", code.as_str().unwrap());
            assert!(crate::i18n::try_t(&key).is_some(), "{key}");
        }
        for op in ["resync", "recovery", "reshape", "check"] {
            assert!(crate::i18n::try_t(&format!("sysp.op.{op}")).is_some(), "{op}");
        }
    }
}

