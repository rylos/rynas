//! Docker › Stacks: every compose project, grouped by who manages it (rynas, Portainer,
//! nobody known). rynas stacks (`/<vol>/docker/stacks/<name>/`) can be created, edited
//! (compose.yaml + .env, validated with `docker compose config`), started, pulled, updated
//! with a snapshot first, rolled back to a snapshot and removed; the others only started,
//! stopped and restarted, or adopted where they are ([`super::adopt`]). Image updates come
//! from the digest check (`/docker/updates`, one report for the page). Rows are keyed by
//! stack name and follow the list in place.

use std::collections::HashSet;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::docker::{
    AutoOutcome, CreateStackRequest, DockerSettingsState, HoldReason, RemoveStackRequest, RollbackRequest, StackAutoUpdate, StackCreated, StackDetail, StackFiles, StackManager, StackSnapshot,
    StackState, StackSummary, StackUpdate, StackValidation, UpdatePolicy, UpdateStackRequest, UpdateVerdict, ValidateStackRequest, valid_stack_name,
};

use super::{Busy, act, action_url, container_pill, done_toast, empty, image_ref, manager_key, page, remote, row_btn, update_chip, updatable};
use crate::api;
use crate::config::{apply_now, keys};
use crate::form::{In, SaveBar, code_field, form_error, save_bar_with, search_box, select_field, spinner, switch, text_field};
use crate::toast::saved_applied;
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::tasks::{self, tasks};
use crate::toast::toast;
use crate::ui::{self, Confirm, Remote, Tone, kv_row};

const GROUPS: [(StackManager, &str); 3] = [(StackManager::Rynas, "dk.g.rynas"), (StackManager::Portainer, "dk.g.portainer"), (StackManager::Untracked, "dk.g.untracked")];
const HEADS: &[&str] = &["dk.col.stack", "col.status", "#dk.col.running", "dk.col.images", ""];

pub(crate) fn state_pill(s: StackState) -> AnyView {
    let (tone, key) = match s {
        StackState::Running => (Tone::Ok, "dk.ss.running"),
        StackState::Partial => (Tone::Warn, "dk.ss.partial"),
        StackState::Stopped => (Tone::Unknown, "dk.ss.stopped"),
        StackState::Down => (Tone::Unknown, "dk.ss.down"),
    };
    ui::status(tone, t(key).to_string())
}

fn stack_url(name: &str, rest: &str) -> String {
    format!("/api/v1/docker/stacks/{}{rest}", api::encode(name))
}

/// Starts an action from a dialog: a refusal stays in the dialog (`error`); once started the
/// dialog closes (`started`), the task panel shows the log and a toast the end.
pub(crate) fn launch(url: String, body: Option<String>, busy: RwSignal<bool>, error: RwSignal<Option<String>>, started: Callback<()>) {
    busy.set(true);
    error.set(None);
    spawn_local(async move {
        match api::start_task(&url, body).await {
            Ok(id) => {
                let _ = busy.try_set(false);
                started.run(());
                tasks().show(&id);
                done_toast(tasks::follow(&id).await);
            }
            Err(e) => {
                let _ = busy.try_set(false);
                let _ = error.try_set(Some(e.text()));
            }
        }
    });
}

fn shown(s: &StackSummary, q: &str) -> bool {
    let q = q.trim().to_lowercase();
    q.is_empty() || s.name.contains(&q)
}

/// What every row reads.
#[derive(Clone, Copy)]
struct Ctx {
    list: Remote<Vec<StackSummary>>,
    upd: Memo<Vec<String>>,
    q: RwSignal<String>,
    busy: Busy,
    detail: RwSignal<Option<String>>,
}

pub fn panel() -> AnyView {
    let list = remote::<Vec<StackSummary>>("/api/v1/docker/stacks", 2);
    let updates = page().updates;
    let settings = page().settings;
    // `/docker?stack=<name>` opens that stack's details (links from Apps & Plugins)
    let detail = RwSignal::new(leptos_router::hooks::use_query_map().with_untracked(|q| q.get("stack")));
    let cx = Ctx { list, upd: updatable(), q: RwSignal::new(String::new()), busy: RwSignal::new(HashSet::new()), detail };
    let edit = RwSignal::new(None::<Option<String>>);
    let importing = RwSignal::new(false);
    let checking = RwSignal::new(false);
    let checked = move || {
        updates.data.with(|u| {
            u.as_ref().map(|u| {
                let last = if u.checked_at_ms == 0 { t("dk.upd.never").to_string() } else { tf("dk.upd.checked", &[("when", &i18n::short_when(u.checked_at_ms))]) };
                if u.next_check_ms == 0 { last } else { format!("{last} · {}", tf("dk.upd.next", &[("when", &i18n::short_when(u.next_check_ms))])) }
            })
        })
    };
    let none = Memo::new(move |_| list.data.with(|l| l.as_ref().is_none_or(Vec::is_empty)));
    let groups = move || {
        if none.get() {
            return empty(icons::LAYERS, "dk.s.none", Some("dk.s.none_text"));
        }
        GROUPS.iter().map(|(m, key)| group(*m, key, cx)).collect_view().into_any()
    };
    view! {
        <section class="card widget w-12">
            <div class="toolbar">
                {search_box("dk.s.search", cx.q)}
                <span class="small muted">{checked}</span>
                <button class="btn ghost" aria-disabled=move || checking.get().then_some("true")
                    on:click=move |_| if !checking.get_untracked() { act("/api/v1/docker/updates/check".into(), None, checking) }>
                    {spinner(checking)}<Icon icon=icons::REFRESH_CW size=16 /><span class="btn-label">{tr("upd.check")}</span>
                </button>
                <button class="btn ghost" on:click=move |_| importing.set(true)>
                    <Icon icon=icons::FOLDER_OPEN size=16 /><span class="btn-label">{tr("dk.im.button")}</span>
                </button>
                <button class="btn primary" on:click=move |_| edit.set(Some(None))>
                    <Icon icon=icons::PLUS size=16 /><span class="btn-label">{tr("dk.s.create")}</span>
                </button>
            </div>
            {move || list.view(6, groups)}
        </section>
        {move || detail.get().map(|n| detail_drawer(n, settings, edit, Callback::new(move |_| detail.set(None))))}
        {move || edit.get().map(|n| editor(n, settings, Callback::new(move |_| edit.set(None))))}
        {move || importing.get().then(|| super::adopt::import_dialog(Callback::new(move |_| importing.set(false))))}
    }
    .into_any()
}

/// The stacks of one manager: a keyed table, hidden while none of them matches the search.
fn group(m: StackManager, key: &'static str, cx: Ctx) -> AnyView {
    let names = Memo::new(move |_| cx.list.data.with(|l| l.as_ref().map(|l| l.iter().filter(|s| s.manager == m).map(|s| s.name.clone()).collect::<Vec<_>>()).unwrap_or_default()));
    let hidden = Memo::new(move |_| cx.q.with(|q| cx.list.data.with(|l| !l.as_ref().is_some_and(|l| l.iter().any(|s| s.manager == m && shown(s, q))))));
    let body = ui::keyed(names.into(), Callback::new(move |n| row(n, cx)));
    view! {
        <div hidden=move || hidden.get()>
            <h3 class="mini-title dk-group">{tr(key)}</h3>
            {ui::table_body("dk-table", HEADS, body)}
        </div>
    }
    .into_any()
}

/// One stack, kept while its name is listed; cells follow the latest summary.
fn row(name: String, cx: Ctx) -> AnyView {
    let s = Memo::new({
        let name = name.clone();
        move |_| cx.list.data.with(|l| l.as_ref().and_then(|l| l.iter().find(|s| s.name == name).cloned()))
    });
    let Some(first) = s.get_untracked() else { return ().into_any() };
    let hidden = Signal::derive(move || !cx.q.with(|q| s.with(|s| s.as_ref().is_some_and(|s| shown(s, q)))));
    let state = Memo::new(move |_| s.with(|s| s.as_ref().map(|s| (s.state, s.unhealthy, s.running, s.containers))));
    let images = Memo::new(move |_| s.with(|s| s.as_ref().map(|s| s.images.clone()).unwrap_or_default()));
    let plan = stack_update(name.clone());
    let has_update = Memo::new(move |_| plan.with(Option::is_some) || images.with(|i| cx.upd.with(|u| i.iter().any(|x| u.contains(x)))));
    let auto = Memo::new(move |_| plan.with(|p| p.as_ref().is_some_and(|p| p.enabled && p.verdict != UpdateVerdict::Hold)));
    let can_start = Signal::derive(move || state.with(|s| s.is_some_and(|s| s.0 != StackState::Running)));
    let can_stop = Signal::derive(move || state.with(|s| s.is_some_and(|s| s.2 > 0)));
    let start = if first.editable { "up" } else { "start" };
    let open_label = ui::verb_name("dk.a.details", &name);
    let detail = cx.detail;
    let (n1, n2) = (name.clone(), name.clone());
    ui::row_hidden(
        vec![
            ui::cell("dk.col.stack", "", ui::two_lines(
                view! { <button class="link-strong" on:click=move |_| detail.set(Some(n1.clone()))>{name.clone()}</button> }.into_any(),
                first.working_dir.clone(),
                "cell-sub mono",
            )),
            ui::cell("col.status", "", (move || state.get().map(|(st, unhealthy, ..)| view! {
                <div class="chips">
                    {state_pill(st)}
                    {(unhealthy > 0).then(|| ui::status(Tone::Err, tf("dk.s.unhealthy_n", &[("n", &unhealthy.to_string())])))}
                </div>
            }.into_any())).into_any()),
            ui::cell("dk.col.running", "num", (move || state.get().map(|s| i18n::of(&s.2.to_string(), &s.3.to_string()))).into_any()),
            ui::cell("dk.col.images", "wrap", view! {
                <div class="cell-main">
                    {move || images.with(|l| l.iter().map(|i| image_ref(i)).collect_view())}
                    {move || has_update.get().then(update_chip)}
                    {move || auto.get().then(|| view! { <span class="chip"><Icon icon=icons::CLOCK size=12 />{tr("dk.au.chip")}</span> })}
                </div>
            }.into_any()),
            ui::actions(view! {
                {row_btn(icons::PLAY, "dk.a.start", "stacks", name.clone(), start, cx.busy, can_start)}
                {row_btn(icons::SQUARE, "dk.a.stop", "stacks", name.clone(), "stop", cx.busy, can_stop)}
                {row_btn(icons::REFRESH_CW, "dk.a.restart", "stacks", name.clone(), "restart", cx.busy, can_stop)}
                <button class="icon-btn sm" title=tr("dk.a.details") aria-label=open_label on:click=move |_| detail.set(Some(n2.clone()))>
                    <Icon icon=icons::CHEVRON_RIGHT size=16 />
                </button>
            }.into_any()),
        ],
        hidden,
    )
}

// ------------------------------------------------------------------ detail

/// What a dialog of the detail drawer asks.
#[derive(Clone)]
enum Ask {
    Update,
    Remove,
    Rollback(StackSnapshot),
    DeleteSnapshot(StackSnapshot),
    Adopt,
    Release,
    Move,
}

pub(crate) fn reason_key(r: &str) -> &'static str {
    match r {
        "update" => "dk.snap.r_update",
        "pull" => "dk.snap.r_pull",
        "before-rollback" => "dk.snap.r_rollback",
        _ => "dk.snap.r_manual",
    }
}

/// A button of the drawer's action bar: focusable while its task runs (`aria-disabled`).
fn bar_btn(icon: &'static str, key: &'static str, busy: RwSignal<bool>, run: Callback<()>) -> AnyView {
    view! {
        <button class="btn ghost sm" aria-disabled=move || busy.get().then_some("true") on:click=move |_| if !busy.get_untracked() { run.run(()) }>
            <Icon icon=icon size=14 />{tr(key)}
        </button>
    }
    .into_any()
}

/// The containers of a stack.
fn stack_containers(det: &StackDetail) -> AnyView {
    if det.containers.is_empty() {
        return ui::empty_line("dk.s.no_containers");
    }
    let rows = det
        .containers
        .iter()
        .map(|c| {
            ui::row(vec![
                ui::cell("dk.col.name", "", ui::two_lines(view! { <strong>{c.name.clone()}</strong> }.into_any(), Some(c.image.clone()), "cell-sub mono")),
                ui::cell("col.status", "", container_pill(c.state, c.health)),
            ])
        })
        .collect::<Vec<_>>();
    ui::table("", &["dk.col.name", "col.status"], rows)
}

/// The snapshots of a rynas stack (rollback, delete).
fn snapshot_rows(det: &StackDetail, ask: RwSignal<Option<Ask>>) -> AnyView {
    if det.snapshots.is_empty() {
        return ui::empty_line("dk.snap.none");
    }
    let rows = det
        .snapshots
        .iter()
        .map(|sn| {
            let (a, b) = (sn.clone(), sn.clone());
            let imgs = sn.images.iter().map(|i| format!("{}: {}", i.service, i.image)).collect::<Vec<_>>();
            ui::row(vec![
                ui::cell("dk.col.created", "", ui::two_lines(ui::text(i18n::datetime(sn.created_s.max(0) as u64 * 1000)), Some(t(reason_key(&sn.reason)).to_string()), "cell-sub")),
                ui::cell("dk.col.images", "wrap", view! { <div class="cell-main">{imgs.iter().map(|i| image_ref(i)).collect_view()}</div> }.into_any()),
                ui::actions(view! {
                    <button class="btn ghost sm" on:click=move |_| ask.set(Some(Ask::Rollback(a.clone())))><Icon icon=icons::ROTATE_CCW size=14 />{tr("snap.rollback")}</button>
                    <button class="icon-btn sm danger" title=tr("delete") aria-label=ui::verb_name("delete", &sn.id) on:click=move |_| ask.set(Some(Ask::DeleteSnapshot(b.clone())))><Icon icon=icons::TRASH_2 size=16 /></button>
                }.into_any()),
            ])
        })
        .collect::<Vec<_>>();
    ui::table("", &["dk.col.created", "dk.col.images", ""], rows)
}

/// The lifecycle actions of the status card: built once per stack kind, so a refresh of the
/// details keeps the buttons (and the focus on them). Edit and Remove are in the footer.
fn action_bar(managed: bool, adoption: Adoption, n: StoredValue<String>, busy: RwSignal<bool>, ask: RwSignal<Option<Ask>>) -> AnyView {
    let task = move |action: &'static str| Callback::new(move |_| act(action_url("stacks", &n.get_value(), action), None, busy));
    let idle = RwSignal::new(false);
    let open = move |a: Ask| Callback::new(move |_| ask.set(Some(a.clone())));
    let adopted = (adoption == Adoption::Adopted).then(|| {
        view! {
            {bar_btn(icons::FOLDER_SYNC, "dk.ad.move", idle, open(Ask::Move))}
            {bar_btn(icons::UNPLUG, "dk.ad.release", idle, open(Ask::Release))}
        }
    });
    if managed {
        view! {
            <div class="sec-actions">
                {bar_btn(icons::PLAY, "dk.a.up", busy, task("up"))}
                {bar_btn(icons::REFRESH_CW, "dk.a.restart", busy, task("restart"))}
                {bar_btn(icons::SQUARE, "dk.a.stop", busy, task("stop"))}
                {bar_btn(icons::DOWNLOAD, "dk.a.pull", busy, task("pull"))}
                {bar_btn(icons::DOWNLOAD, "dk.a.update", idle, Callback::new(move |_| ask.set(Some(Ask::Update))))}
                {bar_btn(icons::CIRCLE_X, "dk.a.down", busy, task("down"))}
                {adopted}
            </div>
        }
        .into_any()
    } else {
        view! {
            <div class="sec-actions">
                {bar_btn(icons::PLAY, "dk.a.start", busy, task("start"))}
                {bar_btn(icons::SQUARE, "dk.a.stop", busy, task("stop"))}
                {bar_btn(icons::REFRESH_CW, "dk.a.restart", busy, task("restart"))}
                {(adoption == Adoption::Candidate).then(|| bar_btn(icons::IMPORT, "dk.ad.adopt", idle, open(Ask::Adopt)))}
                {adopted}
            </div>
        }
        .into_any()
    }
}

/// Where a stack stands with adoption.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Adoption {
    /// A rynas stack in the stacks folder (or one whose folder is missing).
    Native,
    /// Adopted where it is.
    Adopted,
    /// Portainer's or another project: can be adopted.
    Candidate,
}

fn adoption_of(s: &StackSummary) -> Adoption {
    if s.adopted.is_some() {
        Adoption::Adopted
    } else if s.manager == StackManager::Rynas {
        Adoption::Native
    } else {
        Adoption::Candidate
    }
}

fn detail_drawer(name: String, settings: Remote<DockerSettingsState>, edit: RwSignal<Option<Option<String>>>, close: Callback<()>) -> AnyView {
    let d = remote::<StackDetail>(&stack_url(&name, ""), 2);
    let upd = updatable();
    let busy = RwSignal::new(false);
    let ask = RwSignal::new(None::<Ask>);
    let snap_busy = RwSignal::new(false);
    let snap_err = RwSignal::new(None::<String>);
    let n = StoredValue::new(name.clone());
    // what decides the layout; the rest follows the details in place
    let kind = Memo::new(move |_| d.data.with(|d| d.as_ref().map(|d| (d.summary.editable, d.summary.snapshots, d.summary.manager, adoption_of(&d.summary)))));
    let take_snapshot = move |_| {
        snap_busy.set(true);
        snap_err.set(None);
        spawn_local(async move {
            let r = api::post::<(), StackSnapshot>(&stack_url(&n.get_value(), "/snapshots"), None).await;
            let _ = snap_busy.try_set(false);
            match r {
                Ok(_) => {
                    toast(Tone::Ok, t("dk.snap.taken").to_string());
                    d.reload();
                }
                Err(e) => {
                    let _ = snap_err.try_set(Some(e.text()));
                }
            }
        });
    };
    let content = move || {
        let Some((managed, can_snap, manager, adoption)) = kind.get() else { return ().into_any() };
        let adopted = adoption == Adoption::Adopted;
        let summary = Memo::new(move |_| d.data.with(|d| d.as_ref().map(|d| d.summary.clone())));
        let new_images = move || {
            summary.with(|s| s.as_ref().map(|s| s.images.iter().filter(|i| upd.with(|u| u.contains(i))).cloned().collect::<Vec<_>>()).unwrap_or_default())
        };
        view! {
            // the status card: state, who manages it, the lifecycle actions
            <section class="status-card">
                <div class="status-card-head">
                    {move || summary.with(|s| s.as_ref().map(|s| state_pill(s.state)))}
                    <span class="chip">{t(manager_key(manager))}</span>
                    {adopted.then(|| view! { <span class="chip">{tr("dk.ad.chip")}</span> })}
                </div>
                {action_bar(managed, adoption, n, busy, ask)}
            </section>
            {move || summary.with(|s| s.as_ref().and_then(|s| s.adopted.as_ref().map(super::adopt::adopted_notes)))}
            {move || summary.with(|s| s.as_ref().map(|s| view! {
                <dl class="kvs">
                    {s.working_dir.clone().map(|w| kv_row("dk.s.folder", Signal::stored(w), "mono"))}
                    {(!s.config_files.is_empty()).then(|| kv_row("dk.s.files", Signal::stored(s.config_files.join(", ")), "mono"))}
                </dl>
            }))}
            {(!managed && !adopted).then(|| ui::note(Tone::Info, icons::INFO, ui::text(t(if manager == StackManager::Rynas { "dk.s.no_folder" } else if adoption == Adoption::Candidate { "dk.s.readonly_adopt" } else { "dk.s.readonly" }).to_string())))}
            {move || { let l = new_images(); (!l.is_empty()).then(|| ui::note(Tone::Info, icons::DOWNLOAD, ui::text(tf("dk.upd.stack", &[("images", &l.join(", "))])))) }}
            {managed.then(|| auto_section(n.get_value(), settings))}
            <div class="mini-title">{tr("dk.s.containers")}</div>
            {move || d.data.with(|d| d.as_ref().map(stack_containers))}
            {(managed && !adopted).then(|| view! {
                <div class="dk-snap-head">
                    <div class="mini-title">{tr("dk.snap.title")}</div>
                    {can_snap.then(|| view! {
                        <button class="btn ghost sm" aria-disabled=move || snap_busy.get().then_some("true") on:click=move |ev| if !snap_busy.get_untracked() { take_snapshot(ev) }>
                            {spinner(snap_busy)}<Icon icon=icons::CAMERA size=14 />{tr("dk.snap.take")}
                        </button>
                    })}
                </div>
                {form_error(snap_err)}
                {if can_snap {
                    (move || d.data.with(|d| d.as_ref().map(|d| snapshot_rows(d, ask)))).into_any()
                } else {
                    ui::note(Tone::Info, icons::INFO, ui::text(t("dk.snap.not_subvolume").to_string()))
                }}
            })}
        }
        .into_any()
    };
    let dialogs = move || {
        // `ask` first: an early return before it would leave the closure subscribed to nothing
        // (the dialogs never opened when the drawer was built before its details arrived)
        let a = ask.get()?;
        let k = kind.get_untracked()?;
        let (can_snap, adopted) = (k.1, k.3 == Adoption::Adopted);
        let dir = d.data.with_untracked(|d| d.as_ref().and_then(|d| d.summary.working_dir.clone())).unwrap_or_default();
        Some(ask_dialog(a, n.get_value(), can_snap, adopted, dir, settings, ask, close, d))
    };
    let body = view! {
        {move || d.view(8, content)}
        {dialogs}
    }
    .into_any();
    // a stack rynas manages: Remove left, Edit files right (the PC drawer's footer); the others
    // only Close
    let managed = Memo::new(move |_| kind.get().is_some_and(|k| k.0));
    let foot = (move || {
        if !managed.get() {
            return ui::close_foot(close);
        }
        let edit_btn = view! {
            <button type="button" class="btn ghost" on:click=move |_| edit.set(Some(Some(n.get_value())))><Icon icon=icons::PENCIL size=16 />{tr("dk.a.edit")}</button>
        }
        .into_any();
        ui::drawer_foot(Some(ui::Danger::new("dk.a.remove", Callback::new(move |_| ask.set(Some(Ask::Remove))))), Some(edit_btn), close)
    })
    .into_any();
    ui::drawer(Signal::stored(tf("dk.s.title", &[("name", &name)])), close, body, foot)
}

#[allow(clippy::too_many_arguments)]
fn ask_dialog(a: Ask, name: String, can_snap: bool, adopted: bool, dir: String, settings: Remote<DockerSettingsState>, ask: RwSignal<Option<Ask>>, close_drawer: Callback<()>, d: Remote<StackDetail>) -> AnyView {
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let cancel = Callback::new(move |_| ask.set(None));
    let defaults = settings.data.with_untracked(|s| s.as_ref().map(|s| (s.applied.snapshot_before_update, s.applied.stop_for_snapshot)).unwrap_or((true, false)));
    match a {
        Ask::Adopt => super::adopt::adopt_dialog(name, Callback::new(move |_| { ask.set(None); d.reload(); })),
        Ask::Release => super::adopt::release_dialog(name, cancel, Callback::new(move |_| { ask.set(None); d.reload(); })),
        Ask::Move => {
            let target = settings.data.with_untracked(|s| s.as_ref().and_then(|s| s.stacks_dir.clone())).map(|d| format!("{d}/{name}"));
            super::adopt::move_dialog(name, dir, target, cancel)
        }
        Ask::Update => {
            let snap = RwSignal::new(defaults.0 && can_snap);
            let stop = RwSignal::new(defaults.1);
            let url = stack_url(&name, "/update");
            let go = Callback::new(move |_| {
                let body = UpdateStackRequest { snapshot: Some(snap.get_untracked()), stop_first: Some(stop.get_untracked()) };
                launch(url.clone(), serde_json::to_string(&body).ok(), busy, error, cancel);
            });
            let extra = view! {
                <div class="form">
                    {if can_snap {
                        view! {
                            {switch("dk.upd.snapshot", Some("dk.upd.snapshot_help"), snap.into(), Callback::new(move |v| snap.set(v)))}
                            {switch("dk.upd.stop", Some("dk.set.stop_help"), stop.into(), Callback::new(move |v| stop.set(v)))}
                        }
                        .into_any()
                    } else {
                        ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(t("dk.snap.not_subvolume").to_string()))
                    }}
                </div>
            }
            .into_any();
            ui::confirm(Confirm { extra: Some(extra), busy, error, ..Confirm::new(tf("dk.upd.ask", &[("name", &name)]), t("dk.upd.text").to_string(), "dk.a.update", false, go, cancel) })
        }
        Ask::Remove => {
            let files = RwSignal::new(false);
            let url = stack_url(&name, "/remove");
            let n2 = name.clone();
            let go = Callback::new(move |_| {
                let del = files.get_untracked();
                let body = RemoveStackRequest { delete_files: del, confirm: del.then(|| n2.clone()) };
                launch(url.clone(), serde_json::to_string(&body).ok(), busy, error, Callback::new(move |_| { ask.set(None); close_drawer.run(()); }));
            });
            let typed_name = name.clone();
            // an adopted stack's folder is the user's own: the backend never deletes it
            let extra = if adopted {
                ui::note(Tone::Info, icons::INFO, ui::text(t("dk.rm.adopted").to_string()))
            } else {
                view! {
                    <label class="check">
                        <input type="checkbox" prop:checked=move || files.get() on:change=move |ev| files.set(event_target_checked(&ev)) />
                        <span>{tr("dk.rm.files")}</span>
                    </label>
                }
                .into_any()
            };
            ui::confirm(Confirm {
                extra: Some(extra),
                typed: Signal::derive(move || files.get().then(|| typed_name.clone())),
                busy,
                error,
                ..Confirm::new(tf("dk.rm.title", &[("name", &name)]), t("dk.rm.text").to_string(), "dk.rm.go", true, go, cancel)
            })
        }
        Ask::Rollback(sn) => {
            let url = stack_url(&name, "/rollback");
            let id = sn.id.clone();
            let go = Callback::new(move |_| {
                let body = RollbackRequest { snapshot: id.clone() };
                launch(url.clone(), serde_json::to_string(&body).ok(), busy, error, cancel);
            });
            let when = i18n::datetime(sn.created_s.max(0) as u64 * 1000);
            ui::confirm(Confirm { busy, error, ..Confirm::new(tf("dk.snap.rollback_title", &[("when", &when)]), tf("dk.snap.rollback_text", &[("when", &when)]), "snap.rollback", true, go, cancel) })
        }
        Ask::DeleteSnapshot(sn) => {
            let url = stack_url(&name, &format!("/snapshots/{}", api::encode(&sn.id)));
            let go = Callback::new(move |_| {
                busy.set(true);
                let url = url.clone();
                spawn_local(async move {
                    let r = api::delete_empty(&url).await;
                    let _ = busy.try_set(false);
                    match r {
                        Ok(()) => {
                            ask.set(None);
                            d.reload();
                        }
                        Err(e) => {
                            let _ = error.try_set(Some(e.text()));
                        }
                    }
                });
            });
            let when = i18n::datetime(sn.created_s.max(0) as u64 * 1000);
            ui::confirm(Confirm { busy, error, ..Confirm::new(tf("dk.snap.delete", &[("when", &when)]), tf("dk.snap.delete_text", &[("when", &when)]), "delete", true, go, cancel) })
        }
    }
}

// ------------------------------------------------------------------ automatic updates

const POLICIES: &[(&str, &str)] = &[("", "dk.au.p_default"), ("patch", "dk.au.p_patch"), ("minor", "dk.au.p_minor"), ("calver-patch", "dk.au.p_calver"), ("notify", "dk.au.p_notify")];

/// The stack's line of the update report (an update and what its policy does with it).
fn stack_update(name: String) -> Memo<Option<StackUpdate>> {
    let u = page().updates;
    Memo::new(move |_| u.data.with(|u| u.as_ref().and_then(|u| u.stacks.iter().find(|s| s.stack == name).cloned())))
}

fn reason_text(r: HoldReason) -> &'static str {
    match r {
        HoldReason::Major => "dk.au.r_major",
        HoldReason::Minor => "dk.au.r_minor",
        HoldReason::Month => "dk.au.r_month",
        HoldReason::Prerelease => "dk.au.r_pre",
        HoldReason::Variant => "dk.au.r_variant",
        HoldReason::Downgrade => "dk.au.r_downgrade",
        HoldReason::UnknownVersion => "dk.au.r_unknown",
        HoldReason::Policy => "dk.au.r_policy",
        HoldReason::NeedsReview => "dk.au.r_review",
        HoldReason::NotManaged => "dk.au.r_managed",
    }
}

fn outcome_text(o: AutoOutcome) -> (Tone, &'static str) {
    match o {
        AutoOutcome::Updated => (Tone::Ok, "dk.au.o_updated"),
        AutoOutcome::RolledBack => (Tone::Warn, "dk.au.o_rolled_back"),
        AutoOutcome::Held => (Tone::Info, "dk.au.o_held"),
        AutoOutcome::Failed => (Tone::Err, "dk.au.o_failed"),
    }
}

/// What the policy does with the stack's pending update, in words.
fn verdict_note(u: &StackUpdate) -> AnyView {
    let what = u.change.clone().unwrap_or_else(|| u.images.join(", "));
    let (tone, text) = match (u.verdict, u.enabled) {
        (UpdateVerdict::Hold, _) => (Tone::Info, tf("dk.au.v_hold", &[("what", &what), ("why", t(u.reason.map(reason_text).unwrap_or("dk.au.r_unknown")))])),
        (_, false) => (Tone::Info, tf("dk.au.v_off", &[("what", &what)])),
        (UpdateVerdict::Auto, true) => (Tone::Ok, tf("dk.au.v_auto", &[("what", &what)])),
        (UpdateVerdict::AfterPull, true) => (Tone::Ok, tf("dk.au.v_label", &[("what", &what)])),
    };
    ui::note(tone, icons::DOWNLOAD, ui::text(text))
}

/// The stack's automatic updates: on/off and the policy (stored in the `docker` key, applied
/// at once), what the policy does with the pending update and the last automatic result.
fn auto_section(name: String, settings: Remote<DockerSettingsState>) -> AnyView {
    let updates = page().updates;
    let n = StoredValue::new(name.clone());
    let cur = Memo::new(move |_| settings.data.with(|s| s.as_ref().map(|s| s.pending.as_ref().unwrap_or(&s.applied).auto_update.of(&n.get_value())).unwrap_or_default()));
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let save = move |next: StackAutoUpdate| {
        let Some(st) = settings.data.get_untracked() else { return };
        let mut body = st.pending.clone().unwrap_or_else(|| st.applied.clone());
        // never apply a staged engine move from here
        if body.volume != st.applied.volume {
            error.set(Some(t("dk.au.pending").to_string()));
            return;
        }
        if next.enabled || next.policy.is_some() {
            body.auto_update.stacks.insert(n.get_value(), next);
        } else {
            body.auto_update.stacks.remove(&n.get_value());
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match api::put::<_, DockerSettingsState>("/api/v1/docker/settings", &body).await {
                Ok(_) => {
                    if apply_now(&[keys::DOCKER], error).await {
                        saved_applied();
                    }
                }
                Err(e) => error.set(Some(e.text())),
            }
            let _ = busy.try_set(false);
            settings.reload();
            updates.reload();
        });
    };
    let enabled = Signal::derive(move || cur.get().enabled);
    let policy = Signal::derive(move || cur.get().policy.map(|p| p.as_str().to_string()).unwrap_or_default());
    let plan = stack_update(name.clone());
    let last = Memo::new(move |_| updates.data.with(|u| u.as_ref().and_then(|u| u.auto.iter().find(|a| a.stack == n.get_value()).cloned())));
    let window = Memo::new(move |_| updates.data.with(|u| u.as_ref().map(|u| u.next_window_ms).unwrap_or(0)));
    view! {
        <div class="mini-title">{tr("dk.au.stack")}</div>
        <div class="form">
            {switch("dk.au.enable", Some("dk.au.enable_help"), enabled, Callback::new(move |v| if !busy.get_untracked() { save(StackAutoUpdate { enabled: v, ..cur.get_untracked() }) }))}
            {select_field("dk.au.policy", Some("dk.au.policy_help"), POLICIES, policy, Callback::new(move |v: String| if !busy.get_untracked() { save(StackAutoUpdate { policy: UpdatePolicy::parse(&v), ..cur.get_untracked() }) }))}
            {move || (enabled.get() && window.get() > 0).then(|| view! { <p class="field-help">{tf("dk.au.next", &[("when", &i18n::short_when(window.get()))])}</p> })}
            {move || plan.with(|p| p.as_ref().map(verdict_note))}
            {move || last.with(|l| l.as_ref().map(|l| {
                let (tone, key) = outcome_text(l.outcome);
                let mut text = tf(key, &[("when", &i18n::short_when(l.at_ms))]);
                if let Some(d) = &l.detail {
                    text = format!("{text} {d}");
                }
                ui::note(tone, icons::HISTORY, ui::text(text))
            }))}
            {form_error(error)}
        </div>
    }
    .into_any()
}

// ------------------------------------------------------------------ editor

fn validation_view(v: StackValidation) -> AnyView {
    let mut out: Vec<AnyView> = Vec::new();
    if !v.ok {
        out.push(ui::note(Tone::Err, icons::CIRCLE_X, view! { {t("dk.val.bad")}{v.error.clone().map(ui::tech_detail)} }.into_any()));
        return out.into_any();
    }
    let summary = tf("dk.val.ok", &[("services", &v.services.join(", ")), ("ports", &if v.ports.is_empty() { ui::DASH.to_string() } else { v.ports.join(", ") })]);
    out.push(ui::note(Tone::Ok, icons::CIRCLE_CHECK, ui::text(summary)));
    if !v.port_conflicts.is_empty() {
        out.push(ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(tf("dk.val.ports", &[("ports", &v.port_conflicts.join(", "))]))));
    }
    for b in v.binds.iter().filter(|b| b.outside_volume) {
        out.push(ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(tf("dk.val.outside", &[("path", &b.source), ("service", &b.service)]))));
    }
    let create: Vec<String> = v.binds.iter().filter(|b| !b.exists && !b.outside_volume).map(|b| b.source.clone()).collect();
    if !create.is_empty() {
        out.push(ui::note(Tone::Info, icons::FOLDER_PLUS, ui::text(tf("dk.val.create", &[("paths", &create.join(", "))]))));
    }
    out.into_any()
}

/// Create (`None`) or edit (`Some(name)`) a rynas stack.
fn editor(name: Option<String>, settings: Remote<DockerSettingsState>, close: Callback<()>) -> AnyView {
    let creating = name.is_none();
    let n = RwSignal::new(name.clone().unwrap_or_default());
    let vol = RwSignal::new(settings.data.with_untracked(|s| s.as_ref().and_then(|s| s.applied.volume.clone().or_else(|| s.volumes.first().map(|v| v.label.clone())))).unwrap_or_default());
    let compose = RwSignal::new(String::new());
    let env = RwSignal::new(String::new());
    let compose_file = StoredValue::new(String::new());
    let hash = StoredValue::new(String::new());
    let other_files = RwSignal::new(Vec::<String>::new());
    let read_only = RwSignal::new(false);
    let start = RwSignal::new(true);
    let loaded = RwSignal::new(creating);
    let load_err = RwSignal::new(None::<String>);
    let validation = RwSignal::new(None::<StackValidation>);
    let compose_err = RwSignal::new(None::<String>);
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let checking = RwSignal::new(false);
    let load = move || {
        let name = n.get_untracked();
        spawn_local(async move {
            match api::get::<StackFiles>(&stack_url(&name, "/files")).await {
                Ok(f) => {
                    compose.set(f.compose);
                    env.set(f.env);
                    compose_file.set_value(f.compose_file);
                    hash.set_value(f.hash);
                    other_files.set(f.other_files);
                    read_only.set(f.read_only);
                    loaded.set(true);
                }
                Err(e) if e.status() == 401 => {}
                Err(e) => load_err.set(Some(e.code().to_string())),
            }
        });
    };
    if !creating {
        load();
    }
    let name_err = Signal::derive(move || (creating && !n.with(|v| v.is_empty() || valid_stack_name(v))).then(|| t("dk.ed.name_invalid").to_string()));
    let can_save = Signal::derive(move || !busy.get() && !read_only.get() && compose.with(|c| !c.trim().is_empty()) && (!creating || n.with(|v| valid_stack_name(v))));
    let validate = move |_| {
        checking.set(true);
        error.set(None);
        compose_err.set(None);
        let body = ValidateStackRequest { name: Some(n.get_untracked()).filter(|v| !v.is_empty()), compose: compose.get_untracked(), env: env.get_untracked() };
        spawn_local(async move {
            let r = api::post::<_, StackValidation>("/api/v1/docker/stacks/validate", Some(&body)).await;
            let _ = checking.try_set(false);
            match r {
                Ok(v) => {
                    let _ = validation.try_set(Some(v));
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    };
    let save = Callback::new(move |_| {
        busy.set(true);
        error.set(None);
        compose_err.set(None);
        let name = n.get_untracked();
        let (c, e) = (compose.get_untracked(), env.get_untracked());
        spawn_local(async move {
            let res: Result<Option<String>, api::ApiErr> = if creating {
                let v = vol.get_untracked();
                let body = CreateStackRequest { name: name.clone(), compose: c, env: e, files: Vec::new(), volume: (!v.is_empty()).then_some(v), start: start.get_untracked() };
                api::post::<_, StackCreated>("/api/v1/docker/stacks", Some(&body)).await.map(|r| r.task_id)
            } else {
                let body = StackFiles { compose: c, env: e, compose_file: compose_file.get_value(), hash: hash.get_value(), ..Default::default() };
                api::put::<_, StackValidation>(&stack_url(&name, "/files"), &body).await.map(|_| None)
            };
            let _ = busy.try_set(false);
            match res {
                Ok(task) => {
                    toast(Tone::Ok, t(if creating { "dk.ed.created" } else { "dk.ed.saved" }).to_string());
                    crate::docker::docker().on_changed();
                    close.run(());
                    if let Some(id) = task {
                        tasks().show(&id);
                        done_toast(tasks::follow(&id).await);
                    }
                }
                Err(e) if e.field() == Some("compose") => {
                    let _ = compose_err.try_set(Some(e.text()));
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    });
    let body = move || {
        if !loaded.get() {
            return ui::loading(load_err.into(), Callback::new(move |_| load()), 8);
        }
        view! {
            <div class="form">
                {creating.then(|| view! {
                    <div class="form-row">
                        {text_field("dk.ed.name", n.into(), Callback::new(move |v: String| n.set(v.trim().to_lowercase())), name_err, In { mono: true, help: Some("dk.ed.name_help"), maxlength: 63, ..In::default() })}
                        {super::overview::volume_select("dk.ed.volume", None, settings, vol.into(), Callback::new(move |v| vol.set(v)), None)}
                    </div>
                })}
                {move || read_only.get().then(|| ui::note(Tone::Info, icons::LOCK, ui::text(t("dk.ad.ed_ro").to_string())))}
                {move || other_files.with(|o| (!o.is_empty()).then(|| ui::note(Tone::Info, icons::FILE, ui::text(tf("dk.ad.ed_others", &[("files", &o.join(", "))])))))}
                {code_field("dk.ed.compose", compose.into(), Callback::new(move |v| { compose.set(v); validation.set(None); }), compose_err.into(), 16, (!read_only.get_untracked()).then_some(".yaml,.yml,text/yaml,application/yaml"))}
                {code_field("dk.ed.env", env.into(), Callback::new(move |v| { env.set(v); validation.set(None); }), Signal::stored(None), 5, (!read_only.get_untracked()).then_some(".env,text/plain"))}
                <p class="field-help">{tr("dk.ed.env_help")}</p>
                {creating.then(|| switch("dk.ed.start", None, start.into(), Callback::new(move |v| start.set(v))))}
                {move || validation.get().map(validation_view)}
            </div>
        }
        .into_any()
    };
    let secondary = view! {
        <button type="button" class="btn ghost" disabled=move || checking.get() || !loaded.get() on:click=validate>{spinner(checking)}<Icon icon=icons::CHECK size=16 />{tr("dk.ed.validate")}</button>
    }
    .into_any();
    let foot = save_bar_with(SaveBar {
        can_save,
        error: Some(error),
        secondary: Some(secondary),
        ..SaveBar::new(busy, if creating { "create" } else { "save" }, close, save)
    });
    let title = match &name {
        Some(n) => tf("edit_named", &[("name", n)]),
        None => t("dk.s.create").to_string(),
    };
    ui::drawer(Signal::stored(title), close, view! { {body} }.into_any(), foot)
}
