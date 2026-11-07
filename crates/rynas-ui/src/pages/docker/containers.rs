//! Docker › Containers: every container (filters by text and state), its web page, CPU and
//! memory from the latest `docker.stats` sample (in place), start/stop/restart, and the logs
//! drawer that follows the container's output (`.../logs/stream`, SSE).

use leptos::prelude::*;
use rynas_api_types::docker::{ContainerHealth, ContainerState, ContainerSummary, LogLine, UpdateReport};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{EventSource, MessageEvent};

use super::{act, action_url, container_pill, container_stat, first_port, open_link, port_href, remote};
use crate::form::{search_box, switch};
use crate::i18n::{self, t, tr};
use crate::icons::{self, Icon};
use crate::ui::{self, EmptyState};

const STATES: &[(&str, &str)] = &[("", "dk.f.all"), ("running", "dk.f.running"), ("stopped", "dk.f.stopped"), ("unhealthy", "dk.f.unhealthy")];

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

pub fn panel() -> AnyView {
    let list = remote::<Vec<ContainerSummary>>("/api/v1/docker/containers");
    let updates = remote::<UpdateReport>("/api/v1/docker/updates");
    let q = RwSignal::new(String::new());
    let state = RwSignal::new(String::new());
    let logs = RwSignal::new(None::<ContainerSummary>);
    let table = move || {
        let all = list.data.get().unwrap_or_default();
        if all.is_empty() {
            return view! { <EmptyState icon=icons::CONTAINER title=Signal::derive(|| t("dk.c.none").to_string()) text=Signal::derive(|| t("dk.c.none_text").to_string()) /> }.into_any();
        }
        let (qv, sv) = (q.get(), state.get());
        let upd: Vec<String> = updates.data.with(|u| u.as_ref().map(|u| u.images.iter().filter(|i| i.update_available).map(|i| i.image.clone()).collect()).unwrap_or_default());
        let rows: Vec<AnyView> = all.into_iter().filter(|c| matches(c, &qv, &sv)).map(|c| { let u = upd.contains(&c.image); row(c, u, logs) }).collect();
        if rows.is_empty() {
            return view! { <p class="muted small dk-pad">{tr("dk.c.no_match")}</p> }.into_any();
        }
        ui::table("dk-table", &["dk.col.name", "dk.col.image", "col.status", "dk.col.ports", "#dk.col.cpu", "#dk.col.mem", ""], rows)
    };
    view! {
        <section class="card widget w-12">
            <div class="toolbar">
                {search_box("dk.c.search", q)}
                <select class="input dk-filter" aria-label=tr("dk.f.label") on:change=move |ev| state.set(event_target_value(&ev))>
                    {STATES.iter().map(|(v, k)| view! { <option value=*v selected=move || state.with(|s| s == v)>{tr(k)}</option> }).collect_view()}
                </select>
            </div>
            {move || list.view(6, table)}
        </section>
        {move || logs.get().map(|c| logs_drawer(c, Callback::new(move |_| logs.set(None))))}
    }
    .into_any()
}

fn row(c: ContainerSummary, update: bool, logs: RwSignal<Option<ContainerSummary>>) -> AnyView {
    let busy = RwSignal::new(false);
    let running = c.state == ContainerState::Running;
    let id = c.id.clone();
    let stat = Memo::new(move |_| container_stat(&id));
    let link = if running {
        c.url.clone().or_else(|| first_port(&c.ports).map(port_href)).map(|h| open_link(h, format!("{}: {}", t("dk.open_web"), c.name)))
    } else {
        None
    };
    let ports: Vec<String> =
        c.ports.iter().filter_map(|p| p.public_port.map(|pp| if pp == p.private_port { format!("{pp}/{}", p.protocol) } else { format!("{pp}→{}/{}", p.private_port, p.protocol) })).collect();
    let name = c.name.clone();
    let btn = move |icon: &'static str, key: &'static str, action: &'static str| {
        let (n, label) = (name.clone(), format!("{}: {}", t(key), name));
        view! {
            <button class="icon-btn sm" title=tr(key) aria-label=label disabled=move || busy.get()
                on:click=move |_| act(action_url("containers", &n, action), None, busy)>
                <Icon icon=icon size=16 />
            </button>
        }
    };
    let c2 = c.clone();
    let log_label = format!("{}: {}", t("dk.a.logs"), c.name);
    ui::row(vec![
        ui::cell("dk.col.name", "", view! {
            <div class="cell-main">
                <strong class="dk-name">{c.name.clone()}</strong>
                {c.stack.clone().map(|s| view! { <span class="cell-sub">{match c.service.clone() { Some(sv) => format!("{s} › {sv}"), None => s }}</span> })}
            </div>
        }.into_any()),
        ui::cell("dk.col.image", "wrap", view! {
            <div class="cell-main">
                <span class="mono small">{c.image.clone()}</span>
                {update.then(|| view! { <span class="chip accent"><Icon icon=icons::DOWNLOAD size=12 />{tr("dk.upd.available")}</span> })}
            </div>
        }.into_any()),
        ui::cell("col.status", "", view! { <span title=c.status.clone() lang="en">{container_pill(c.state, c.health)}</span> }.into_any()),
        ui::cell("dk.col.ports", "", view! {
            <div class="chips">
                {link}
                {ports.into_iter().map(|p| view! { <span class="chip mono">{p}</span> }).collect_view()}
            </div>
        }.into_any()),
        ui::cell("dk.col.cpu", "num", view! { {move || stat.get().map(|s| i18n::pct(s.0)).unwrap_or_else(|| "—".into())} }.into_any()),
        ui::cell("dk.col.mem", "num", view! { {move || stat.get().map(|s| i18n::bytes(s.1)).unwrap_or_else(|| "—".into())} }.into_any()),
        ui::actions(view! {
            {if running {
                view! { {btn(icons::SQUARE, "dk.a.stop", "stop")}{btn(icons::REFRESH_CW, "dk.a.restart", "restart")} }.into_any()
            } else {
                btn(icons::PLAY, "dk.a.start", "start").into_any()
            }}
            <button class="icon-btn sm" title=tr("dk.a.logs") aria-label=log_label on:click=move |_| logs.set(Some(c2.clone()))>
                <Icon icon=icons::SCROLL_TEXT size=16 />
            </button>
        }.into_any()),
    ])
}

// ------------------------------------------------------------------ logs

/// Bytes of log kept in the drawer (older lines are dropped).
const LOG_KEEP: usize = 256 * 1024;

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

fn logs_drawer(c: ContainerSummary, close: Callback<()>) -> AnyView {
    let text = RwSignal::new(String::new());
    let follow = RwSignal::new(true);
    let stamps = RwSignal::new(false);
    let state = RwSignal::new(LogState::Following);
    let pre = NodeRef::<leptos::html::Pre>::new();
    let stream = StoredValue::new_local(None::<LogStream>);
    let url_base = format!("/api/v1/docker/containers/{}/logs/stream?tail=200", crate::api::encode(&c.id));
    let append = move |line: String| {
        let at_end = pre.get_untracked().is_none_or(|p| p.scroll_height() - p.scroll_top() - p.client_height() < 24);
        text.update(|t| {
            t.push_str(&line);
            t.push('\n');
            if t.len() > LOG_KEEP {
                let cut = t.len() - LOG_KEEP;
                let cut = t[cut..].find('\n').map(|i| cut + i + 1).unwrap_or(cut);
                t.drain(..cut);
            }
        });
        if at_end && let Some(p) = pre.get_untracked() {
            request_animation_frame(move || p.set_scroll_top(p.scroll_height()));
        }
    };
    let open = move |url: String| {
        stream.set_value(None);
        text.set(String::new());
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
        <pre class="log dk-log" node_ref=pre tabindex="0" aria-label=tr("dk.log.title") aria-live="off">{move || text.get()}</pre>
    }
    .into_any();
    let foot = view! {
        <button type="button" class="btn ghost" on:click=move |_| text.with_untracked(|t| crate::api::save_text(&file, t))>
            <Icon icon=icons::DOWNLOAD size=16 />{tr("dk.log.download")}
        </button>
        <button type="button" class="btn primary" on:click=move |_| close.run(())>{tr("close")}</button>
    }
    .into_any();
    let title = format!("{} · {}", t("dk.log.title"), c.name);
    ui::dialog(ui::DlgKind::Drawer, Signal::stored(title), close, body, Some(foot))
}
