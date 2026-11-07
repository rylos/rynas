//! Docker › Containers: every container (filters by text and state), its web page, CPU and
//! memory from the latest `docker.stats` sample (in place), start/stop/restart, and the logs
//! drawer that follows the container's output (`.../logs/stream`, SSE).
//!
//! Rows are keyed by container id: a refresh updates the cells that changed, the filter hides
//! rows instead of rebuilding them.

use std::collections::{HashSet, VecDeque};

use leptos::prelude::*;
use rynas_api_types::docker::{ContainerHealth, ContainerState, ContainerSummary, LogLine};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{EventSource, MessageEvent};

use super::{Busy, container_pill, empty, first_port, image_ref, open_link, port_href, remote, row_btn, update_chip, updatable};
use crate::docker::docker;
use crate::form::{search_box, switch};
use crate::i18n::{self, t, tr};
use crate::icons::{self, Icon};
use crate::ui::{self, Remote};

const STATES: &[(&str, &str)] = &[("", "dk.f.all"), ("running", "dk.f.running"), ("stopped", "dk.f.stopped"), ("unhealthy", "dk.f.unhealthy")];

const HEADS: &[&str] = &["dk.col.name", "dk.col.image", "col.status", "dk.col.ports", "#dk.col.cpu", "#dk.col.mem", ""];

fn matches(c: &ContainerSummary, q: &str, state: &str) -> bool {
    let running = c.state == ContainerState::Running;
    let ok_state = match state {
        "running" => running,
        "stopped" => !running,
        "unhealthy" => c.health == Some(ContainerHealth::Unhealthy),
        _ => true,
    };
    let q = q.trim().to_lowercase();
    ok_state
        && (q.is_empty()
            || c.name.to_lowercase().contains(&q)
            || c.image.to_lowercase().contains(&q)
            || c.stack.as_deref().is_some_and(|s| s.to_lowercase().contains(&q)))
}

/// What every row reads.
#[derive(Clone, Copy)]
struct Ctx {
    list: Remote<Vec<ContainerSummary>>,
    upd: Memo<Vec<String>>,
    q: RwSignal<String>,
    state: RwSignal<String>,
    busy: Busy,
    logs: RwSignal<Option<ContainerSummary>>,
}

pub fn panel() -> AnyView {
    let list = remote::<Vec<ContainerSummary>>("/api/v1/docker/containers", 1);
    let cx = Ctx { list, upd: updatable(), q: RwSignal::new(String::new()), state: RwSignal::new(String::new()), busy: RwSignal::new(HashSet::new()), logs: RwSignal::new(None) };
    let ids = Memo::new(move |_| list.data.with(|l| l.as_ref().map(|l| l.iter().map(|c| c.id.clone()).collect::<Vec<_>>()).unwrap_or_default()));
    let none = Memo::new(move |_| ids.with(Vec::is_empty));
    let no_match = Memo::new(move |_| cx.q.with(|q| cx.state.with(|s| list.data.with(|l| !l.as_ref().is_some_and(|l| l.iter().any(|c| matches(c, q, s)))))));
    let table = move || {
        if none.get() {
            return empty(icons::CONTAINER, "dk.c.none", Some("dk.c.none_text"));
        }
        let body = ui::keyed(ids.into(), Callback::new(move |id| row(id, cx)));
        view! {
            <p class="muted small dk-pad" hidden=move || !no_match.get()>{tr("dk.c.no_match")}</p>
            <div hidden=move || no_match.get()>{ui::table_body("dk-table", HEADS, body)}</div>
        }
        .into_any()
    };
    let state = cx.state;
    view! {
        <section class="card widget w-12">
            <div class="toolbar">
                {search_box("dk.c.search", cx.q)}
                <select class="input dk-filter" aria-label=tr("dk.f.label") on:change=move |ev| state.set(event_target_value(&ev))>
                    {STATES.iter().map(|(v, k)| view! { <option value=*v selected=move || state.with(|s| s == v)>{tr(k)}</option> }).collect_view()}
                </select>
            </div>
            {move || list.view(6, table)}
        </section>
        {move || cx.logs.get().map(|c| logs_drawer(c, Callback::new(move |_| cx.logs.set(None))))}
    }
    .into_any()
}

/// Published ports as text (`8080/tcp`, `8443→443/tcp`).
fn port_texts(c: &ContainerSummary) -> Vec<String> {
    c.ports.iter().filter_map(|p| p.public_port.map(|pp| if pp == p.private_port { format!("{pp}/{}", p.protocol) } else { format!("{pp}→{}/{}", p.private_port, p.protocol) })).collect()
}

/// The web page link (running containers) and the published ports.
fn ports_cell(c: &ContainerSummary) -> AnyView {
    let link = (c.state == ContainerState::Running)
        .then(|| c.url.clone().or_else(|| first_port(&c.ports).map(port_href)))
        .flatten()
        .map(|h| open_link(h, format!("{}: {}", t("dk.open_web"), c.name)));
    view! {
        <div class="chips">
            {link}
            {port_texts(c).into_iter().map(|p| view! { <span class="chip mono">{p}</span> }).collect_view()}
        </div>
    }
    .into_any()
}

/// One container, kept while its id is listed; every cell follows the latest summary.
fn row(id: String, cx: Ctx) -> AnyView {
    let c = Memo::new({
        let id = id.clone();
        move |_| cx.list.data.with(|l| l.as_ref().and_then(|l| l.iter().find(|c| c.id == id).cloned()))
    });
    let Some(first) = c.get_untracked() else { return ().into_any() };
    let name = first.name.clone();
    let stat = Memo::new(move |_| docker().stats.with(|s| s.as_ref().and_then(|s| s.of(&id))));
    let running = Memo::new(move |_| c.with(|c| c.as_ref().is_some_and(|c| c.state == ContainerState::Running)));
    let hidden = Signal::derive(move || !cx.q.with(|q| cx.state.with(|s| c.with(|c| c.as_ref().is_some_and(|c| matches(c, q, s))))));
    let ports = Memo::new(move |_| c.with(|c| c.as_ref().map(|c| (c.state == ContainerState::Running, c.url.clone(), c.ports.clone()))));
    let health = Memo::new(move |_| c.with(|c| c.as_ref().map(|c| (c.state, c.health, c.status.clone()))));
    let has_update = Memo::new(move |_| c.with(|c| c.as_ref().is_some_and(|c| cx.upd.with(|u| u.contains(&c.image)))));
    let sub = first.stack.clone().map(|s| match first.service.clone() {
        Some(sv) => format!("{s} › {sv}"),
        None => s,
    });
    let log_label = format!("{}: {}", t("dk.a.logs"), name);
    let num = |f: fn((f64, u64)) -> String| move || stat.get().map(f).unwrap_or_else(|| "—".into());
    ui::row_hidden(
        vec![
            ui::cell("dk.col.name", "", ui::two_lines(view! { <strong class="dk-name">{name.clone()}</strong> }.into_any(), sub, "cell-sub")),
            ui::cell("dk.col.image", "wrap", view! {
                <div class="cell-main">
                    {image_ref(&first.image)}
                    {move || has_update.get().then(update_chip)}
                </div>
            }.into_any()),
            ui::cell("col.status", "", (move || health.with(|h| h.as_ref().map(|(s, hl, text)| view! { <span title=text.clone() lang="en">{container_pill(*s, *hl)}</span> }.into_any()))).into_any()),
            ui::cell("dk.col.ports", "", (move || {
                ports.track();
                c.with(|c| c.as_ref().map(ports_cell))
            }).into_any()),
            ui::cell("dk.col.cpu", "num", num(|s| i18n::pct(s.0)).into_any()),
            ui::cell("dk.col.mem", "num", num(|s| i18n::bytes(s.1)).into_any()),
            ui::actions(view! {
                {row_btn(icons::SQUARE, "dk.a.stop", "containers", name.clone(), "stop", cx.busy, running.into())}
                {row_btn(icons::REFRESH_CW, "dk.a.restart", "containers", name.clone(), "restart", cx.busy, running.into())}
                {row_btn(icons::PLAY, "dk.a.start", "containers", name.clone(), "start", cx.busy, Signal::derive(move || !running.get()))}
                <button class="icon-btn sm" title=tr("dk.a.logs") aria-label=log_label on:click=move |_| cx.logs.set(c.get_untracked())>
                    <Icon icon=icons::SCROLL_TEXT size=16 />
                </button>
            }.into_any()),
        ],
        hidden,
    )
}

// ------------------------------------------------------------------ logs

/// Bytes of log kept in the drawer (older lines are dropped).
const LOG_KEEP: usize = 256 * 1024;
/// Lines kept in the drawer.
const LOG_LINES: usize = 5000;

/// The open log stream and its callbacks, dropped together (M8).
struct LogStream {
    es: EventSource,
    _on_log: Closure<dyn Fn(MessageEvent)>,
    _on_end: Closure<dyn Fn(MessageEvent)>,
    _on_err: Closure<dyn Fn()>,
}

impl Drop for LogStream {
    fn drop(&mut self) {
        self.es.close();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LogState {
    Following,
    Ended,
    Paused,
    Lost,
}

/// The lines shown: what arrived since the last frame, and (bytes, lines) of every text node
/// already in the `<pre>`, oldest first.
#[derive(Default)]
struct LogBuf {
    pending: String,
    pending_lines: usize,
    scheduled: bool,
    chunks: VecDeque<(usize, usize)>,
    bytes: usize,
    lines: usize,
}

/// Once per frame: one text node with the new lines, the oldest nodes dropped over the limits
/// (no copy of the whole log per line, M5 of the 2026-11-07 review).
fn flush(pre: NodeRef<leptos::html::Pre>, buf: StoredValue<LogBuf, LocalStorage>) {
    let Some(p) = pre.get_untracked() else {
        // the drawer is not on the page yet: next frame
        if buf.try_with_value(|_| ()).is_some() {
            request_animation_frame(move || flush(pre, buf));
        }
        return;
    };
    let at_end = p.scroll_height() - p.scroll_top() - p.client_height() < 24;
    let Some(text) = buf.try_update_value(|b| {
        b.scheduled = false;
        let text = std::mem::take(&mut b.pending);
        b.chunks.push_back((text.len(), b.pending_lines));
        b.bytes += text.len();
        b.lines += b.pending_lines;
        b.pending_lines = 0;
        text
    }) else {
        return;
    };
    if text.is_empty() {
        buf.update_value(|b| {
            b.chunks.pop_back();
        });
        return;
    }
    let _ = p.append_with_str_1(&text);
    buf.update_value(|b| {
        while (b.bytes > LOG_KEEP || b.lines > LOG_LINES) && b.chunks.len() > 1 {
            let (n, l) = b.chunks.pop_front().unwrap_or_default();
            b.bytes -= n;
            b.lines -= l;
            if let Some(first) = p.first_child() {
                let _ = p.remove_child(&first);
            }
        }
    });
    if at_end {
        p.set_scroll_top(p.scroll_height());
    }
}

fn logs_drawer(c: ContainerSummary, close: Callback<()>) -> AnyView {
    let follow = RwSignal::new(true);
    let stamps = RwSignal::new(false);
    let state = RwSignal::new(LogState::Following);
    let pre = NodeRef::<leptos::html::Pre>::new();
    let buf = StoredValue::new_local(LogBuf::default());
    let stream = StoredValue::new_local(None::<LogStream>);
    let url_base = format!("/api/v1/docker/containers/{}/logs/stream?tail=200", crate::api::encode(&c.id));
    let append = move |line: String| {
        let schedule = buf
            .try_update_value(|b| {
                b.pending.push_str(&line);
                b.pending.push('\n');
                b.pending_lines += 1;
                !std::mem::replace(&mut b.scheduled, true)
            })
            .unwrap_or(false);
        if schedule {
            request_animation_frame(move || flush(pre, buf));
        }
    };
    let clear = move || {
        buf.set_value(LogBuf::default());
        if let Some(p) = pre.get_untracked() {
            p.set_text_content(None);
        }
    };
    let open = move |url: String| {
        stream.set_value(None);
        clear();
        let Ok(es) = EventSource::new(&url) else {
            state.set(LogState::Lost);
            return;
        };
        let on_log = Closure::<dyn Fn(MessageEvent)>::new(move |ev: MessageEvent| {
            if let Some(txt) = ev.data().as_string()
                && let Ok(l) = serde_json::from_str::<LogLine>(&txt)
            {
                append(l.line);
            }
        });
        let _ = es.add_event_listener_with_callback("log", on_log.as_ref().unchecked_ref());
        // the container stopped logging (or the server reported an error): no reconnect
        let on_end = Closure::<dyn Fn(MessageEvent)>::new(move |ev: MessageEvent| {
            let lost = ev.type_() == "error";
            let _ = state.try_set(if lost { LogState::Lost } else { LogState::Ended });
            set_timeout(move || { let _ = stream.try_update_value(|s| *s = None); }, std::time::Duration::ZERO);
        });
        let _ = es.add_event_listener_with_callback("end", on_end.as_ref().unchecked_ref());
        let on_err = Closure::<dyn Fn()>::new(move || {
            // a dropped connection would be retried by the browser and repeat the tail
            if state.try_get_untracked() == Some(LogState::Following) {
                let _ = state.try_set(LogState::Lost);
            }
            set_timeout(move || { let _ = stream.try_update_value(|s| *s = None); }, std::time::Duration::ZERO);
        });
        es.set_onerror(Some(on_err.as_ref().unchecked_ref()));
        state.set(LogState::Following);
        stream.set_value(Some(LogStream { es, _on_log: on_log, _on_end: on_end, _on_err: on_err }));
    };
    Effect::new(move || {
        let (f, ts) = (follow.get(), stamps.get());
        if f {
            open(if ts { format!("{url_base}&timestamps=1") } else { url_base.clone() });
        } else {
            stream.set_value(None);
            state.set(LogState::Paused);
        }
    });
    on_cleanup(move || {
        let _ = stream.try_update_value(|s| *s = None);
    });
    let file = format!("{}.log", c.name);
    let status = move || {
        let (tone, key) = match state.get() {
            LogState::Following => (ui::Tone::Info, "dk.log.following"),
            LogState::Ended => (ui::Tone::Unknown, "dk.log.ended"),
            LogState::Paused => (ui::Tone::Unknown, "dk.log.paused"),
            LogState::Lost => (ui::Tone::Warn, "dk.log.lost"),
        };
        ui::status(tone, t(key).to_string())
    };
    let body = view! {
        <div class="dk-log-tools">
            {switch("dk.log.follow", None, follow.into(), Callback::new(move |v| follow.set(v)))}
            {switch("dk.log.stamps", None, stamps.into(), Callback::new(move |v| stamps.set(v)))}
            {status}
            <button type="button" class="btn ghost sm" on:click=move |_| {
                if state.get_untracked() == LogState::Lost {
                    follow.set(false);
                    follow.set(true);
                }
            } hidden=move || state.get() != LogState::Lost>
                <Icon icon=icons::REFRESH_CW size=14 />{tr("err.retry")}
            </button>
        </div>
        <pre class="log dk-log" node_ref=pre tabindex="0" aria-label=tr("dk.log.title") aria-live="off"></pre>
    }
    .into_any();
    let foot = view! {
        <button type="button" class="btn ghost" on:click=move |_| {
            let text = pre.get_untracked().and_then(|p| p.text_content()).unwrap_or_default();
            crate::api::save_text(&file, &text);
        }>
            <Icon icon=icons::DOWNLOAD size=16 />{tr("dk.log.download")}
        </button>
        <button type="button" class="btn primary" on:click=move |_| close.run(())>{tr("close")}</button>
    }
    .into_any();
    let title = format!("{} · {}", t("dk.log.title"), c.name);
    ui::dialog(ui::DlgKind::Drawer, Signal::stored(title), close, body, Some(foot))
}
