//! Wake-on-LAN (`/wake`, `docs/research/wake-on-lan.md`): the devices this NAS can wake, their
//! status, Wake (a task followed in the row), Sleep through a paired PC agent, the device panel
//! and Add device.
//!
//! - [`Wol`] holds the overview, the wakes this page follows and the open panel; the PC backup
//!   page uses it too ([`pc`]) for the Wake button of a PC.
//! - The list polls `GET /api/v1/wol` every 5 s while the page is visible, and every second
//!   while a device is waking.

mod device;
pub mod pc;

use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::pcagent::SleepMode;
use rynas_api_types::wol::*;

use crate::api;
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::toast::toast;
use crate::ui::{self, Confirm, EmptyState, PageHeader, Remote, Tone};

pub(crate) const URL: &str = "/api/v1/wol";

pub(crate) fn dev_url(id: &str) -> String {
    format!("{URL}/devices/{}", api::encode(id))
}

fn now_ms() -> u64 {
    js_sys::Date::now() as u64
}

/// The demo data set is shown (no API behind the page).
fn demo() -> bool {
    #[cfg(feature = "demo")]
    {
        crate::auth::auth().phase.get_untracked() == crate::auth::Phase::Demo
    }
    #[cfg(not(feature = "demo"))]
    {
        false
    }
}

/// The signed-in user manages the devices (adds them); each device also says it
/// ([`WolDeviceView::can_manage`]).
fn is_admin() -> bool {
    demo() || crate::auth::auth().session.with(|s| s.as_ref().is_some_and(|s| s.user.role == rynas_api_types::Role::Admin))
}

/// A wake the page follows: its task, when it began and the task's last line.
#[derive(Clone, PartialEq)]
pub(crate) struct Run {
    upid: String,
    started_ms: u64,
    line: Option<String>,
    done: bool,
}

/// The state of the Wake-on-LAN views of a page.
#[derive(Clone, Copy)]
pub(crate) struct Wol {
    pub data: Remote<WolOverview>,
    /// The device whose panel is open.
    pub open: RwSignal<Option<String>>,
    /// Add device is open, with what it starts from.
    pub add: RwSignal<Option<WolDevice>>,
    /// The wakes followed, by device id.
    runs: RwSignal<HashMap<String, Run>>,
    /// The clock of the elapsed seconds and the relative times (every second while something
    /// wakes, else every 30 s).
    now: RwSignal<u64>,
    /// An administrator: task logs can be read (`/api/v1/tasks` is theirs only).
    admin: bool,
}

impl Wol {
    /// `fetch`: read the overview now (else at the first [`Remote::reload`]); `poll`: every
    /// 5 s while the page is visible (the Wake-on-LAN page; the PC backup page only follows
    /// running wakes).
    pub fn new(fetch: bool, poll: bool) -> Self {
        let w = Wol {
            data: if fetch { Remote::new(URL) } else { Remote::idle(URL) },
            open: RwSignal::new(None),
            add: RwSignal::new(None),
            runs: RwSignal::new(HashMap::new()),
            now: RwSignal::new(now_ms()),
            admin: is_admin(),
        };
        #[cfg(feature = "demo")]
        if demo() {
            w.data.data.set(Some(crate::demo::wol_overview()));
        }
        // wakes the server reports (started elsewhere, or before this page opened) are
        // followed like the page's own
        Effect::new(move |_| {
            let waking: Vec<(String, String, u64)> = w.data.data.with(|d| {
                d.iter().flat_map(|o| o.devices.iter()).filter_map(|v| v.waking.clone().map(|u| (v.device.id.clone(), u, v.waking_since_ms.unwrap_or_else(now_ms)))).collect()
            });
            for (id, upid, since) in waking {
                if w.runs.with_untracked(|r| r.get(&id).is_some_and(|r| r.upid == upid)) {
                    continue;
                }
                w.follow(id, upid, since);
            }
        });
        // a wake has ended once its result is in the history, or once the server no longer
        // reports it (also how users who cannot read task logs see the end)
        Effect::new(move |_| {
            let now = now_ms();
            let ended: Vec<String> = w.data.data.with(|d| {
                let Some(o) = d.as_ref() else { return Vec::new() };
                w.runs.with_untracked(|r| {
                    r.iter()
                        .filter(|(_, run)| !run.done && !run.upid.is_empty())
                        .filter(|(id, run)| {
                            o.devices.iter().find(|v| v.device.id == **id).is_none_or(|v| {
                                v.history.iter().any(|h| h.upid.as_deref() == Some(run.upid.as_str()))
                                    || (v.waking.as_deref() != Some(run.upid.as_str()) && now.saturating_sub(run.started_ms) > 10_000)
                            })
                        })
                        .map(|(id, _)| id.clone())
                        .collect()
                })
            });
            if !ended.is_empty() {
                w.runs.update(|r| {
                    for id in &ended {
                        if let Some(x) = r.get_mut(id) {
                            x.done = true;
                        }
                    }
                });
            }
        });
        if poll {
            crate::live::poll(std::time::Duration::from_secs(5), move || w.data.reload());
        }
        crate::live::poll(std::time::Duration::from_secs(30), move || w.now.set(now_ms()));
        crate::live::poll_while(1000, move || w.waking_any(), move || {
            w.now.set(now_ms());
            w.data.reload();
        });
        w
    }

    /// Something wakes (tracked).
    fn waking_any(self) -> bool {
        self.runs.with(|r| r.values().any(|r| !r.done)) || self.data.data.with(|d| d.as_ref().is_some_and(|o| o.devices.iter().any(|v| v.status == WolStatus::Waking)))
    }

    /// The device `id` as last read (tracked).
    fn view_of(self, id: &str) -> Option<WolDeviceView> {
        self.data.data.with(|d| d.as_ref().and_then(|o| o.devices.iter().find(|v| v.device.id == id).cloned()))
    }

    /// The device linked to the PC backup device `pc` (tracked).
    fn of_pc(self, pc: &str) -> Option<WolDeviceView> {
        self.data.data.with(|d| d.as_ref().and_then(|o| o.devices.iter().find(|v| v.device.pc_device.as_deref() == Some(pc)).cloned()))
    }

    fn run_of(self, id: &str) -> Option<Run> {
        self.runs.with(|r| r.get(id).cloned())
    }

    /// Follows the task `upid` of a wake of `id`. Administrators read its last line every
    /// second (retrying after an error) until it ends or the server stops reporting the wake;
    /// the result then comes from the device's history.
    fn follow(self, id: String, upid: String, started_ms: u64) {
        // already followed: no second loop (nor one for a page left while the wake was asked)
        if self.runs.try_with_untracked(|r| r.get(&id).is_some_and(|r| r.upid == upid && !r.done)) != Some(false) {
            return;
        }
        self.runs.update(|r| {
            r.insert(id.clone(), Run { upid: upid.clone(), started_ms, line: None, done: false });
        });
        if demo() || !self.admin || upid.is_empty() {
            return;
        }
        spawn_local(async move {
            let mut offset = 0u64;
            loop {
                // ended, replaced by another wake, dismissed, or the page is gone
                if self.runs.try_with_untracked(|r| r.get(&id).is_some_and(|r| r.upid == upid && !r.done)) != Some(true) {
                    break;
                }
                // nothing read while the tab is hidden
                if !crate::live::should_poll() {
                    api::sleep(1000).await;
                    continue;
                }
                if let Ok(c) = api::task_log(&upid, offset).await {
                    if let Some(l) = c.text.lines().rev().map(str::trim).find(|l| !l.is_empty()) {
                        let l = l.to_string();
                        let _ = self.runs.try_update(|r| {
                            if let Some(x) = r.get_mut(&id) {
                                x.line = Some(l);
                            }
                        });
                    }
                    offset = c.next;
                    if c.complete {
                        let _ = self.runs.try_update(|r| {
                            if let Some(x) = r.get_mut(&id).filter(|x| x.upid == upid) {
                                x.done = true;
                            }
                        });
                        break;
                    }
                }
                api::sleep(1000).await;
            }
            self.data.reload();
        });
    }

    /// Wake `id`.
    fn wake(self, id: String, name: String, busy: RwSignal<bool>) {
        if demo() {
            self.follow(id, String::new(), now_ms());
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let r = api::post::<(), WolWakeStarted>(&format!("{}/wake", dev_url(&id)), None).await;
            let _ = busy.try_set(false);
            match r {
                Ok(s) => {
                    self.follow(id, s.upid, now_ms());
                    self.data.reload();
                }
                Err(e) => toast(Tone::Err, format!("{}: {}", tf("wol.wake_failed", &[("name", &name)]), e.text())),
            }
        });
    }

    fn dismiss(self, id: &str) {
        self.runs.update(|r| {
            r.remove(id);
        });
    }
}

/// The icon of a kind of device.
pub(crate) fn kind_icon(k: WolKind) -> &'static str {
    match k {
        WolKind::Desktop => icons::MONITOR,
        WolKind::Laptop => icons::LAPTOP,
        WolKind::Server => icons::SERVER,
        WolKind::Media => icons::TV,
        WolKind::Other => icons::NETWORK,
    }
}

/// The status pill.
pub(crate) fn status_view(s: WolStatus) -> AnyView {
    let (tone, k) = match s {
        WolStatus::Online => (Tone::Ok, "wol.st.online"),
        WolStatus::Offline => (Tone::Unknown, "wol.st.offline"),
        WolStatus::Waking => (Tone::Info, "wol.st.waking"),
        WolStatus::Unknown => (Tone::Unknown, "wol.st.unknown"),
    };
    ui::status(tone, t(k).to_string())
}

/// "for 5 min" under the status, when the server knows since when (not while waking: the
/// button counts the seconds then). `now`: the page's clock, so the text moves.
fn since_text(v: &WolDeviceView, now: u64) -> Option<String> {
    (v.status != WolStatus::Waking).then_some(v.since_ms).flatten().map(|s| tf("wol.since", &[("time", &i18n::approx(now.saturating_sub(s) / 1000))]))
}

/// The power commands the agent of a device takes (none without one or with an old agent).
fn power_modes(v: &WolDeviceView) -> Vec<SleepMode> {
    v.agent.as_ref().filter(|a| a.power_commands).map(|a| a.sleep.iter().copied().filter(|m| *m != SleepMode::Unknown).collect()).unwrap_or_default()
}

pub(crate) fn warning_text(w: WolWarning) -> &'static str {
    match w {
        WolWarning::Wireless => "wol.warn.wireless",
        WolWarning::Unsupported => "wol.warn.unsupported",
        WolWarning::Disabled => "wol.warn.disabled",
        WolWarning::NoRoute => "wol.warn.no_route",
        WolWarning::RandomMac => "wol.warn.random_mac",
    }
}

fn sleep_key(m: SleepMode) -> Option<(&'static str, &'static str)> {
    match m {
        SleepMode::Suspend => Some(("wol.sleep.suspend", icons::MOON)),
        SleepMode::Hibernate => Some(("wol.sleep.hibernate", icons::MOON_STAR)),
        SleepMode::Poweroff => Some(("sysp.shutdown", icons::POWER)),
        SleepMode::Unknown => None,
    }
}

pub(crate) fn wake_button(w: Wol, id: String) -> AnyView {
    let busy = RwSignal::new(false);
    let id = StoredValue::new(id);
    let state = Memo::new(move |_| {
        let v = w.view_of(&id.get_value());
        let run = w.run_of(&id.get_value()).filter(|r| !r.done);
        (v.as_ref().map(|v| (v.status, v.device.name.clone(), v.waking.clone(), v.waking_since_ms)), run.map(|r| (r.upid, r.started_ms)))
    });
    (move || {
        let (dev, run) = state.get();
        let Some((status, name, waking, since)) = dev else { return ().into_any() };
        let running = run.clone().map(|r| r.0).or(waking).filter(|_| run.is_some() || status == WolStatus::Waking);
        match running {
            Some(upid) => {
                // the start the server knows, else the click
                let started = since.or(run.map(|r| r.1)).unwrap_or_else(now_ms);
                let seconds = move || tf("wol.waking_s", &[("s", &(w.now.get().saturating_sub(started) / 1000).to_string())]);
                if w.admin && !upid.is_empty() {
                    // an administrator opens the task's log
                    view! {
                        <button type="button" class="btn sm wol-wake" title=tr("wol.open_task") on:click=move |_| crate::tasks::tasks().show(&upid)>
                            <span class="btn-spinner" aria-hidden="true"></span>
                            <span class="tnum">{seconds}</span>
                        </button>
                    }
                    .into_any()
                } else {
                    view! {
                        <span class="btn sm wol-wake wol-wake-busy" role="status">
                            <span class="btn-spinner" aria-hidden="true"></span>
                            <span class="tnum">{seconds}</span>
                        </span>
                    }
                    .into_any()
                }
            }
            None => {
                let on = status == WolStatus::Online;
                let label = ui::verb_name("wol.wake", &name);
                let n = name.clone();
                view! {
                    <button type="button" class="btn primary sm wol-wake" aria-label=label disabled=move || on || busy.get()
                        title=move || if on { t("wol.already_on") } else { "" }
                        on:click=move |_| w.wake(id.get_value(), n.clone(), busy)>
                        {crate::form::spinner_icon(busy.into(), icons::POWER)}{tr("wol.wake")}
                    </button>
                }
                .into_any()
            }
        }
    })
    .into_any()
}

pub(crate) fn sleep_menu(w: Wol, id: String) -> AnyView {
    let id = StoredValue::new(id);
    let open = RwSignal::new(false);
    let ask_off = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let state = Memo::new(move |_| {
        w.view_of(&id.get_value()).and_then(|v| {
            let modes = power_modes(&v);
            (!modes.is_empty()).then(|| (v.device.name.clone(), modes, v.status == WolStatus::Online, v.agent.as_ref().is_some_and(|a| a.busy)))
        })
    });
    let send = move |mode: SleepMode| {
        let Some((name, ..)) = state.get_untracked() else { return };
        open.set(false);
        if demo() {
            toast(Tone::Ok, tf("wol.sleep_sent", &[("name", &name)]));
            ask_off.set(false);
            return;
        }
        busy.set(true);
        error.set(None);
        let u = format!("{}/sleep", dev_url(&id.get_value()));
        spawn_local(async move {
            let r = api::post_empty(&u, Some(&WolSleepRequest { mode })).await;
            let _ = busy.try_set(false);
            match r {
                Ok(()) => {
                    toast(Tone::Ok, tf("wol.sleep_sent", &[("name", &name)]));
                    let _ = ask_off.try_set(false);
                    w.data.reload();
                }
                Err(e) if ask_off.try_get_untracked() == Some(true) => {
                    let _ = error.try_set(Some(e.text()));
                }
                Err(e) => toast(Tone::Err, e.text()),
            }
        });
    };
    let confirm_off = Callback::new(move |_| send(SleepMode::Poweroff));
    // the menu is placed on the page (fixed), not in the row: a table scrolls sideways and
    // would clip it; it closes when the page scrolls or resizes (it would float away)
    let pos = RwSignal::new(String::new());
    let listeners: StoredValue<Vec<WindowListenerHandle>, LocalStorage> = StoredValue::new_local(Vec::new());
    let drop_listeners = move || {
        if let Some(v) = listeners.try_update_value(std::mem::take) {
            for h in v {
                h.remove();
            }
        }
    };
    Effect::new(move |_| {
        if open.get() {
            let a = window_event_listener(leptos::ev::scroll, move |_| {
                let _ = open.try_set(false);
            });
            let b = window_event_listener(leptos::ev::resize, move |_| {
                let _ = open.try_set(false);
            });
            listeners.update_value(|v| v.extend([a, b]));
        } else {
            drop_listeners();
        }
    });
    on_cleanup(drop_listeners);
    let place = move |e: &web_sys::MouseEvent| {
        use wasm_bindgen::JsCast;
        let Some(el) = e.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) else { return };
        let r = el.get_bounding_client_rect();
        let win = window();
        let px = |v: Result<wasm_bindgen::JsValue, wasm_bindgen::JsValue>| v.ok().and_then(|v| v.as_f64()).unwrap_or(0.0);
        let (vw, vh) = (px(win.inner_width()), px(win.inner_height()));
        let right = (vw - r.right()).max(8.0);
        let below = vh - r.bottom();
        // upward when there is little room below and more above
        pos.set(if below < 200.0 && r.top() > below {
            format!("position:fixed;top:auto;bottom:{}px;right:{right}px", vh - r.top() + 8.0)
        } else {
            format!("position:fixed;top:{}px;right:{right}px", r.bottom() + 8.0)
        });
    };
    let menu = move || {
        let (name, modes, on, backing_up) = state.get()?;
        let why = if backing_up { Some(t("wol.sleep_busy")) } else if !on { Some(t("wol.sleep_off")) } else { None };
        let label = ui::verb_name("wol.power", &name);
        let items = move || {
            modes
                .iter()
                .filter_map(|m| {
                    let m = *m;
                    let (k, icon) = sleep_key(m)?;
                    Some(view! {
                        <button type="button" class="menu-item" role="menuitem" on:click=move |_| {
                            if m == SleepMode::Poweroff {
                                open.set(false);
                                error.set(None);
                                ask_off.set(true);
                            } else {
                                send(m);
                            }
                        }><Icon icon=icon size=16 />{tr(k)}</button>
                    })
                })
                .collect_view()
        };
        Some(view! {
            <div class="pop-anchor" on:keydown=move |e| if e.key() == "Escape" && open.get_untracked() { e.stop_propagation(); open.set(false); }
                on:focusout=move |e: web_sys::FocusEvent| {
                    use wasm_bindgen::JsCast;
                    let inside = e.related_target().and_then(|t| t.dyn_into::<web_sys::Node>().ok()).zip(e.current_target().and_then(|c| c.dyn_into::<web_sys::Node>().ok())).is_some_and(|(t, c)| c.contains(Some(&t)));
                    if !inside {
                        open.set(false);
                    }
                }>
                <button type="button" class="btn ghost sm" aria-label=label aria-haspopup="menu" aria-expanded=move || open.get().to_string()
                    disabled=move || why.is_some() || busy.get() title=why.unwrap_or("")
                    on:click=move |e| {
                        if !open.get_untracked() {
                            place(&e);
                        }
                        open.update(|o| *o = !*o)
                    }>
                    {crate::form::spinner_icon(busy.into(), icons::MOON)}{tr("wol.power")}<Icon icon=icons::CHEVRON_DOWN size=14 />
                </button>
                <Show when=move || open.get()>
                    <div class="popover menu wol-menu" role="menu" style=move || pos.get()>{items()}</div>
                </Show>
            </div>
        })
    };
    // the confirmation lives outside the menu: a poll that rebuilds the menu keeps it open
    view! {
        {menu}
        {move || ask_off.get().then(|| {
            let name = state.with_untracked(|s| s.as_ref().map(|s| s.0.clone()).unwrap_or_default());
            ui::confirm(Confirm { busy, error, ..Confirm::new(tf("wol.off_title", &[("name", &name)]), t("wol.off_text").to_string(), "sysp.shutdown", true, confirm_off, Callback::new(move |_| ask_off.set(false))) })
        })}
    }
    .into_any()
}

pub(crate) fn wake_line(w: Wol, id: String, open: Option<Callback<()>>) -> AnyView {
    let id = StoredValue::new(id);
    let state = Memo::new(move |_| {
        let run = w.run_of(&id.get_value())?;
        let v = w.view_of(&id.get_value())?;
        let end = v.history.iter().find(|h| !run.upid.is_empty() && h.upid.as_deref() == Some(run.upid.as_str())).map(|h| (h.end, h.secs, v.device.wake_timeout_s));
        Some((run, end))
    });
    (move || {
        let (run, end) = state.get()?;
        let close = move |_| w.dismiss(&id.get_value());
        let x = view! { <button type="button" class="icon-btn sm" aria-label=tr("close") on:click=close><Icon icon=icons::X size=14 /></button> };
        let line = run.line.clone().map(|l| view! { <span lang="en" class="wol-task-line">{l}</span> });
        match end {
            // running: the task's last line (the seconds are on the button)
            None if !run.done => line.map(|l| view! { <p class="wol-progress small" role="status">{l}</p> }.into_any()),
            // the task ended, its result not read yet
            None => Some(view! { <p class="wol-progress small">{line}{x}</p> }.into_any()),
            Some((WolWakeEnd::Online, secs, _)) => Some(ui::note_class(Tone::Ok, icons::CIRCLE_CHECK, "wol-outcome", view! {
                <span>{tf("wol.end.online", &[("time", &i18n::elapsed(u64::from(secs.unwrap_or(0))))])}</span>{x}
            }.into_any())),
            Some((WolWakeEnd::AlreadyOn, ..)) => Some(ui::note_class(Tone::Info, icons::INFO, "wol-outcome", view! { <span>{tr("wol.end.already_on")}</span>{x} }.into_any())),
            Some((WolWakeEnd::Unconfirmed, ..)) => Some(ui::note_class(Tone::Info, icons::SEND, "wol-outcome", view! { <span>{tr("wol.end.unconfirmed")}</span>{x} }.into_any())),
            Some((WolWakeEnd::SendFailed, ..)) => Some(ui::note_class(Tone::Err, icons::CIRCLE_X, "wol-outcome", view! {
                <span class="cell-main"><span>{tr("wol.end.send_failed")}</span>{line}</span>{x}
            }.into_any())),
            Some((WolWakeEnd::Timeout, _, timeout)) => Some(ui::note_class(Tone::Warn, icons::TRIANGLE_ALERT, "wol-outcome", view! {
                <span class="cell-main">
                    <strong>{tf("wol.end.timeout", &[("time", &i18n::approx(u64::from(timeout)))])}</strong>
                    <span>{tr("wol.causes")}</span>
                    <ul class="wol-causes">
                        <li>{tr("wol.cause.wifi")}</li>
                        <li>{tr("wol.cause.off")}</li>
                        <li>{tr("wol.cause.power")}</li>
                        <li>{tr("wol.cause.subnet")}</li>
                    </ul>
                    {open.map(|o| view! { <button type="button" class="link-btn wol-open" on:click=move |_| o.run(())>{tr("wol.open_panel")}</button> })}
                </span>
                {x}
            }.into_any())),
        }
    })
    .into_any()
}

/// One device of the list: two rows, the device and (while there is one) its wake.
fn device_row(w: Wol, id: String) -> AnyView {
    let sid = StoredValue::new(id.clone());
    let v = Memo::new(move |_| w.view_of(&sid.get_value()));
    let open = move || w.open.set(Some(sid.get_value()));
    let name_cell = move || {
        v.with(|v| {
            v.as_ref().map(|v| {
                let warn = (!v.warnings.is_empty()).then(|| {
                    let text = v.warnings.iter().map(|x| t(warning_text(*x))).collect::<Vec<_>>().join(" ");
                    view! { <span class="wol-warn" title=text.clone() role="img" aria-label=text><Icon icon=icons::TRIANGLE_ALERT size=14 /></span> }
                });
                let label = ui::verb_name("bk.open", &v.device.name);
                view! {
                    <div class="wol-name">
                        <span class="wol-kind" aria-hidden="true"><Icon icon=kind_icon(v.device.kind) size=18 /></span>
                        <div class="cell-main">
                            <button type="button" class="row-link" aria-label=label data-opener=format!("wol-{}", v.device.id) on:click=move |_| open()>{v.device.name.clone()}</button>
                            {(v.agent.is_some() || warn.is_some()).then(|| view! {
                                <div class="chips">
                                    {v.agent.is_some().then(|| ui::chip(Tone::Info, view! { {tr("wol.agent")} }.into_any()))}
                                    {warn}
                                </div>
                            })}
                        </div>
                    </div>
                }
            })
        })
    };
    let status_cell = move || {
        v.with(|v| {
            v.as_ref().map(|v| {
                view! {
                    <div class="cell-main">
                        {status_view(v.status)}
                        {since_text(v, w.now.get()).map(|s| view! { <span class="cell-sub small">{s}</span> })}
                    </div>
                }
            })
        })
    };
    let addr_cell = move || {
        v.with(|v| {
            v.as_ref().map(|v| {
                let ip = v.ip.clone().or_else(|| v.device.host.clone()).unwrap_or_else(|| ui::DASH.to_string());
                view! {
                    <div class="cell-main">
                        <span class="mono">{ip}</span>
                        <span class="cell-sub small mono">{v.device.macs.first().cloned().unwrap_or_default()}</span>
                    </div>
                }
            })
        })
    };
    // a line while the task says something, then the result
    let has_line = Memo::new(move |_| w.runs.with(|r| r.get(&sid.get_value()).is_some_and(|r| r.done || r.line.is_some())));
    view! {
        <tr>
            {ui::cell("wol.col.device", "wrap", (move || name_cell()).into_any())}
            {ui::cell("col.status", "wrap", (move || status_cell()).into_any())}
            {ui::cell("wol.col.address", "wrap", (move || addr_cell()).into_any())}
            {ui::actions(view! { <div class="wol-actions">{wake_button(w, id.clone())}{sleep_menu(w, id.clone())}</div> }.into_any())}
        </tr>
        <Show when=move || has_line.get()>
            <tr class="wol-line-row">
                <td colspan="4">{wake_line(w, sid.get_value(), Some(Callback::new(move |_| open())))}</td>
            </tr>
        </Show>
    }
    .into_any()
}

/// The page.
#[component]
pub fn WakePage() -> impl IntoView {
    let w = Wol::new(true, true);
    let admin = is_admin();
    // `?add=<PC>` (from a PC's backup settings, administrators) opens Add device for that PC,
    // `?device=<id>` the panel of a device; the query goes when they close
    let query = leptos_router::hooks::use_query_map();
    Effect::new(move |_| {
        let q = query.get();
        if let Some(pc) = q.get("add").filter(|p| admin && !p.is_empty()) {
            w.add.set(Some(WolDevice { name: pc.clone(), pc_device: Some(pc), ..WolDevice::default() }));
        }
        if let Some(d) = q.get("device").filter(|d| !d.is_empty()) {
            w.open.set(Some(d));
        }
    });
    let navigate = StoredValue::new_local(leptos_router::hooks::use_navigate());
    Effect::new(move |prev: Option<(bool, bool)>| {
        let now = (w.add.with(Option::is_some), w.open.with(Option::is_some));
        if let Some(p) = prev
            && ((p.0 && !now.0) || (p.1 && !now.1))
            && query.with_untracked(|q| q.get("add").is_some() || q.get("device").is_some())
        {
            navigate.with_value(|nav| nav("/wake", leptos_router::NavigateOptions { replace: true, ..Default::default() }));
        }
        now
    });
    let ids = Memo::new(move |_| w.data.data.with(|d| d.as_ref().map(|o| o.devices.iter().map(|v| v.device.id.clone()).collect::<Vec<_>>()).unwrap_or_default()));
    let empty = Memo::new(move |_| w.data.data.with(|d| d.as_ref().is_some_and(|o| o.devices.is_empty())));
    let add = move || w.add.set(Some(WolDevice::default()));
    let content = move || {
        if empty.get() {
            return view! {
                <EmptyState icon=icons::POWER title=Signal::derive(move || t("wol.empty").to_string()) text=Signal::derive(move || t(if admin { "wol.empty_text" } else { "wol.empty_user" }).to_string())>
                    {admin.then(|| view! { <div class="sec-actions"><button class="btn primary" on:click=move |_| add()><Icon icon=icons::PLUS size=16 />{tr("wol.add")}</button></div> })}
                </EmptyState>
            }
            .into_any();
        }
        let body = ui::keyed(ids.into(), Callback::new(move |id: String| device_row(w, id)));
        ui::table_body("wol-table", &["wol.col.device", "col.status", "wol.col.address", ""], body)
    };
    view! {
        <div class="page">
            <PageHeader icon=icons::POWER title="wol.title" subtitle="wol.subtitle">
                {move || (admin && !empty.get() && w.data.ready()).then(|| view! {
                    <button class="btn primary" on:click=move |_| add()><Icon icon=icons::PLUS size=16 /><span class="btn-label">{tr("wol.add")}</span></button>
                })}
            </PageHeader>
            <section class="card widget w-12">
                {move || w.data.view(4, content)}
            </section>
            {move || w.open.get().map(|id| device::drawer(w, id))}
            {move || w.add.get().map(|init| device::add_dialog(w, init))}
        </div>
    }
}
