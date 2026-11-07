//! Docker (D-DOCKER, wave D2 of `docs/phase2/docker-apps.md`): the engine, its containers,
//! compose stacks, images and volumes, over the D1 API (`/api/v1/docker/...`, §5.1).
//!
//! - The page and its navigation entry exist only while Docker is installed or being
//!   installed; without Docker the module's card on the Plugins page offers the install
//!   ([`install_card`], also shown here when the page is opened directly).
//! - Every stack and container is shown, whoever made it; stacks rynas does not manage are
//!   read-only here (start, stop, restart only).
//! - Lists are read again on `docker.changed` events (debounced, and only by the visible tab;
//!   a hidden tab reads again when it is shown) and when a `docker.*` task ends; the update
//!   report is one per page and is read again only when a `docker.*` task ends. CPU and memory
//!   come from the aggregated `docker.stats` events and update in place.
//! - Tables are keyed by id: a refresh updates the rows in place (hover, focus, running
//!   actions survive); row actions keep their button focusable while they run
//!   (`aria-disabled`, G10).

mod adopt;
mod containers;
mod images;
mod overview;
mod stacks;

use std::collections::HashSet;

pub use overview::install_card;
pub(crate) use stacks::{launch, reason_key, state_pill};

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::TaskStatus;
use rynas_api_types::docker::{ContainerHealth, ContainerState, ManagerKind, PortMapping, StackManager, UpdateReport, DockerSettingsState};
use serde::de::DeserializeOwned;

use crate::docker::{Stats, docker};
use crate::form::tabs;
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::tasks;
use crate::toast::toast;
use crate::ui::{self, EmptyState, Meter, PageHeader, Remote, Skeleton, Tone, WidgetHead};

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

/// What the tabs share: the visible tab, the page's one update report and the settings.
#[derive(Clone, Copy)]
struct Page {
    tab: RwSignal<usize>,
    updates: Remote<UpdateReport>,
    /// Docker settings (Overview and the stack editor), read again after configuration changes.
    settings: Remote<DockerSettingsState>,
}

fn page() -> Page {
    expect_context::<Page>()
}

/// The four tabs; a tab is built on its first visit and kept (lists, drafts, filters).
fn tabbed() -> AnyView {
    // `?stack=<name>` opens the Stacks tab on that stack
    let deep = leptos_router::hooks::use_query_map().with_untracked(|q| q.get("stack").is_some());
    let tab = RwSignal::new(if deep { 2usize } else { 0 });
    let seen = RwSignal::new([!deep, false, deep, false]);
    Effect::new(move || {
        let i = tab.get();
        if !seen.with_untracked(|s| s[i]) {
            seen.update(|s| s[i] = true);
        }
    });
    // digests change only through a task (update check, pull, update, rollback)
    let updates = Remote::<UpdateReport>::new("/api/v1/docker/updates");
    let ended = docker().ended;
    Effect::new(move |prev: Option<()>| {
        ended.track();
        if prev.is_some() {
            updates.reload();
        }
    });
    provide_context(Page { tab, updates, settings: Remote::with_config("/api/v1/docker/settings") });
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

/// A list shown on tab `tab`, read again after `docker.changed` (debounced) while that tab is
/// visible; a hidden tab reads it again when it is shown (only a changed answer changes the
/// data, see [`Remote`]).
fn remote<T: DeserializeOwned + Send + Sync + 'static>(url: &str, tab: usize) -> Remote<T> {
    let r = Remote::<T>::new(url);
    follow_changes(tab, Callback::new(move |_| r.reload()));
    r
}

/// Body of [`remote`], not generic.
fn follow_changes(tab: usize, reload: Callback<()>) {
    let shown = page().tab;
    crate::docker::follow(docker().generation, Signal::derive(move || shown.get() == tab), reload);
}

/// Images of the update report with a newer registry digest.
fn updatable() -> Memo<Vec<String>> {
    let u = page().updates;
    Memo::new(move |_| u.data.with(|u| u.as_ref().map(|u| u.images.iter().filter(|i| i.update_available).map(|i| i.image.clone()).collect()).unwrap_or_default()))
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

/// Actions running on a panel, by key (`<name>`): rows are kept across refreshes, the set
/// outlives them anyway (a row rebuilt while its action runs stays busy).
type Busy = RwSignal<HashSet<String>>;

/// [`act`] for a row: `key` is in `busy` while the task runs.
fn act_row(url: String, busy: Busy, key: String) {
    if busy.with_untracked(|b| b.contains(&key)) {
        return;
    }
    busy.update(|b| {
        b.insert(key.clone());
    });
    spawn_local(async move {
        let r = tasks::run_task(&url, None).await;
        let _ = busy.try_update(|b| b.remove(&key));
        done_toast(r);
    });
}

/// An icon button of a row action (`<what>/<name>/<action>`): stays focusable while its task
/// runs (`aria-disabled`, clicks ignored), hidden when `shown` is false.
fn row_btn(icon: &'static str, key: &'static str, what: &'static str, name: String, action: &'static str, busy: Busy, shown: Signal<bool>) -> AnyView {
    let label = format!("{}: {}", t(key), name);
    let running = Memo::new({
        let name = name.clone();
        move |_| busy.with(|b| b.contains(&name))
    });
    view! {
        <button class="icon-btn sm" data-act="" title=tr(key) aria-label=label hidden=move || !shown.get()
            aria-disabled=move || running.get().then_some("true")
            on:click=move |_| act_row(action_url(what, &name, action), busy, name.clone())>
            <Icon icon=icon size=16 />
        </button>
    }
    .into_any()
}

/// The "update available" chip.
fn update_chip() -> AnyView {
    view! { <span class="chip accent"><Icon icon=icons::DOWNLOAD size=12 />{tr("dk.upd.available")}</span> }.into_any()
}

/// An image reference (`registry/repo:tag`) that wraps at its separators.
fn image_ref(s: &str) -> AnyView {
    ui::breakable(s, "mono small")
}

/// The empty state of a Docker list.
fn empty(icon: &'static str, title: &'static str, text: Option<&'static str>) -> AnyView {
    view! { <EmptyState icon=icon title=Signal::derive(move || t(title).to_string()) text=Signal::derive(move || text.map(|k| t(k).to_string()).unwrap_or_default()) /> }.into_any()
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

// ------------------------------------------------------------------ dashboard widget

/// The dashboard's Docker widget (`docker.summary`), from the `docker.stats` events.
pub fn widget() -> AnyView {
    view! { <DockerWidget /> }.into_any()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WState {
    Waiting,
    Stopped,
    Live,
}

#[component]
fn DockerWidget() -> impl IntoView {
    let d = docker();
    let st = d.stats;
    // an installed engine that does not answer sends no sample: say so instead of waiting
    let state = Memo::new(move |_| if d.stopped() { WState::Stopped } else if st.with(Option::is_some) { WState::Live } else { WState::Waiting });
    view! {
        <section class="card widget">
            <WidgetHead icon=icons::CONTAINER title="w.docker">
                <a class="w-link" href="/docker">{tr("dk.w.open")}<Icon icon=icons::CHEVRON_RIGHT size=14 /></a>
            </WidgetHead>
            {move || match state.get() {
                WState::Waiting => view! { <Skeleton lines=4 /> }.into_any(),
                WState::Stopped => empty(icons::CONTAINER, "dk.w.stopped", Some("dk.w.stopped_text")),
                WState::Live => widget_body(),
            }}
        </section>
    }
}

fn widget_body() -> AnyView {
    let st = docker().stats;
    let num = move |f: fn(&Stats) -> u64| move || st.with(|s| s.as_ref().map(f).unwrap_or(0));
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
            {ui::keyed(ids.into(), Callback::new(top_row))}
            <Show when=move || ids.with(Vec::is_empty)>
                <li class="muted small">{tr("dk.w.none")}</li>
            </Show>
        </ul>
    }
    .into_any()
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
