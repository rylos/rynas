//! Data layer: fetches the API, keeps the live metrics history and polls slow resources.
//! One multiplexed event stream per tab (`/api/v1/events`): metrics samples, hardware status
//! changes, task events (handed to [`crate::tasks`]), notifications (handed to
//! [`crate::notify`]) and `resync`.
//!
//! One store for the whole app, started once: pages read cached data instantly when they mount
//! (no spinner on page change) and the metrics history keeps growing in the background.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::{HardwareStatus, MetricsSample, Session, StorageOverview, SystemInfo, TaskEvent};
use wasm_bindgen::{JsCast, closure::Closure};
use web_sys::{EventSource, MessageEvent};

use crate::api;
use crate::auth::auth;

/// Samples kept in memory: 10 minutes at 1/s.
pub const HISTORY_CAP: usize = 600;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Conn {
    Connecting,
    Live,
    Offline,
    #[cfg_attr(not(feature = "demo"), allow(dead_code))]
    Demo,
}

#[derive(Clone, Copy)]
pub struct Live {
    pub system: RwSignal<Option<SystemInfo>>,
    pub storage: RwSignal<Option<StorageOverview>>,
    pub hardware: RwSignal<Option<HardwareStatus>>,
    /// Latest sample (fine-grained readers: numbers in the DOM).
    pub sample: RwSignal<Option<MetricsSample>>,
    /// Rolling history (charts read it directly when `tick` changes).
    pub history: StoredValue<VecDeque<MetricsSample>>,
    pub tick: RwSignal<u64>,
    pub conn: RwSignal<Conn>,
    /// Seconds since the system info was fetched (uptime keeps counting locally).
    pub uptime_offset: RwSignal<u64>,
    /// Error code of the last failed fetch of each resource (cleared by the next success):
    /// pages show an error state with Retry instead of an endless skeleton.
    pub system_err: RwSignal<Option<String>>,
    pub storage_err: RwSignal<Option<String>>,
    pub hardware_err: RwSignal<Option<String>>,
    /// UPS status (`ups.status` events; `None` until the UPS page or an event asked).
    pub ups: RwSignal<Option<rynas_api_types::ups::UpsStatus>>,
    /// Removable drives (`removable.state` events; the stream sends the latest on connect).
    pub removable: RwSignal<Option<rynas_api_types::removable::RemovableState>>,
}

/// A polled resource (for error states and Retry).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Res {
    System,
    Storage,
    Hardware,
}

/// The open event stream (`/api/v1/events`) and the callbacks registered on it: they are
/// dropped with the stream (no leaked closures per reconnect).
struct Stream {
    es: EventSource,
    _on_sample: Closure<dyn Fn(MessageEvent)>,
    _on_hardware: Closure<dyn Fn(MessageEvent)>,
    _on_ups: Closure<dyn Fn(MessageEvent)>,
    _on_removable: Closure<dyn Fn(MessageEvent)>,
    _on_task: Closure<dyn Fn(MessageEvent)>,
    _on_notif: Closure<dyn Fn(MessageEvent)>,
    _on_docker: Closure<dyn Fn(MessageEvent)>,
    _on_resync: Closure<dyn Fn(MessageEvent)>,
    _on_open: Closure<dyn Fn()>,
    _on_err: Closure<dyn Fn()>,
}

pub fn use_live() -> Live {
    expect_context::<Live>()
}

thread_local! {
    /// The open metrics stream, if any (closed on sign-out / expiry).
    static STREAM: RefCell<Option<Stream>> = const { RefCell::new(None) };
    /// Polling timers are created once and skip their work while signed out.
    static POLLING: Cell<bool> = const { Cell::new(false) };
}

fn close_stream() {
    STREAM.with(|s| {
        if let Some(st) = s.borrow_mut().take() {
            st.es.close();
        }
    });
}

fn stream_open() -> bool {
    STREAM.with(|s| s.borrow().is_some())
}

/// Periodic work runs only while signed in and the tab is visible.
pub fn should_poll() -> bool {
    auth().is_ready() && !page_hidden()
}

/// The one timer of a page that reads something again on its own (next runs, a status that
/// moves): `tick` every `every` while signed in and the tab is visible ([`should_poll`]),
/// stopped with the page. Data that changes only through the UI is not polled.
pub fn poll(every: std::time::Duration, tick: impl Fn() + 'static) {
    if let Ok(h) = set_interval_with_handle(
        move || {
            if should_poll() {
                tick();
            }
        },
        every,
    ) {
        on_cleanup(move || h.clear());
    }
}

/// Live progress: `reload` every `every_ms` while `busy()` says something runs (a backup, a
/// receive) and [`should_poll`] holds; nothing while idle. Each round ends by checking `busy`
/// again (also when the answer did not change), and a change of `busy` starts one.
pub fn poll_while(every_ms: u32, busy: impl Fn() -> bool + Send + Sync + 'static, reload: impl Fn() + Copy + Send + Sync + 'static) {
    let ticking = RwSignal::new(false);
    Effect::new(move |_| {
        if busy() && !ticking.get() {
            ticking.set(true);
            leptos::task::spawn_local(async move {
                crate::api::sleep(every_ms).await;
                let _ = ticking.try_set(false);
                if should_poll() {
                    reload();
                }
            });
        }
    });
}

fn page_hidden() -> bool {
    document().hidden()
}

impl Live {
    pub fn new() -> Self {
        Live {
            system: RwSignal::new(None),
            storage: RwSignal::new(None),
            hardware: RwSignal::new(None),
            sample: RwSignal::new(None),
            history: StoredValue::new(VecDeque::with_capacity(HISTORY_CAP)),
            tick: RwSignal::new(0),
            conn: RwSignal::new(Conn::Connecting),
            uptime_offset: RwSignal::new(0),
            system_err: RwSignal::new(None),
            storage_err: RwSignal::new(None),
            hardware_err: RwSignal::new(None),
            ups: RwSignal::new(None),
            removable: RwSignal::new(None),
        }
    }

    pub fn err(self, r: Res) -> RwSignal<Option<String>> {
        match r {
            Res::System => self.system_err,
            Res::Storage => self.storage_err,
            Res::Hardware => self.hardware_err,
        }
    }

    pub fn refresh(self, r: Res) {
        match r {
            Res::System => self.refresh_system(),
            Res::Storage => self.refresh_storage(),
            Res::Hardware => self.refresh_hardware(),
        }
    }

    /// Record the outcome of a fetch: a 401 is handled by the API layer (login screen).
    fn outcome<T>(self, r: Res, res: Result<T, api::ApiErr>) -> Option<T> {
        match res {
            Ok(v) => {
                if self.err(r).with_untracked(Option::is_some) {
                    self.err(r).set(None);
                }
                Some(v)
            }
            Err(e) => {
                if e.status() != 401 {
                    self.err(r).set(Some(e.code().to_string()));
                }
                None
            }
        }
    }

    pub fn refresh_system(self) {
        spawn_local(async move {
            if let Some(info) = self.outcome(Res::System, api::get::<SystemInfo>("/api/v1/system").await) {
                self.set_system(info);
            }
        });
    }

    /// Ask for the session again now (offline screen's Retry).
    pub fn retry_now(self) {
        self.check_session();
    }

    pub fn set_system(self, info: SystemInfo) {
        crate::ui::set_cpu_model(&info.cpu_model);
        self.system.set(Some(info));
    }

    pub fn push(self, s: MetricsSample) {
        let mut newer = true;
        self.history.update_value(|h| {
            if let Some(last) = h.back()
                && s.ts_ms <= last.ts_ms
            {
                newer = false;
                return;
            }
            if h.len() == HISTORY_CAP {
                h.pop_front();
            }
            h.push_back(s.clone());
        });
        if newer {
            crate::chart::note_sample(s.ts_ms);
            self.sample.set(Some(s));
            self.tick.update(|t| *t += 1);
        }
    }

    /// Boot: `?demo` → demo data without sign-in; otherwise ask for the session (401 shows the
    /// login screen, an unreachable API falls back to demo data when the feature is built in).
    pub fn start(self) {
        // local uptime counter, so the system widget ticks without polling
        let off = self.uptime_offset;
        set_interval(move || off.update(|o| *o += 1), Duration::from_secs(1));
        self.follow_page_lifecycle();

        let forced_demo = window().location().search().unwrap_or_default().contains("demo");
        if forced_demo {
            self.start_demo();
        } else {
            self.check_session();
        }
    }

    /// Close the event stream when the page is left and open it again when the browser
    /// brings the page back from its back/forward cache. A cached page keeps its open
    /// `EventSource`, i.e. one of the six HTTP/1.1 connections the browser allows per
    /// origin: after a few back/forward steps the next page load waits for a free
    /// connection (seen in Chromium: a full load stalled for ~20 s). Registered once for the
    /// life of the page.
    fn follow_page_lifecycle(self) {
        let _ = leptos::prelude::window_event_listener_untyped("pagehide", |_| close_stream());
        let _ = leptos::prelude::window_event_listener_untyped("pageshow", move |ev| {
            let persisted = js_sys::Reflect::get(&ev, &wasm_bindgen::JsValue::from_str("persisted"))
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            // restored from the cache: everything may be stale, read it again
            if persisted && auth().is_ready() {
                self.resume();
            }
        });
    }

    fn check_session(self) {
        spawn_local(async move {
            match api::get::<Session>("/api/v1/auth/session").await {
                Ok(s) => auth().signed_in(s),
                // 401: the API layer already switched to the login screen
                Err(e) if e.status() == 401 => {}
                Err(_) => self.unreachable(),
            }
        });
    }

    #[cfg(feature = "demo")]
    fn unreachable(self) {
        self.start_demo();
    }

    /// Without the demo feature, keep retrying until the API answers.
    #[cfg(not(feature = "demo"))]
    fn unreachable(self) {
        self.conn.set(Conn::Offline);
        auth().unreachable.set(true);
        set_timeout(move || self.check_session(), Duration::from_secs(5));
    }

    #[cfg(feature = "demo")]
    fn start_demo(self) {
        auth().phase.set(crate::auth::Phase::Demo);
        crate::demo::start(self);
    }

    #[cfg(not(feature = "demo"))]
    fn start_demo(self) {
        self.check_session();
    }

    /// Signed in (first time or again): load everything and open the stream.
    pub fn resume(self) {
        if self.conn.get_untracked() != Conn::Live {
            self.conn.set(Conn::Connecting);
        }
        // the history does not wait for the system info: on a phone every hop in the chain is a
        // round trip more before the dashboard can draw
        self.refresh_system();
        spawn_local(async move {
            self.load_history().await;
            if auth().is_ready() && !stream_open() {
                self.open_stream();
            }
        });
        crate::tasks::tasks().reload();
        if let Some(n) = crate::notify::server() {
            n.reload();
        }
        crate::config::changed();
        self.refresh_storage();
        self.refresh_hardware();
        crate::docker::docker().refresh();
        if POLLING.with(|p| p.replace(true)) {
            return;
        }
        // hardware changes arrive on the event stream (`hardware.status`): no polling
        set_interval(
            move || {
                // the storage pages read the inventory themselves meanwhile
                if should_poll() && !crate::pages::storage::pages_open() {
                    self.refresh_storage();
                }
            },
            Duration::from_secs(30),
        );
    }

    async fn load_history(self) {
        if let Ok(hist) = api::get::<Vec<MetricsSample>>("/api/v1/metrics/history?minutes=10").await {
            for s in hist {
                self.push(s);
            }
        }
    }

    /// The stream skipped samples: fill the gap from the history (older samples are ignored).
    fn reload_history(self) {
        spawn_local(async move { self.load_history().await });
    }

    /// Signed out or expired: stop the stream (timers idle by themselves).
    pub fn suspend(self) {
        close_stream();
        if let Some(n) = crate::notify::server() {
            n.clear();
        }
        crate::docker::docker().clear();
        self.conn.set(Conn::Offline);
    }

    pub fn refresh_storage(self) {
        spawn_local(async move {
            if let Some(s) = self.outcome(Res::Storage, api::get::<StorageOverview>("/api/v1/storage").await) {
                // pages rebuild their tables on change only
                if self.storage.with_untracked(|cur| cur.as_ref() != Some(&s)) {
                    self.storage.set(Some(s));
                }
            }
        });
    }

    pub fn refresh_hardware(self) {
        spawn_local(async move {
            if let Some(h) = self.outcome(Res::Hardware, api::get::<HardwareStatus>("/api/v1/hardware").await) {
                self.hardware.set(Some(h));
            }
        });
    }

    /// Reads the UPS status now (UPS page, `resync`); later readings arrive as events.
    pub fn refresh_ups(self) {
        spawn_local(async move {
            if let Ok(u) = api::get::<rynas_api_types::ups::UpsStatus>("/api/v1/ups").await {
                self.ups.set(Some(u));
            }
        });
    }

    /// Reads the removable drives now (`resync`, Refresh); changes arrive as events.
    pub fn refresh_removable(self) {
        spawn_local(async move {
            if let Ok(r) = api::get::<rynas_api_types::removable::RemovableState>("/api/v1/removable").await {
                self.set_removable(r);
            }
        });
    }

    /// A new removable drives state (set only when something changed).
    pub fn set_removable(self, r: rynas_api_types::removable::RemovableState) {
        if self.removable.with_untracked(|cur| cur.as_ref().map(|c| &c.drives) != Some(&r.drives)) {
            self.removable.set(Some(r));
        }
    }

    fn open_stream(self) {
        let Ok(es) = EventSource::new("/api/v1/events") else {
            self.conn.set(Conn::Offline);
            return;
        };
        let on_sample = Closure::<dyn Fn(MessageEvent)>::new(move |ev: MessageEvent| {
            if let Some(txt) = ev.data().as_string()
                && let Ok(s) = crate::api::decode::<MetricsSample>(&txt)
            {
                self.push(s);
            }
        });
        let _ = es.add_event_listener_with_callback("metrics.sample", on_sample.as_ref().unchecked_ref());
        let on_hardware = Closure::<dyn Fn(MessageEvent)>::new(move |ev: MessageEvent| {
            if let Some(txt) = ev.data().as_string()
                && let Ok(h) = crate::api::decode::<HardwareStatus>(&txt)
            {
                if self.hardware_err.with_untracked(Option::is_some) {
                    self.hardware_err.set(None);
                }
                if self.hardware.with_untracked(|cur| cur.as_ref() != Some(&h)) {
                    self.hardware.set(Some(h));
                }
            }
        });
        let _ = es.add_event_listener_with_callback("hardware.status", on_hardware.as_ref().unchecked_ref());
        let on_ups = Closure::<dyn Fn(MessageEvent)>::new(move |ev: MessageEvent| {
            if let Some(txt) = ev.data().as_string()
                && let Ok(u) = crate::api::decode::<rynas_api_types::ups::UpsStatus>(&txt)
                && self.ups.with_untracked(|cur| cur.as_ref() != Some(&u))
            {
                self.ups.set(Some(u));
            }
        });
        let _ = es.add_event_listener_with_callback("ups.status", on_ups.as_ref().unchecked_ref());
        let on_removable = Closure::<dyn Fn(MessageEvent)>::new(move |ev: MessageEvent| {
            if let Some(txt) = ev.data().as_string()
                && let Ok(r) = crate::api::decode::<rynas_api_types::removable::RemovableState>(&txt)
            {
                self.set_removable(r);
            }
        });
        let _ = es.add_event_listener_with_callback("removable.state", on_removable.as_ref().unchecked_ref());
        let on_task = Closure::<dyn Fn(MessageEvent)>::new(move |ev: MessageEvent| {
            if let Some(txt) = ev.data().as_string()
                && let Ok(e) = crate::api::decode::<TaskEvent>(&txt)
            {
                crate::tasks::tasks().on_event(e);
            }
        });
        for name in ["task.started", "task.updated", "task.finished"] {
            let _ = es.add_event_listener_with_callback(name, on_task.as_ref().unchecked_ref());
        }
        let on_notif = Closure::<dyn Fn(MessageEvent)>::new(move |ev: MessageEvent| {
            if let Some(txt) = ev.data().as_string()
                && let Ok(e) = crate::api::decode::<rynas_api_types::notify::NotificationEvent>(&txt)
                && let Some(n) = crate::notify::server()
            {
                n.on_event(e);
            }
        });
        let _ = es.add_event_listener_with_callback("notification", on_notif.as_ref().unchecked_ref());
        // Docker: the widget's sample and change hints (only sent where Docker answers)
        let on_docker = Closure::<dyn Fn(MessageEvent)>::new(move |ev: MessageEvent| {
            let d = crate::docker::docker();
            if ev.type_() == "docker.changed" {
                d.on_changed();
            } else if let Some(txt) = ev.data().as_string()
                && let Some(s) = crate::docker::parse_stats(&txt)
            {
                d.on_stats(s);
            }
        });
        for name in ["docker.stats", "docker.changed"] {
            let _ = es.add_event_listener_with_callback(name, on_docker.as_ref().unchecked_ref());
        }
        // missed events: re-read that collection
        let on_resync = Closure::<dyn Fn(MessageEvent)>::new(move |ev: MessageEvent| {
            let data = ev.data().as_string().unwrap_or_default();
            if data.contains("\"tasks\"") {
                crate::tasks::tasks().reload();
                crate::config::changed();
            } else if data.contains("\"notifications\"") {
                if let Some(n) = crate::notify::server() {
                    n.reload();
                }
            } else if data.contains("\"metrics\"") {
                self.reload_history();
            } else if data.contains("\"hardware\"") {
                self.refresh_hardware();
            } else if data.contains("\"ups\"") {
                self.refresh_ups();
            } else if data.contains("\"removable\"") {
                self.refresh_removable();
            } else if data.contains("\"docker\"") {
                crate::docker::docker().resync();
            }
        });
        let _ = es.add_event_listener_with_callback("resync", on_resync.as_ref().unchecked_ref());
        let conn = self.conn;
        let on_open = Closure::<dyn Fn()>::new(move || conn.set(Conn::Live));
        es.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        // EventSource reconnects by itself after a dropped connection; an error answer (401 when
        // the session ended, 5xx) closes it for good: ask for the session to tell which.
        let es2 = es.clone();
        let on_err = Closure::<dyn Fn()>::new(move || {
            conn.set(Conn::Offline);
            if es2.ready_state() == EventSource::CLOSED {
                // drop the stream (and this closure) outside of its own callback
                let es3 = es2.clone();
                set_timeout(
                    move || {
                        let mine = STREAM.with(|s| s.borrow().as_ref().is_some_and(|st| st.es == es3));
                        if mine {
                            close_stream();
                            self.reopen_later();
                        }
                    },
                    Duration::ZERO,
                );
            }
        });
        es.set_onerror(Some(on_err.as_ref().unchecked_ref()));
        let st = Stream { es, _on_sample: on_sample, _on_hardware: on_hardware, _on_ups: on_ups, _on_removable: on_removable, _on_task: on_task, _on_notif: on_notif, _on_docker: on_docker, _on_resync: on_resync, _on_open: on_open, _on_err: on_err };
        STREAM.with(|s| *s.borrow_mut() = Some(st));
    }

    fn reopen_later(self) {
        set_timeout(
            move || {
                if !auth().is_ready() || stream_open() {
                    return;
                }
                spawn_local(async move {
                    match api::get::<Session>("/api/v1/auth/session").await {
                        Ok(s) => {
                            auth().session.set(Some(s));
                            if !stream_open() {
                                self.open_stream();
                            }
                        }
                        // 401: the API layer shows the login screen
                        Err(e) if e.status() == 401 => {}
                        Err(_) => self.reopen_later(),
                    }
                });
            },
            Duration::from_secs(3),
        );
    }
}
