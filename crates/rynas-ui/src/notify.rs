//! Notification center (bell in the app bar).
//!
//! Two sources:
//! - **server notifications** from rynasd's engine (`/api/v1/notifications`, D-NOTIFY): the
//!   latest unacknowledged ones and the counter, kept current by the `notification` event of
//!   the multiplexed stream ([`crate::live`]); acknowledging is a server action shared by every
//!   administrator;
//! - **local alerts** derived from data the UI already receives (`alerts::derive`: fan
//!   failures, temperatures over threshold, degraded arrays, full volumes). Their "read" state
//!   is kept per alert key in localStorage, and a key that disappears and comes back is unread
//!   again.

use std::cell::Cell;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::notify::{AckAllResult, Notification, NotificationChangeKind, NotificationEvent, NotifySeverity};

use crate::alerts::{self, Alert};
use crate::api;
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::live::Live;
use crate::prefs::{storage_get, storage_set, use_ui};
use crate::toast::toast;
use crate::ui::{EmptyState, Tone, cpu_offset};

const SEEN_KEY: &str = "rynas.notif.seen";
/// Unacknowledged server notifications kept for the bell (newest first).
const BELL_CAP: usize = 50;

/// Tone of a notification severity (state = icon + text + colour, rule G6).
pub fn severity_tone(s: NotifySeverity) -> Tone {
    match s {
        NotifySeverity::Info | NotifySeverity::Notice => Tone::Info,
        NotifySeverity::Warning => Tone::Warn,
        NotifySeverity::Error | NotifySeverity::Critical => Tone::Err,
    }
}

/// Tone of a notification in the bell: a resolved event (a task that succeeded again, a
/// problem gone) keeps the severity of the problem for delivery rules, but reads as good news.
pub fn notification_tone(n: &Notification) -> Tone {
    if n.state == rynas_api_types::notify::EventState::Resolved { Tone::Ok } else { severity_tone(n.severity) }
}

pub fn severity_key(s: NotifySeverity) -> &'static str {
    match s {
        NotifySeverity::Info => "nt.sev.info",
        NotifySeverity::Notice => "nt.sev.notice",
        NotifySeverity::Warning => "nt.sev.warning",
        NotifySeverity::Error => "nt.sev.error",
        NotifySeverity::Critical => "nt.sev.critical",
    }
}

/// Server notifications as the bell shows them.
#[derive(Clone, Copy)]
pub struct Server {
    /// Unacknowledged, newest first, at most [`BELL_CAP`].
    pub list: RwSignal<Vec<Notification>>,
    /// Unacknowledged in all (exact after the first event, at least `list.len()` before).
    pub unacked: RwSignal<u64>,
    pub err: RwSignal<Option<String>>,
    /// Bumped on every change: pages that show the whole list read it again.
    pub generation: RwSignal<u64>,
}

thread_local! {
    static SERVER: Cell<Option<Server>> = const { Cell::new(None) };
}

/// The store (also reachable from the stream callbacks).
pub fn server() -> Option<Server> {
    SERVER.with(Cell::get)
}

impl Server {
    fn new() -> Self {
        let s = Server { list: RwSignal::new(Vec::new()), unacked: RwSignal::new(0), err: RwSignal::new(None), generation: RwSignal::new(0) };
        SERVER.with(|c| c.set(Some(s)));
        s
    }

    /// Read the unacknowledged notifications again (sign-in, `resync`, Retry).
    pub fn reload(self) {
        spawn_local(async move {
            match api::get::<Vec<Notification>>(&format!("/api/v1/notifications?acked=false&limit={BELL_CAP}")).await {
                Ok(l) => {
                    self.unacked.set(l.len() as u64);
                    self.list.set(l);
                    self.err.set(None);
                    self.generation.update(|g| *g += 1);
                }
                Err(e) if e.status() == 401 => {}
                Err(e) => self.err.set(Some(e.code().to_string())),
            }
        });
    }

    /// Signed out: nothing of the previous session stays.
    pub fn clear(self) {
        self.list.set(Vec::new());
        self.unacked.set(0);
        self.err.set(None);
    }

    /// A `notification` event of the stream.
    pub fn on_event(self, ev: NotificationEvent) {
        self.unacked.set(ev.unacked);
        match ev.kind {
            NotificationChangeKind::Created | NotificationChangeKind::Updated => {
                if let Some(n) = ev.notification {
                    self.apply(n);
                }
            }
            NotificationChangeKind::AckedAll => {
                let upto = ev.notification.map(|n| n.id);
                self.list.update(|l| l.retain(|x| upto.is_some_and(|u| x.id > u)));
            }
        }
        self.generation.update(|g| *g += 1);
    }

    /// Insert, replace or drop (acknowledged) one notification of the bell list.
    fn apply(self, n: Notification) {
        self.list.update(|l| {
            l.retain(|x| x.id != n.id);
            if n.acked_ms.is_none() {
                let at = l.iter().position(|x| x.id < n.id).unwrap_or(l.len());
                l.insert(at, n);
                l.truncate(BELL_CAP);
            }
        });
    }

    /// Acknowledge one notification (`POST /notifications/{id}/ack`).
    pub fn ack(self, id: u64) {
        spawn_local(async move {
            match api::post::<(), Notification>(&format!("/api/v1/notifications/{id}/ack"), None).await {
                Ok(n) => {
                    if self.list.with_untracked(|l| l.iter().any(|x| x.id == id)) {
                        self.unacked.update(|u| *u = u.saturating_sub(1));
                    }
                    self.apply(n);
                    self.generation.update(|g| *g += 1);
                }
                Err(e) => toast(Tone::Err, e.text()),
            }
        });
    }

    /// Acknowledge everything (`POST /notifications/ack-all`).
    pub fn ack_all(self) {
        spawn_local(async move {
            match api::post::<(), AckAllResult>("/api/v1/notifications/ack-all", None).await {
                Ok(r) => {
                    self.list.set(Vec::new());
                    self.unacked.set(0);
                    self.generation.update(|g| *g += 1);
                    toast(Tone::Ok, tf("nt.acked_n", &[("n", &r.acked.to_string())]));
                }
                Err(e) => toast(Tone::Err, e.text()),
            }
        });
    }
}

#[derive(Clone, Copy)]
pub struct Notes {
    /// Alerts active right now, most severe first.
    pub alerts: Memo<Vec<Alert>>,
    /// Notifications of rynasd's engine.
    pub server: Server,
    seen: RwSignal<Vec<String>>,
    /// When each key was first seen (ms since the epoch), for the "since 14:32" line.
    first_seen: RwSignal<Vec<(String, f64)>>,
    /// Unread local alerts plus unacknowledged server notifications.
    pub unread: Memo<usize>,
    /// Most severe unread tone (badge color).
    pub worst: Memo<Tone>,
}

pub fn use_notes() -> Notes {
    expect_context::<Notes>()
}

impl Notes {
    pub fn new(live: Live) -> Self {
        let alerts = Memo::new(move |prev: Option<&Vec<Alert>>| {
            live.system.track(); // the CPU model moves the CPU thresholds
            let hw = live.hardware.read();
            let st = live.storage.read();
            live.sample.with(|s| alerts::derive(s.as_ref(), hw.as_ref(), st.as_ref(), cpu_offset(), prev.map(Vec::as_slice).unwrap_or(&[])))
        });
        let server = Server::new();
        let seen: RwSignal<Vec<String>> = RwSignal::new(
            storage_get(SEEN_KEY).map(|s| s.lines().filter(|l| !l.is_empty()).map(str::to_string).collect()).unwrap_or_default(),
        );
        let first_seen = RwSignal::new(Vec::<(String, f64)>::new());
        let all_keys = move || alerts.with(|a| a.iter().map(|a| a.key.clone()).collect::<Vec<_>>());
        // timestamps, and forget the read state of alerts that went away (so they come back
        // unread), but only once every source has answered at least once
        Effect::new(move || {
            let keys = all_keys();
            let now = js_sys::Date::now();
            first_seen.update(|m| {
                m.retain(|(k, _)| keys.contains(k));
                for k in &keys {
                    if !m.iter().any(|(x, _)| x == k) {
                        m.push((k.clone(), now));
                    }
                }
            });
            let loaded = live.sample.with(Option::is_some) && live.storage.with(Option::is_some) && live.hardware.with(Option::is_some);
            if loaded && seen.with_untracked(|s| s.iter().any(|k| !keys.contains(k))) {
                seen.update(|s| s.retain(|k| keys.contains(k)));
            }
        });
        Effect::new(move || {
            let s = seen.with(|s| s.join("\n"));
            storage_set(SEEN_KEY, &s);
        });
        let unread = Memo::new(move |_| {
            let local = all_keys().iter().filter(|k| !seen.with(|s| s.contains(*k))).count();
            local + server.unacked.get().min(999) as usize
        });
        let worst = Memo::new(move |_| {
            let mut tones: Vec<Tone> = alerts.with(|a| a.iter().filter(|a| !seen.with(|s| s.contains(&a.key))).map(|a| a.tone).collect());
            server.list.with(|l| tones.extend(l.iter().map(notification_tone)));
            [Tone::Err, Tone::Warn, Tone::Info].into_iter().find(|t| tones.contains(t)).unwrap_or(Tone::Info)
        });
        Notes { alerts, server, seen, first_seen, unread, worst }
    }

    /// "Mark all as read": local alerts read, server notifications acknowledged.
    pub fn mark_all_read(self) {
        let keys: Vec<String> = self.alerts.with_untracked(|a| a.iter().map(|a| a.key.clone()).collect());
        self.seen.update(|s| {
            for k in keys {
                if !s.contains(&k) {
                    s.push(k);
                }
            }
        });
        if self.server.unacked.get_untracked() > 0 {
            self.server.ack_all();
        }
    }
}

fn tone_icon(tone: Tone) -> &'static str {
    match tone {
        Tone::Err => icons::CIRCLE_X,
        Tone::Warn => icons::TRIANGLE_ALERT,
        Tone::Ok => icons::CIRCLE_CHECK,
        _ => icons::INFO,
    }
}

/// One server notification in the popover: link to the center, acknowledge button.
fn server_item(n: Notification, server: Server, open: RwSignal<bool>) -> AnyView {
    let tone = notification_tone(&n);
    let id = n.id;
    let when = i18n::short_when(n.last_ms);
    let count = (n.count > 1).then(|| format!("×{}", n.count));
    let title = n.title.clone();
    let ack_label = format!("{}: {}", t("nt.ack"), n.title);
    view! {
        <li class="notif-item">
            <a class=format!("notif unread {}", tone.class()) href="/notifications" on:click=move |_| open.set(false)>
                <span class="notif-icon"><Icon icon=tone_icon(tone) size=18 /></span>
                <span class="notif-body">
                    <span class="notif-line">
                        <strong>{title}</strong>
                        <span class="notif-time tnum">{count.map(|c| view! { <span class="chip">{c}</span> " " })}{when}</span>
                    </span>
                    <span class="notif-text clamp-2">{n.body}</span>
                </span>
                <span class="notif-dot" aria-hidden="true"></span>
            </a>
            <button class="icon-btn sm notif-ack" title=tr("nt.ack") aria-label=ack_label on:click=move |_| server.ack(id)>
                <Icon icon=icons::CHECK size=16 />
            </button>
        </li>
    }
    .into_any()
}

/// Bell button with unread badge and the notification popover.
#[component]
pub fn Bell() -> impl IntoView {
    let ui = use_ui();
    let notes = use_notes();
    let server = notes.server;
    let open = ui.notif_open;
    let list = notes.alerts;
    let empty = move || list.with(Vec::is_empty) && server.list.with(Vec::is_empty);
    let label = move || {
        let n = notes.unread.get();
        if n == 0 { t("nav.notifications").to_string() } else { format!("{} ({n} {})", t("nav.notifications"), t("notif.unread")) }
    };
    view! {
        <div class="pop-anchor">
            <button
                class="icon-btn bell"
                aria-label=label
                title=label
                aria-haspopup="dialog"
                aria-expanded=move || open.get().to_string()
                on:click=move |_| {
                    ui.user_menu_open.set(false);
                    open.update(|o| *o = !*o);
                }
            >
                <Icon icon=icons::BELL />
                <Show when=move || { notes.unread.get() > 0 }>
                    <span class=move || format!("badge {}", notes.worst.get().class()) aria-hidden="true">
                        {move || { let n = notes.unread.get(); if n > 9 { "9+".to_string() } else { n.to_string() } }}
                    </span>
                </Show>
            </button>
            <Show when=move || open.get()>
                <div class="popover notif-pop" role="dialog" aria-label=tr("nav.notifications")>
                    <div class="notif-head">
                        <span class="notif-title">{tr("nav.notifications")}</span>
                        <Show when=move || { notes.unread.get() > 0 }>
                            <span class="chip accent tnum">{move || notes.unread.get()}</span>
                        </Show>
                        <button class="link-btn" disabled=move || notes.unread.get() == 0 on:click=move |_| notes.mark_all_read()>
                            <Icon icon=icons::CHECK_CHECK size=14 />{tr("notif.mark_read")}
                        </button>
                    </div>
                    {move || server.err.get().map(|code| view! {
                        <div class="notif-err" role="alert">
                            <Icon icon=icons::TRIANGLE_ALERT size=14 />
                            <span>{tr("nt.load_failed")}" "{i18n::err_text(&code)}</span>
                            <button class="link-btn" on:click=move |_| server.reload()>{tr("err.retry")}</button>
                        </div>
                    })}
                    <Show
                        when=move || !empty()
                        fallback=|| view! {
                            <EmptyState
                                icon=icons::BELL_OFF
                                title=Signal::derive(|| t("notif.empty").to_string())
                                text=Signal::derive(|| t("notif.empty_text").to_string())
                            />
                        }
                    >
                        <ul class="notif-list">
                            <For each=move || server.list.get() key=|n| (n.id, n.last_ms, n.count, n.severity) let:n>
                                {server_item(n, server, open)}
                            </For>
                            <For each=move || list.get() key=|a| a.key.clone() let:a>
                                {
                                    // the entry stays (keyed), its text follows the live value and the language
                                    let key = a.key.clone();
                                    let cur = {
                                        let key = key.clone();
                                        move || list.with(|l| l.iter().find(|x| x.key == key).cloned())
                                    };
                                    let text = {
                                        let cur = cur.clone();
                                        move || {
                                            cur().map(|a| {
                                                let args: Vec<(&str, &str)> = a.args.iter().map(|(k, v)| (*k, v.as_str())).collect();
                                                tf(a.text, &args)
                                            })
                                        }
                                    };
                                    let (k1, k2, k3) = (key.clone(), key.clone(), key);
                                    let unread = move || !notes.seen.with(|s| s.contains(&k1));
                                    let since = move || notes.first_seen.with(|m| m.iter().find(|(k, _)| *k == k2).map(|(_, t)| *t)).map(crate::i18n::time_hm).unwrap_or_default();
                                    let title = a.title;
                                    view! {
                                        <li>
                                            <a
                                                class=move || format!("notif {}{}", a.tone.class(), if unread() { " unread" } else { "" })
                                                href=a.route
                                                on:click=move |_| {
                                                    notes.seen.update(|s| if !s.contains(&k3) { s.push(k3.clone()) });
                                                    open.set(false);
                                                }
                                            >
                                                <span class="notif-icon"><Icon icon=tone_icon(a.tone) size=18 /></span>
                                                <span class="notif-body">
                                                    <span class="notif-line">
                                                        <strong>{move || t(title)}</strong>
                                                        <span class="notif-time tnum">{move || format!("{} {}", t("notif.since"), since())}</span>
                                                    </span>
                                                    <span class="notif-text">{text}</span>
                                                </span>
                                                <span class="notif-dot" aria-hidden="true"></span>
                                            </a>
                                        </li>
                                    }
                                }
                            </For>
                        </ul>
                    </Show>
                    <div class="notif-foot">
                        <a class="link-btn" href="/notifications" on:click=move |_| open.set(false)>
                            <Icon icon=icons::BELL size=14 />{tr("nt.view_all")}
                        </a>
                        <a class="link-btn" href="/system/notifications" on:click=move |_| open.set(false)>
                            <Icon icon=icons::SETTINGS size=14 />{tr("nt.settings")}
                        </a>
                    </div>
                </div>
            </Show>
        </div>
    }
}
