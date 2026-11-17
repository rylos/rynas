//! Schedule (System › Schedule, S5 of `docs/research/scheduling.md`): every scheduled job of
//! the NAS on one page, from `GET /api/v1/scheduler/overview`: a week view (7 days × 24 h,
//! the look of the week grid; a list of days on narrow screens), the next 24 hours as a
//! timeline list, each job a bar of its usual length coloured by its owner, the windows as
//! bands behind them, the conflicts outlined with their sentence; filters by volume and by
//! owner; a click opens the job where it is set (its feature's page, or the task editor).
//! The global maintenance window (`/api/v1/scheduler/maintenance-window`, the shared week
//! grid), "Pause all" and the history of every task (`/api/v1/scheduler/runs`) live here too.

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::scheduler::{
    ConflictKind, ConflictOutcome, MaintenanceWindow, OverviewConflict, OverviewJob, OverviewOwner, OverviewRun, OverviewWindowKind, Resource,
    RunsPage, ScheduledTask, SchedulerOverview, SchedulerPause, TaskAction, TaskRun, TaskView,
};

use super::scheduler::{
    action_text, history_drawer, own_job_key, owner_class, owner_page, owner_text, pause_banner, pause_dialog, run_origin, run_status, task_editor,
    trigger_text,
};
use crate::api;
use crate::form::switch;
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::toast::toast;
use crate::ui::{self, EmptyState, Modal, PageHeader, Remote, Tone};
use crate::week_grid::{GridLevel, GridTone, summary_text, week_grid, week_grid_preview};

const DAY_MS: u64 = 86_400_000;

/// The levels of the maintenance window's grid.
const LEVELS: &[GridLevel] = &[
    GridLevel { code: '0', label_key: "sv.grid_off", tone: GridTone::Off },
    GridLevel { code: '1', label_key: "sv.grid_on", tone: GridTone::Full },
];

/// The window a new maintenance window starts from: every night 01:00–06:00.
fn default_window() -> String {
    (0..168).map(|i| if (1..6).contains(&(i % 24)) { '1' } else { '0' }).collect()
}

/// Local midnight of the day `days` after today (the browser's time).
fn midnight(days: i32) -> u64 {
    let d = js_sys::Date::new_0();
    d.set_hours(0);
    d.set_minutes(0);
    d.set_seconds(0);
    d.set_milliseconds(0);
    d.set_date((d.get_date() as i32 + days) as u32);
    d.get_time() as u64
}

/// A job's name in the interface's language (rynas's own jobs by their action).
fn job_name(j: &OverviewJob) -> String {
    if let Some(k) = own_job_key(&j.id) {
        return t(k).to_string();
    }
    if let Some(a) = &j.action
        && owner_page(a).is_some()
        && !matches!(a, TaskAction::PowerShutdown { .. })
    {
        return action_text(a).0;
    }
    j.name.clone()
}

/// Where a job is changed: its feature's page, or the task editor (a user's task).
#[derive(Clone)]
enum Open {
    Page(String),
    Edit(String),
}

fn open_of(j: &OverviewJob) -> Open {
    let Some(a) = &j.action else { return Open::Page("/backup".into()) };
    if j.system {
        return Open::Page("/system/scheduler".into());
    }
    match (j.owner, a) {
        (OverviewOwner::User, _) => Open::Edit(j.id.clone()),
        (OverviewOwner::Dedup, TaskAction::Dedup { volume }) => Open::Page(crate::pages::storage::dedup::job_href(volume)),
        (OverviewOwner::Snapshots | OverviewOwner::Replication, _) => Open::Page("/snapshots".into()),
        (OverviewOwner::Backup | OverviewOwner::PcBackup, _) => Open::Page("/backup".into()),
        (OverviewOwner::Power, _) => Open::Page("/hardware".into()),
        (OverviewOwner::Updates, _) => Open::Page("/system/updates".into()),
        (OverviewOwner::Docker | OverviewOwner::Apps, _) => Open::Page("/docker".into()),
        _ => Open::Page("/system/scheduler".into()),
    }
}

fn resource_words(r: &Resource) -> String {
    match r {
        Resource::Volume { path } if path == "*" => t("sch.res.every").to_string(),
        Resource::Volume { path } => path.clone(),
        Resource::Disk { disk } => tf("sch.res.disk", &[("disk", disk)]),
        Resource::Packages => t("sch.res.packages").to_string(),
        Resource::Power => t("sch.res.power").to_string(),
        Resource::Maintenance => t("sch.res.maintenance").to_string(),
    }
}

/// A moment: the time of day when it is within the week shown, with its day.
fn at_text(ms: u64) -> String {
    i18n::date_with(ms as f64, &[("weekday", "short"), ("hour", "2-digit"), ("minute", "2-digit")])
}

/// A usual length in words: "25 min", "1 h 12 min", "9 h" (whole hours from 3 h up).
fn length_text(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3 * 3600 { i18n::approx(s + 1800) } else { i18n::duration(s) }
}

fn est_text(j: &OverviewJob) -> String {
    j.estimate_ms.map(|d| format!(" ({})", tf("sv.about", &[("d", &length_text(d))]))).unwrap_or_default()
}

/// The conflict's sentence in the interface's language.
fn conflict_text(c: &OverviewConflict, jobs: &[OverviewJob]) -> String {
    let find = |id: &str| jobs.iter().find(|j| j.id == id);
    let name = |id: &str| find(id).map(job_name).unwrap_or_else(|| id.to_string());
    let a = name(&c.first);
    let b = c.second.as_deref().map(name).unwrap_or_default();
    let est = find(&c.first).map(est_text).unwrap_or_default();
    let when = at_text(c.second_ms.unwrap_or(c.first_ms));
    let res = c.resource.as_ref().map(resource_words).unwrap_or_default();
    match (c.kind, c.outcome) {
        (ConflictKind::Maintenance, ConflictOutcome::Skipped) => tf("sv.c.skipped", &[("a", &a), ("when", &at_text(c.first_ms))]),
        (ConflictKind::Maintenance, _) => tf(
            "sv.c.deferred",
            &[("a", &a), ("when", &at_text(c.first_ms)), ("until", &c.opens_ms.map(at_text).unwrap_or_default())],
        ),
        (_, ConflictOutcome::GivesWay) => tf("sv.c.gives_way", &[("a", &a), ("est", &est), ("b", &b), ("when", &when), ("res", &res)]),
        (_, ConflictOutcome::BothRun) => tf("sv.c.both_run", &[("a", &a), ("est", &est), ("b", &b), ("when", &when), ("res", &res)]),
        _ => tf("sv.c.waits", &[("a", &a), ("est", &est), ("b", &b), ("when", &when), ("res", &res), ("who", &name(&c.affected))]),
    }
}

/// The conflicts a start of a job is part of.
fn conflicts_of<'a>(r: &OverviewRun, o: &'a SchedulerOverview) -> Vec<&'a OverviewConflict> {
    let me = |id: &str, ms: u64| id == r.job && (ms == r.start_ms || Some(ms) == r.deferred_to_ms);
    o.conflicts
        .iter()
        .filter(|c| me(&c.first, c.first_ms) || c.second.as_deref().zip(c.second_ms).is_some_and(|(s, m)| me(s, m)))
        .collect()
}

/// The filters of the page.
#[derive(Clone, Copy)]
struct Filters {
    volume: RwSignal<String>,
    owner: RwSignal<String>,
}

impl Filters {
    fn shows(&self, j: &OverviewJob) -> bool {
        let v = self.volume.get();
        let o = self.owner.get();
        (v.is_empty() || j.resources.iter().any(|r| matches!(r, Resource::Volume { path } if *path == v || path == "*")))
            && (o.is_empty() || owner_class(j.owner) == o)
    }
}

#[component]
pub fn Schedule() -> impl IntoView {
    let tab = RwSignal::new(0usize);
    let from = midnight(0);
    let to = midnight(7) - 1;
    let ov = Remote::<SchedulerOverview>::new(format!("/api/v1/scheduler/overview?from={from}&to={to}"));
    let tasks = Remote::<Vec<TaskView>>::with_config("/api/v1/scheduler/tasks");
    let pause = Remote::<SchedulerPause>::new("/api/v1/scheduler/pause");
    crate::live::poll(Duration::from_secs(60), move || {
        ov.reload();
        pause.reload();
    });
    let pausing = RwSignal::new(false);
    let paused = Memo::new(move |_| pause.data.with(|p| p.as_ref().is_some_and(|p| p.all.is_some())));
    let edit = RwSignal::new(None::<ScheduledTask>);
    let history = RwSignal::new(None::<ScheduledTask>);
    let filters = Filters { volume: RwSignal::new(String::new()), owner: RwSignal::new(String::new()) };
    let open = Callback::new(move |id: String| {
        if let Some(t) = tasks.data.with_untracked(|d| d.as_ref().and_then(|l| l.iter().find(|v| v.task.id == id).map(|v| v.task.clone()))) {
            edit.set(Some(t));
        }
    });
    let show_history = Callback::new(move |id: String| {
        if let Some(t) = tasks.data.with_untracked(|d| d.as_ref().and_then(|l| l.iter().find(|v| v.task.id == id).map(|v| v.task.clone()))) {
            history.set(Some(t));
        }
    });
    view! {
        <div class="page">
            <PageHeader icon=icons::CALENDAR_CLOCK title="sv.title" subtitle="sv.subtitle">
                <a class="btn ghost" href="/system/scheduler"><span class="btn-label">{tr("sch.title")}</span><Icon icon=icons::CHEVRON_RIGHT size=16 /></a>
                <Show when=move || !paused.get()>
                    <button class="btn ghost" on:click=move |_| pausing.set(true)>
                        <Icon icon=icons::PAUSE size=16 /><span class="btn-label">{tr("sch.pause_all")}</span>
                    </button>
                </Show>
            </PageHeader>
            {pause_banner(pause, tasks)}
            {maintenance_card(ov)}
            <section class="card widget w-12">
                {crate::form::tabs("sv", &["sv.tab.week", "sv.tab.next", "sv.tab.history"], tab, "sv.title")}
                <div role="tabpanel" id="sv-p0" aria-labelledby="sv-t0" hidden=move || tab.get() != 0>
                    {move || (tab.get() == 0).then(|| ov.view(6, move || plan_view(ov, filters, open, true)))}
                </div>
                <div role="tabpanel" id="sv-p1" aria-labelledby="sv-t1" hidden=move || tab.get() != 1>
                    {move || (tab.get() == 1).then(|| ov.view(6, move || plan_view(ov, filters, open, false)))}
                </div>
                <div role="tabpanel" id="sv-p2" aria-labelledby="sv-t2" hidden=move || tab.get() != 2>
                    {move || (tab.get() == 2).then(|| history_panel(tasks, show_history))}
                </div>
            </section>
            {move || pausing.get().then(|| pause_dialog(None, Callback::new(move |_| { pause.reload(); ov.reload(); }), Callback::new(move |_| pausing.set(false))))}
            {move || edit.get().map(|task| task_editor(task, false, Callback::new(move |_| { edit.set(None); ov.reload(); })))}
            {move || history.get().map(|task| history_drawer(task, tasks, Callback::new(move |_| history.set(None))))}
        </div>
    }
}

// ------------------------------------------------------------------ maintenance window

fn maintenance_card(ov: Remote<SchedulerOverview>) -> AnyView {
    let win = Remote::<MaintenanceWindow>::new("/api/v1/scheduler/maintenance-window");
    let editing = RwSignal::new(false);
    let shown = Signal::derive(move || win.data.with(|w| w.as_ref().and_then(|w| w.window.clone()).unwrap_or_default()));
    let body = move || {
        let w = win.data.get().and_then(|w| w.window);
        match w {
            None => view! {
                <p class="muted">{tr("sv.mw.off")}</p>
            }
            .into_any(),
            Some(v) => view! {
                <div class="sv-mw-on">
                    {week_grid_preview(LEVELS, shown, "sv.mw.title")}
                    <p>{summary_text(&v, LEVELS)}</p>
                </div>
            }
            .into_any(),
        }
    };
    view! {
        <section class="card widget w-12" aria-labelledby="sv-mw-h">
            <div class="sec-head">
                <h2 class="w-title" id="sv-mw-h">{tr("sv.mw.title")}</h2>
                <button class="btn ghost sm" on:click=move |_| editing.set(true) disabled=move || !win.ready()>
                    <Icon icon=icons::PENCIL size=16 /><span class="btn-label">{tr("edit")}</span>
                </button>
            </div>
            <p class="muted small">{tr("sv.mw.help")}</p>
            {win.view(2, body)}
            {move || editing.get().then(|| maintenance_dialog(win.data.get_untracked().and_then(|w| w.window), ov.data.get_untracked().map(|o| o.jobs).unwrap_or_default(), Callback::new(move |_| { win.reload(); ov.reload(); }), Callback::new(move |_| editing.set(false))))}
        </section>
    }
    .into_any()
}

/// Which jobs wait for the window and which start at their own time (those that could
/// follow it: the switch is in the task's editor).
fn followers(jobs: &[OverviewJob]) -> AnyView {
    let can: Vec<&OverviewJob> = jobs.iter().filter(|j| j.action.as_ref().is_some_and(|a| a.may_follow_maintenance())).collect();
    let list = |follow: bool| {
        let names: Vec<String> = can.iter().filter(|j| j.maintenance == follow).map(|j| job_name(j)).collect();
        if names.is_empty() {
            view! { <p class="muted small">{tr("sv.mw.none")}</p> }.into_any()
        } else {
            view! { <ul class="sv-follow">{names.into_iter().map(|n| view! { <li>{n}</li> }).collect_view()}</ul> }.into_any()
        }
    };
    view! {
        <div class="field">
            <span class="field-label">{tr("sv.mw.follow")}</span>
            {list(true)}
        </div>
        <div class="field">
            <span class="field-label">{tr("sv.mw.own_time")}</span>
            {list(false)}
            <span class="field-help">{tr("sv.mw.own_time_help")}</span>
        </div>
    }
    .into_any()
}

fn maintenance_dialog(cur: Option<String>, jobs: Vec<OverviewJob>, done: Callback<()>, close: Callback<()>) -> AnyView {
    let on = RwSignal::new(cur.is_some());
    let grid = RwSignal::new(cur.unwrap_or_else(default_window));
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let empty = Memo::new(move |_| grid.with(|g| !g.contains('1')));
    let save = Callback::new(move |_| {
        if on.get_untracked() && empty.get_untracked() {
            return;
        }
        let body = MaintenanceWindow { window: on.get_untracked().then(|| grid.get_untracked()) };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = api::put::<_, MaintenanceWindow>("/api/v1/scheduler/maintenance-window", &body).await;
            let _ = busy.try_set(false);
            match r {
                Ok(_) => {
                    toast(Tone::Ok, t("sv.mw.saved").to_string());
                    done.run(());
                    close.run(());
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    });
    let foot = move || crate::form::save_bar(busy, Signal::derive(move || !(on.get() && empty.get())), "save", close, save, error);
    view! {
        <Modal title=Signal::derive(|| t("sv.mw.title").to_string()) on_close=close wide=true foot=foot>
            <div class="form">
                {switch("sv.mw.enabled", Some("sv.mw.enabled_help"), Signal::derive(move || on.get()), Callback::new(move |v| on.set(v)))}
                <Show when=move || on.get()>
                    <div class="field">
                        <span class="field-label">{tr("sv.mw.title")}</span>
                        {week_grid(LEVELS, grid, "sv.mw.title", None)}
                        <span class="field-help">{tr("sv.mw.grid_help")}</span>
                    </div>
                    {move || empty.get().then(|| view! { <p class="form-error">{tr("sv.mw.empty")}</p> })}
                </Show>
                {followers(&jobs)}
            </div>
        </Modal>
    }
    .into_any()
}

// ------------------------------------------------------------------ week view and next 24 h

/// Filters, conflicts, and the week view (`week`) or the next 24 hours.
fn plan_view(ov: Remote<SchedulerOverview>, f: Filters, open: Callback<String>, week: bool) -> AnyView {
    let o = ov.data.get().unwrap_or_default();
    // the filters' choices: the volumes and owners of the jobs
    let mut volumes: Vec<String> = o
        .jobs
        .iter()
        .flat_map(|j| j.resources.iter())
        .filter_map(|r| match r {
            Resource::Volume { path } if path != "*" => Some(path.clone()),
            _ => None,
        })
        .collect();
    volumes.sort();
    volumes.dedup();
    let mut owners: Vec<OverviewOwner> = o.jobs.iter().map(|j| j.owner).collect();
    owners.sort();
    owners.dedup();
    let filters = view! {
        <div class="toolbar sv-filters">
            <label class="sv-filter">
                <span class="field-label">{tr("sv.f.volume")}</span>
                <select class="input" on:change=move |ev| f.volume.set(event_target_value(&ev))>
                    <option value="" selected=move || f.volume.with(String::is_empty)>{tr("sv.f.all_volumes")}</option>
                    {volumes.into_iter().map(|v| { let (v1, v2) = (v.clone(), v.clone()); view! { <option value=v1 selected=move || f.volume.with(|x| *x == v2)>{v}</option> } }).collect_view()}
                </select>
            </label>
            <label class="sv-filter">
                <span class="field-label">{tr("sv.f.owner")}</span>
                <select class="input" on:change=move |ev| f.owner.set(event_target_value(&ev))>
                    <option value="" selected=move || f.owner.with(String::is_empty)>{tr("sv.f.all_owners")}</option>
                    {owners.into_iter().map(|w| { let c = owner_class(w); view! { <option value=c selected=move || f.owner.with(|x| x == c)>{owner_text(w)}</option> } }).collect_view()}
                </select>
            </label>
        </div>
    }
    .into_any();
    let body = move || {
        let o = ov.data.get().unwrap_or_default();
        if o.jobs.is_empty() {
            return view! { <EmptyState icon=icons::CALENDAR_CLOCK title=Signal::derive(|| t("sv.empty").to_string()) text=Signal::derive(|| t("sv.empty_text").to_string()) /> }.into_any();
        }
        let shown: Vec<OverviewJob> = o.jobs.iter().filter(|j| f.shows(j)).cloned().collect();
        let ids: Vec<&str> = shown.iter().map(|j| j.id.as_str()).collect();
        let conflicts: Vec<&OverviewConflict> = o
            .conflicts
            .iter()
            .filter(|c| ids.contains(&c.first.as_str()) || c.second.as_deref().is_some_and(|s| ids.contains(&s)))
            .filter(|c| week || c.first_ms < from_now() + DAY_MS)
            .collect();
        let conflicts_part = conflicts_list(&conflicts, &o.jobs);
        let main = if week { week_part(&o, &shown, open) } else { next_part(&o, &shown, open) };
        view! { {conflicts_part}{main}{legend(&shown)} }.into_any()
    };
    view! { {filters}{body} }.into_any()
}

fn from_now() -> u64 {
    js_sys::Date::now() as u64
}

/// The conflicts in words, each kind of conflict of a pair of jobs once ("and 6 more times").
fn conflicts_list(list: &[&OverviewConflict], jobs: &[OverviewJob]) -> AnyView {
    if list.is_empty() {
        return ().into_any();
    }
    let mut groups: Vec<(String, &OverviewConflict, usize)> = Vec::new();
    for c in list {
        let key = format!("{:?}{:?}{}{:?}", c.kind, c.outcome, c.first, c.second);
        match groups.iter_mut().find(|g| g.0 == key) {
            Some(g) => g.2 += 1,
            None => groups.push((key, c, 1)),
        }
    }
    let items = groups
        .into_iter()
        .map(|(_, c, n)| {
            let more = (n > 1).then(|| view! { " "<span class="muted">{i18n::tn("sv.c.more", (n - 1) as u64, &[])}</span> });
            view! { <li>{conflict_text(c, jobs)}{more}</li> }
        })
        .collect_view();
    ui::note(
        Tone::Warn,
        icons::TRIANGLE_ALERT,
        view! {
            <strong>{i18n::tn("sv.conflicts", list.len() as u64, &[])}</strong>
            <ul class="sv-conflicts">{items}</ul>
        }
        .into_any(),
    )
}

/// A job's bar or list entry opens where it is changed.
fn opener(j: &OverviewJob, open: Callback<String>, class: String, label: String, title: String, style: String, content: AnyView) -> AnyView {
    match open_of(j) {
        Open::Page(href) => view! { <a class=class href=href aria-label=label title=title style=style>{content}</a> }.into_any(),
        Open::Edit(id) => {
            view! { <button type="button" class=class aria-label=label title=title style=style on:click=move |_| open.run(id.clone())>{content}</button> }.into_any()
        }
    }
}

/// One start of a job in words: "02:00–03:10 · Backup offsite (≈1 h 10 min)".
fn run_label(r: &OverviewRun, j: &OverviewJob) -> String {
    format!("{}–{} · {}{}", i18n::time_hm(r.start_ms as f64), i18n::time_hm(r.end_ms as f64), job_name(j), est_text(j))
}

/// The week: one row per day (7 × 24 h), bars placed by time, lanes so that bars of one day
/// never cover each other; on a narrow screen the same days as lists (CSS swaps them).
fn week_part(o: &SchedulerOverview, shown: &[OverviewJob], open: Callback<String>) -> AnyView {
    let days: Vec<(u64, u64)> = (0..7).map(|i| (midnight(i), midnight(i + 1))).collect();
    let now = from_now();
    let rows = days
        .iter()
        .map(|&(ds, de)| {
            let span = (de - ds) as f64;
            let pct = |ms: u64| (ms.clamp(ds, de) - ds) as f64 / span * 100.0;
            // background bands: the maintenance window and the jobs' windows
            let bands = o
                .windows
                .iter()
                .filter(|w| w.end_ms > ds && w.start_ms < de)
                .filter(|w| w.job.as_deref().is_none_or(|id| shown.iter().any(|j| j.id == id)))
                .map(|w| {
                    let class = match (w.kind, w.job.as_deref().and_then(|id| shown.iter().find(|j| j.id == id))) {
                        (OverviewWindowKind::Maintenance, _) => "sv-band sv-band-mw".to_string(),
                        (_, Some(j)) => format!("sv-band own-{}", owner_class(j.owner)),
                        _ => "sv-band".to_string(),
                    };
                    let style = format!("left:{:.3}%;width:{:.3}%", pct(w.start_ms), (pct(w.end_ms) - pct(w.start_ms)).max(0.2));
                    view! { <span class=class style=style></span> }
                })
                .collect_view();
            // the bars, in lanes
            let mut segs: Vec<(&OverviewRun, &OverviewJob, u64, u64)> = o
                .runs
                .iter()
                .filter(|r| r.end_ms > ds && r.start_ms < de)
                .filter_map(|r| shown.iter().find(|j| j.id == r.job).map(|j| (r, j, r.start_ms.max(ds), r.end_ms.min(de))))
                .collect();
            segs.sort_by_key(|s| (s.2, s.3));
            let mut lanes: Vec<u64> = Vec::new();
            let mut placed: Vec<(usize, &OverviewRun, &OverviewJob, u64, u64)> = Vec::new();
            for (r, j, s, e) in segs {
                // a bar is at least ~1.5 % wide: lanes count that width
                let e_vis = e.max(s + (span * 0.015) as u64);
                let lane = match lanes.iter().position(|end| *end <= s) {
                    Some(l) => {
                        lanes[l] = e_vis;
                        l
                    }
                    None => {
                        lanes.push(e_vis);
                        lanes.len() - 1
                    }
                };
                placed.push((lane, r, j, s, e));
            }
            let n_lanes = lanes.len().max(1);
            let bars = placed
                .into_iter()
                .map(|(lane, r, j, s, e)| {
                    let cs = conflicts_of(r, o);
                    let mut class = format!("sv-bar own-{}", owner_class(j.owner));
                    if !cs.is_empty() {
                        class.push_str(" sv-conflict");
                    }
                    if r.skipped {
                        class.push_str(" sv-skipped");
                    }
                    let style = format!("left:{:.3}%;width:{:.3}%;--lane:{lane}", pct(s), (pct(e) - pct(s)).max(0.4));
                    let mut title = run_label(r, j);
                    for c in &cs {
                        title.push_str("\n⚠ ");
                        title.push_str(&conflict_text(c, &o.jobs));
                    }
                    opener(j, open, class, title.clone(), title, style, view! { <span class="sv-bar-text">{job_name(j)}</span> }.into_any())
                })
                .collect_view();
            let label = i18n::date_with(ds as f64, &[("weekday", "short"), ("day", "numeric")]);
            let today = now >= ds && now < de;
            let now_mark = today.then(|| view! { <span class="sv-now" style=format!("left:{:.3}%", pct(now)) aria-hidden="true"></span> });
            view! {
                <div class="sv-day" class:sv-today=today role="listitem">
                    <div class="sv-day-label">{label}</div>
                    <div class="sv-track" style=format!("--lanes:{n_lanes}")>
                        <span class="sv-grid" aria-hidden="true"></span>
                        {bands}
                        {now_mark}
                        {bars}
                    </div>
                </div>
            }
        })
        .collect_view();
    let hours = [0u32, 6, 12, 18]
        .into_iter()
        .map(|h| view! { <span style=format!("left:{:.3}%", f64::from(h) / 24.0 * 100.0)>{i18n::hm(h, 0)}</span> })
        .collect_view();
    // narrow screens: the same days as lists
    let lists = days
        .iter()
        .map(|&(ds, de)| {
            let label = i18n::date_with(ds as f64, &[("weekday", "long"), ("day", "numeric"), ("month", "short")]);
            let items = listed(o, shown, |r| r.start_ms >= ds && r.start_ms < de, open);
            let empty = items.is_empty();
            view! {
                <section class="sv-list-day">
                    <h3 class="mini-title">{label}</h3>
                    {if empty { view! { <p class="muted small">{tr("sv.nothing")}</p> }.into_any() } else { view! { <ul class="sv-items">{items}</ul> }.into_any() }}
                </section>
            }
        })
        .collect_view();
    view! {
        <div class="sv">
            <div class="sv-week" role="list" aria-label=tr("sv.tab.week")>
                <div class="sv-hours" aria-hidden="true"><span class="sv-day-label"></span><div class="sv-hour-scale">{hours}</div></div>
                {rows}
            </div>
            <div class="sv-daylist">{lists}</div>
        </div>
    }
    .into_any()
}

/// The starts of a list: a job that starts more than 6 times in it (snapshots every hour)
/// and has no conflict there is listed once, at its first start, with how many starts it has.
fn listed<'a>(o: &'a SchedulerOverview, shown: &'a [OverviewJob], keep: impl Fn(&OverviewRun) -> bool, open: Callback<String>) -> Vec<AnyView> {
    let runs: Vec<&OverviewRun> = o.runs.iter().filter(|r| keep(r)).filter(|r| shown.iter().any(|j| j.id == r.job)).collect();
    let count = |id: &str| runs.iter().filter(|r| r.job == id).count();
    let calm = |id: &str| runs.iter().filter(|r| r.job == id).all(|r| conflicts_of(r, o).is_empty());
    let mut seen: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for r in &runs {
        let Some(j) = shown.iter().find(|j| j.id == r.job) else { continue };
        let n = count(&r.job);
        if n > 6 && calm(&r.job) {
            if seen.contains(&r.job.as_str()) {
                continue;
            }
            seen.push(&r.job);
            out.push(run_item(r, j, o, open, n));
        } else {
            out.push(run_item(r, j, o, open, 1));
        }
    }
    out
}

/// One start in a list: time, the job (opens where it is set), owner, usual length, what it
/// claims, its conflicts.
fn run_item(r: &OverviewRun, j: &OverviewJob, o: &SchedulerOverview, open: Callback<String>, starts: usize) -> AnyView {
    let cs = conflicts_of(r, o);
    let when = if starts > 1 {
        format!("{} · {}", i18n::time_hm(r.start_ms as f64), i18n::tn("sv.starts", starts as u64, &[]))
    } else {
        format!("{}–{}", i18n::time_hm(r.start_ms as f64), i18n::time_hm(r.end_ms as f64))
    };
    let est = match (j.estimate_ms, r.estimated) {
        (Some(d), true) => tf("sv.usual", &[("d", &length_text(d))]),
        _ => t("sv.no_estimate").to_string(),
    };
    let res: Vec<String> = j.resources.iter().map(resource_words).collect();
    let name = job_name(j);
    let link = opener(j, open, "link-strong".to_string(), name.clone(), String::new(), String::new(), view! { {name.clone()} }.into_any());
    let sched = j
        .trigger
        .as_ref()
        .map(trigger_text)
        .or_else(|| j.every_h.map(|h| tf(if j.window.is_some() { "sv.every_h_window" } else { "sv.every_h" }, &[("h", &h.to_string())])))
        .unwrap_or_default();
    let warn = (!cs.is_empty()).then(|| {
        let lines = cs.iter().map(|c| view! { <li>{conflict_text(c, &o.jobs)}</li> }).collect_view();
        view! { <ul class="sv-item-warn">{lines}</ul> }
    });
    let mut class = format!("sv-item own-{}", owner_class(j.owner));
    if !cs.is_empty() {
        class.push_str(" sv-conflict");
    }
    view! {
        <li class=class>
            <span class="sv-item-time tnum">{when}</span>
            <div class="sv-item-main">
                <div class="sv-item-head">
                    {link}
                    <span class=format!("owner-tag own-{}", owner_class(j.owner))>{owner_text(j.owner)}</span>
                    {j.paused.then(|| ui::status(Tone::Warn, t("sch.paused").to_string()))}
                    {r.skipped.then(|| ui::status(Tone::Unknown, t("sch.skip.maintenance").to_string()))}
                </div>
                <span class="cell-sub">{sched}" · "{est}</span>
                {(!res.is_empty()).then(|| view! { <span class="cell-sub">{tf("sv.uses", &[("res", &res.join(", "))])}</span> })}
                {warn}
            </div>
        </li>
    }
    .into_any()
}

/// The next 24 hours as a timeline list.
fn next_part(o: &SchedulerOverview, shown: &[OverviewJob], open: Callback<String>) -> AnyView {
    let now = from_now();
    let items = listed(o, shown, |r| r.end_ms > now && r.start_ms < now + DAY_MS, open);
    if items.is_empty() {
        return view! { <p class="muted table-none">{tr("sv.nothing_24h")}</p> }.into_any();
    }
    view! { <div class="sv"><ul class="sv-items sv-timeline">{items}</ul></div> }.into_any()
}

/// The owners' colours.
fn legend(shown: &[OverviewJob]) -> AnyView {
    let mut owners: Vec<OverviewOwner> = shown.iter().map(|j| j.owner).collect();
    owners.sort();
    owners.dedup();
    view! {
        <ul class="sv-legend">
            {owners.into_iter().map(|o| view! { <li class=format!("owner-tag own-{}", owner_class(o))>{owner_text(o)}</li> }).collect_view()}
            <li class="sv-legend-mw"><span class="sv-swatch-mw" aria-hidden="true"></span>{tr("sv.mw.title")}</li>
        </ul>
    }
    .into_any()
}

// ------------------------------------------------------------------ history

const HISTORY_PAGE: usize = 50;

/// Outcome filter: (value of the API's `outcome`, label).
const OUTCOMES: &[(&str, &str)] = &[("", "sv.h.all"), ("ok", "sv.h.ok"), ("failed", "sv.h.failed"), ("skipped", "sv.h.skipped"), ("yielded", "sv.h.yielded")];

fn history_panel(tasks: Remote<Vec<TaskView>>, show: Callback<String>) -> AnyView {
    let outcome = RwSignal::new(String::new());
    let task = RwSignal::new(String::new());
    let page = RwSignal::new(None::<RunsPage>);
    let err = RwSignal::new(None::<String>);
    let load = move |before: Option<u64>| {
        let mut url = format!("/api/v1/scheduler/runs?limit={HISTORY_PAGE}");
        let o = outcome.get_untracked();
        if !o.is_empty() {
            url.push_str(&format!("&outcome={o}"));
        }
        let tk = task.get_untracked();
        if !tk.is_empty() {
            url.push_str(&format!("&task={}", api::encode(&tk)));
        }
        if let Some(b) = before {
            url.push_str(&format!("&before={b}"));
        }
        spawn_local(async move {
            match api::get::<RunsPage>(&url).await {
                Ok(mut p) => {
                    if before.is_some()
                        && let Some(old) = page.get_untracked()
                    {
                        let mut runs = old.runs;
                        runs.extend(p.runs);
                        p.runs = runs;
                        let mut names = old.names;
                        names.extend(p.names);
                        p.names = names;
                    }
                    let _ = page.try_set(Some(p));
                    let _ = err.try_set(None);
                }
                Err(e) => {
                    let _ = err.try_set(Some(e.code().to_string()));
                }
            }
        });
    };
    Effect::new(move |_| {
        outcome.track();
        task.track();
        load(None);
    });
    let chips = crate::form::segmented("sv.h.outcome", OUTCOMES, outcome.into(), Callback::new(move |v| outcome.set(v)));
    let task_select = move || {
        let mut l: Vec<(String, String)> = tasks
            .data
            .get()
            .unwrap_or_default()
            .into_iter()
            .map(|v| {
                let name = own_job_key(&v.task.id).map(|k| t(k).to_string()).unwrap_or_else(|| if v.task.name.is_empty() { v.task.id.clone() } else { v.task.name.clone() });
                (v.task.id, name)
            })
            .collect();
        l.sort_by_key(|x| x.1.to_lowercase());
        view! {
            <label class="sv-filter">
                <span class="field-label">{tr("sch.col.task")}</span>
                <select class="input" on:change=move |ev| task.set(event_target_value(&ev))>
                    <option value="" selected=move || task.with(String::is_empty)>{tr("sv.h.all_tasks")}</option>
                    {l.into_iter().map(|(id, n)| { let id2 = id.clone(); view! { <option value=id selected=move || task.with(|x| *x == id2)>{n}</option> } }).collect_view()}
                </select>
            </label>
        }
    };
    let list = move || {
        let Some(p) = page.get() else {
            return match err.get() {
                Some(code) => view! { <crate::ui::ErrorBox code=code on_retry=Callback::new(move |_| load(None)) /> }.into_any(),
                None => view! { <crate::ui::Skeleton lines=4 /> }.into_any(),
            };
        };
        if p.runs.is_empty() {
            return view! { <EmptyState icon=icons::HISTORY title=Signal::derive(|| t("sch.no_runs").to_string()) text=Signal::derive(|| t("sv.h.none").to_string()) /> }.into_any();
        }
        let names = p.names.clone();
        let rows = p
            .runs
            .iter()
            .map(|r: &TaskRun| {
                let (tone, text) = run_status(r);
                let started = r.start_ms.unwrap_or(r.end_ms);
                let dur = match (r.skipped, r.start_ms) {
                    (Some(_), _) => ui::DASH.to_string(),
                    (None, Some(s)) => i18n::elapsed(r.end_ms.saturating_sub(s) / 1000),
                    (None, None) => String::new(),
                };
                let name = own_job_key(&r.task_id).map(|k| t(k).to_string()).or_else(|| names.get(&r.task_id).cloned()).unwrap_or_else(|| r.task_id.clone());
                let origin = run_origin(r);
                let id = r.task_id.clone();
                let label = ui::verb_name("sch.history", &name);
                let pc_label = ui::verb_name("sv.h.open_pc", &name);
                ui::row(vec![
                    ui::cell("sch.col.started", "tnum", view! {
                        <div class="cell-main">
                            <span>{i18n::datetime(started)}</span>
                            {origin.map(|o| view! { <span class="cell-sub">{o}</span> })}
                        </div>
                    }
                    .into_any()),
                    ui::cell("sch.col.task", "", view! { <span class="wrap-anywhere">{name}</span> }.into_any()),
                    ui::cell("sch.col.duration", "num tnum", view! { {dur} }.into_any()),
                    ui::cell("sch.col.result", "", ui::status(tone, text)),
                    // a PC's backup: its history and details are on the PC backups page
                    ui::actions(if id.starts_with("pc:") {
                        view! {
                            <a class="icon-btn sm" href="/backup" title=tr("sv.h.open_pc") aria-label=pc_label>
                                <Icon icon=icons::CHEVRON_RIGHT size=16 />
                            </a>
                        }
                        .into_any()
                    } else {
                        view! {
                            <button class="icon-btn sm" title=tr("sch.history") aria-label=label on:click=move |_| show.run(id.clone())>
                                <Icon icon=icons::FILE_TEXT size=16 />
                            </button>
                        }
                        .into_any()
                    }),
                ])
            })
            .collect::<Vec<_>>();
        let more = p.more;
        let last = p.runs.last().map(|r| r.id);
        view! {
            {ui::table("", &["sch.col.started", "sch.col.task", "#sch.col.duration", "sch.col.result", ""], rows)}
            {more.then(|| view! {
                <div class="load-more">
                    <button class="btn ghost sm" on:click=move |_| load(last)>{tr("nt.load_more")}</button>
                </div>
            })}
        }
        .into_any()
    };
    view! {
        <div class="toolbar sv-filters">
            {task_select}
            <div class="sv-outcomes">{chips}</div>
        </div>
        {list}
    }
    .into_any()
}
