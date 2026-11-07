//! Docker (D-DOCKER, wave D2 of `docs/phase2/docker-apps.md`): the engine, its containers,
//! compose stacks, images and volumes, over the D1 API (`/api/v1/docker/...`, §5.1).
//!
//! - The page and its navigation entry exist only while Docker is installed or being
//!   installed; without Docker the module's card on the Plugins page offers the install
//!   ([`install_card`], also shown here when the page is opened directly).
//! - Every stack and container is shown, whoever made it; stacks rynas does not manage are
//!   read-only here (start, stop, restart only).
//! - Lists are read again on `docker.changed` events and when a `docker.*` task ends; CPU and
//!   memory come from the aggregated `docker.stats` events and update in place.

mod containers;
mod images;
mod overview;
mod stacks;

pub use overview::install_card;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::TaskStatus;
use rynas_api_types::docker::{ContainerHealth, ContainerState, DockerStats, ManagerKind, PortMapping, StackManager};
use serde::de::DeserializeOwned;

use crate::docker::docker;
use crate::form::tabs;
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::tasks;
use crate::toast::toast;
use crate::ui::{self, Meter, PageHeader, Remote, Skeleton, Tone, WidgetHead};

const TABS: &[&str] = &["dk.tab.overview", "dk.tab.containers", "dk.tab.stacks", "dk.tab.images"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Loading,
    Absent,
    Present,
}

#[component]
pub fn DockerPage() -> impl IntoView {
    let d = docker();
    d.refresh();
    let phase = Memo::new(move |_| {
        d.status.with(|s| match s {
            None => Phase::Loading,
            Some(s) if s.installed || s.task_id.is_some() => Phase::Present,
            Some(_) => Phase::Absent,
        })
    });
    view! {
        <div class="page">
            <PageHeader icon=icons::CONTAINER title="dk.title" subtitle="dk.subtitle" />
            {move || match phase.get() {
                Phase::Loading => view! { <div class="card">{ui::loading(d.status_err.into(), Callback::new(move |_| d.refresh()), 6)}</div> }.into_any(),
                Phase::Absent => view! { <div class="grid">{install_card()}</div> }.into_any(),
                Phase::Present => tabbed(),
            }}
        </div>
    }
}

/// The four tabs; a tab is built on its first visit and kept (lists, drafts, filters).
fn tabbed() -> AnyView {
    let tab = RwSignal::new(0usize);
    let seen = RwSignal::new([true, false, false, false]);
    Effect::new(move || {
        let i = tab.get();
        if !seen.with_untracked(|s| s[i]) {
            seen.update(|s| s[i] = true);
        }
    });
    let panel = move |i: usize, body: fn() -> AnyView| {
        let mounted = Memo::new(move |_| seen.with(|s| s[i]));
        view! {
            <div class="hw-panel" role="tabpanel" id=format!("dkt-p{i}") aria-labelledby=format!("dkt-t{i}") hidden=move || tab.get() != i>
                {move || mounted.get().then(body)}
            </div>
        }
    };
    view! {
        <div class="card hw-tabs">{tabs("dkt", TABS, tab, "dk.title")}</div>
        {panel(0, overview::panel)}
        {panel(1, containers::panel)}
        {panel(2, stacks::panel)}
        {panel(3, images::panel)}
    }
    .into_any()
}

// ------------------------------------------------------------------ shared pieces

/// A list the page shows, read again on every `docker.changed` (only a changed answer
/// changes the data, see [`Remote`]).
fn remote<T: DeserializeOwned + Send + Sync + 'static>(url: &str) -> Remote<T> {
    let r = Remote::<T>::new(url);
    let g = docker().generation;
    Effect::new(move |prev: Option<()>| {
        g.track();
        if prev.is_some() {
            r.reload();
        }
    });
    r
}

/// Starts a `docker.*` action (202) and follows it to its end: the task panel shows it
/// meanwhile, a toast says how it ended. `busy` while it runs.
fn act(url: String, body: Option<String>, busy: RwSignal<bool>) {
    busy.set(true);
    spawn_local(async move {
        let r = tasks::run_task(&url, body).await;
        let _ = busy.try_set(false);
        done_toast(r);
    });
}

fn done_toast(r: Result<rynas_api_types::TaskInfo, crate::api::ApiErr>) {
    match r {
        Ok(tk) if tk.status == TaskStatus::Ok => toast(Tone::Ok, tf("dk.done", &[("task", &tasks::task_label(&tk.kind))])),
        Ok(tk) => toast(Tone::Err, format!("{}: {}", tasks::task_label(&tk.kind), tasks::task_error(&tk))),
        Err(e) if e.status() == 401 => {}
        Err(e) => toast(Tone::Err, e.text()),
    }
}

/// `/api/v1/docker/<what>/<name>/<action>`.
fn action_url(what: &str, name: &str, action: &str) -> String {
    format!("/api/v1/docker/{what}/{}/{action}", crate::api::encode(name))
}

/// State pill of a container: health first while it runs.
fn container_pill(state: ContainerState, health: Option<ContainerHealth>) -> AnyView {
    use ContainerState as S;
    let (tone, key) = match (state, health) {
        (S::Running, Some(ContainerHealth::Unhealthy)) => (Tone::Err, "dk.h.unhealthy"),
        (S::Running, Some(ContainerHealth::Starting)) => (Tone::Info, "dk.h.starting"),
        (S::Running, Some(ContainerHealth::Healthy)) => (Tone::Ok, "dk.h.healthy"),
        (S::Running, None) => (Tone::Ok, "dk.st.running"),
        (S::Restarting, _) => (Tone::Warn, "dk.st.restarting"),
        (S::Paused, _) => (Tone::Warn, "dk.st.paused"),
        (S::Created, _) => (Tone::Unknown, "dk.st.created"),
        (S::Exited, _) => (Tone::Unknown, "dk.st.exited"),
        (S::Dead, _) => (Tone::Err, "dk.st.dead"),
        (S::Removing | S::Stopping, _) => (Tone::Info, "dk.st.stopping"),
        (S::Unknown, _) => (Tone::Unknown, "dk.st.unknown"),
    };
    ui::status(tone, t(key).to_string())
}

fn manager_key(m: StackManager) -> &'static str {
    match m {
        StackManager::Rynas => "dk.m.rynas",
        StackManager::Portainer => "dk.m.portainer",
        StackManager::Untracked => "dk.m.untracked",
    }
}

/// Product name of a Docker manager (names, not translated).
fn manager_name(k: ManagerKind) -> &'static str {
    match k {
        ManagerKind::Arcane => "Arcane",
        ManagerKind::Dockhand => "Dockhand",
        ManagerKind::Portainer => "Portainer",
        ManagerKind::Dockge => "Dockge",
        ManagerKind::Komodo => "Komodo",
    }
}

/// `http(s)://<this host>:<port>/`: where a published port of the NAS answers.
fn port_href(port: u16) -> String {
    let loc = window().location();
    format!("{}//{}:{port}/", loc.protocol().unwrap_or_default(), loc.hostname().unwrap_or_default())
}

/// The first published TCP port.
fn first_port(ports: &[PortMapping]) -> Option<u16> {
    ports.iter().filter(|p| p.protocol == "tcp").filter_map(|p| p.public_port).min()
}

/// A link that opens a container's web page in a new tab.
fn open_link(href: String, label: String) -> AnyView {
    view! {
        <a class="chip accent" href=href target="_blank" rel="noopener noreferrer" aria-label=label>
            <Icon icon=icons::EXTERNAL_LINK size=12 />{tr("dk.open_web")}
        </a>
    }
    .into_any()
}

/// CPU of a sample in % of the whole machine (the sample's numbers are % of one CPU).
fn machine_pct(cpu_pct: f32, cpus: u32) -> f64 {
    cpu_pct as f64 / cpus.max(1) as f64
}

/// CPU and memory of one container from the latest sample (`None`: not in the sample).
fn container_stat(id: &str) -> Option<(f64, u64)> {
    docker().stats.with(|s| s.as_ref().and_then(|s| s.top.iter().find(|x| x.id == id).map(|x| (machine_pct(x.cpu_pct, s.cpus), x.mem_bytes))))
}

// ------------------------------------------------------------------ dashboard widget

/// The dashboard's Docker widget (`docker.summary`), from the `docker.stats` events.
pub fn widget() -> AnyView {
    view! { <DockerWidget /> }.into_any()
}

#[component]
fn DockerWidget() -> impl IntoView {
    let st = docker().stats;
    let num = move |f: fn(&DockerStats) -> u64| move || st.with(|s| s.as_ref().map(f).unwrap_or(0));
    let running = num(|s| s.running as u64);
    let total = num(|s| s.total as u64);
    let mem = num(|s| s.mem_bytes);
    let mem_total = num(|s| s.mem_total_bytes);
    let cpu = move || st.with(|s| s.as_ref().map(|s| machine_pct(s.cpu_pct, s.cpus)).unwrap_or(0.0));
    let mem_pct = Signal::derive(move || {
        let tot = mem_total();
        if tot == 0 { 0.0 } else { mem() as f64 * 100.0 / tot as f64 }
    });
    // the rows follow the ranking (keyed by id: a row moves, it is not built again)
    let ids = Memo::new(move |_| st.with(|s| s.as_ref().map(|s| s.top.iter().map(|x| x.id.clone()).collect::<Vec<_>>()).unwrap_or_default()));
    view! {
        <section class="card widget">
            <WidgetHead icon=icons::CONTAINER title="w.docker">
                <a class="w-link" href="/docker">{tr("dk.w.open")}<Icon icon=icons::CHEVRON_RIGHT size=14 /></a>
            </WidgetHead>
            <Show when=move || st.with(Option::is_some) fallback=|| view! { <Skeleton lines=4 /> }>
                <div class="dk-kpis">
                    <div class="kpi-col">
                        <div class="kpi"><span class="kpi-num">{running}</span><span class="kpi-unit">{move || format!("/ {}", total())}</span></div>
                        <div class="kpi-sub">{tr("dk.w.running")}</div>
                    </div>
                    <div class="kpi-col">
                        <div class="kpi"><span class="kpi-num">{move || i18n::num(cpu(), 1)}</span><span class="kpi-unit">"%"</span></div>
                        <div class="kpi-sub">{tr("dk.w.cpu")}</div>
                    </div>
                    <div class="kpi-col">
                        <div class="kpi"><span class="kpi-num dk-mem">{move || i18n::bytes(mem())}</span></div>
                        <div class="kpi-sub">{move || format!("{} {}", t("m.of"), i18n::bytes(mem_total()))}</div>
                    </div>
                </div>
                <Meter pct=mem_pct tone=Signal::derive(move || ui::usage_tone(mem_pct.get())) />
                <div class="mini-title">{tr("dk.w.top")}</div>
                <ul class="dk-top">
                    <For each=move || ids.get() key=|id| id.clone() let:id>
                        {top_row(id)}
                    </For>
                    <Show when=move || ids.with(Vec::is_empty)>
                        <li class="muted small">{tr("dk.w.none")}</li>
                    </Show>
                </ul>
            </Show>
        </section>
    }
}

/// One container of the widget's top list; its numbers change in place.
fn top_row(id: String) -> AnyView {
    let st = docker().stats;
    let row = Memo::new(move |_| {
        st.with(|s| {
            s.as_ref().and_then(|s| s.top.iter().find(|x| x.id == id).map(|x| (x.name.clone(), x.stack.clone(), machine_pct(x.cpu_pct, s.cpus), x.mem_bytes)))
        })
    });
    let name = Memo::new(move |_| row.with(|r| r.as_ref().map(|r| (r.0.clone(), r.1.clone()))));
    view! {
        <li class="dk-top-row">
            <span class="dk-top-name">
                <span class="ellipsis">{move || name.with(|n| n.as_ref().map(|n| n.0.clone()))}</span>
                <span class="cell-sub ellipsis">{move || name.with(|n| n.as_ref().and_then(|n| n.1.clone()))}</span>
            </span>
            <span class="tnum small">{move || row.with(|r| r.as_ref().map(|r| i18n::pct(r.2)))}</span>
            <span class="tnum small muted">{move || row.with(|r| r.as_ref().map(|r| i18n::bytes(r.3)))}</span>
        </li>
    }
    .into_any()
}
