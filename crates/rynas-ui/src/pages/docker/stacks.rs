//! Docker › Stacks: every compose project, grouped by who manages it (rynas, Portainer,
//! nobody known). rynas stacks (`/<vol>/docker/stacks/<name>/`) can be created, edited
//! (compose.yaml + .env, validated with `docker compose config`), started, pulled, updated
//! with a snapshot first, rolled back to a snapshot and removed; the others only started,
//! stopped and restarted. Image updates come from the digest check (`/docker/updates`, one
//! report for the page). Rows are keyed by stack name and follow the list in place.

use std::collections::HashSet;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::docker::{
    CreateStackRequest, DockerSettingsState, RemoveStackRequest, RollbackRequest, StackCreated, StackDetail, StackFiles, StackManager, StackSnapshot, StackState, StackSummary,
    StackValidation, UpdateStackRequest, ValidateStackRequest, valid_stack_name,
};

use super::{Busy, act, action_url, container_pill, done_toast, empty, image_ref, manager_key, page, remote, row_btn, update_chip, updatable};
use crate::api;
use crate::form::{In, code_field, form_error, search_box, spinner, switch, text_field};
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
    let checking = RwSignal::new(false);
    let checked = move || {
        updates.data.with(|u| u.as_ref().map(|u| if u.checked_at_ms == 0 { t("dk.upd.never").to_string() } else { tf("dk.upd.checked", &[("when", &i18n::short_when(u.checked_at_ms))]) }))
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
                    {spinner(checking)}<Icon icon=icons::REFRESH_CW size=16 /><span class="btn-label">{tr("dk.upd.check")}</span>
                </button>
                <button class="btn primary" on:click=move |_| edit.set(Some(None))>
                    <Icon icon=icons::PLUS size=16 /><span class="btn-label">{tr("dk.s.create")}</span>
                </button>
            </div>
            {move || list.view(6, groups)}
        </section>
        {move || detail.get().map(|n| detail_drawer(n, settings, edit, Callback::new(move |_| detail.set(None))))}
        {move || edit.get().map(|n| editor(n, settings, Callback::new(move |_| edit.set(None))))}
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
    let has_update = Memo::new(move |_| images.with(|i| cx.upd.with(|u| i.iter().any(|x| u.contains(x)))));
    let can_start = Signal::derive(move || state.with(|s| s.is_some_and(|s| s.0 != StackState::Running)));
    let can_stop = Signal::derive(move || state.with(|s| s.is_some_and(|s| s.2 > 0)));
    let start = if first.editable { "up" } else { "start" };
    let open_label = format!("{}: {}", t("dk.a.details"), name);
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
            ui::cell("dk.col.running", "num", (move || state.get().map(|s| format!("{} / {}", s.2, s.3))).into_any()),
            ui::cell("dk.col.images", "wrap", view! {
                <div class="cell-main">
                    {move || images.with(|l| l.iter().map(|i| image_ref(i)).collect_view())}
                    {move || has_update.get().then(update_chip)}
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
        return view! { <p class="muted small">{tr("dk.s.no_containers")}</p> }.into_any();
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
        return view! { <p class="muted small">{tr("dk.snap.none")}</p> }.into_any();
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
                    <button class="btn ghost sm" on:click=move |_| ask.set(Some(Ask::Rollback(a.clone())))><Icon icon=icons::ROTATE_CCW size=14 />{tr("dk.snap.rollback")}</button>
                    <button class="icon-btn sm" title=tr("delete") aria-label=format!("{}: {}", t("delete"), sn.id) on:click=move |_| ask.set(Some(Ask::DeleteSnapshot(b.clone())))><Icon icon=icons::TRASH_2 size=16 /></button>
                }.into_any()),
            ])
        })
        .collect::<Vec<_>>();
    ui::table("", &["dk.col.created", "dk.col.images", ""], rows)
}

/// The drawer's action bar: built once per stack kind, so a refresh of the details keeps the
/// buttons (and the focus on them).
fn action_bar(managed: bool, n: StoredValue<String>, busy: RwSignal<bool>, ask: RwSignal<Option<Ask>>, edit: RwSignal<Option<Option<String>>>) -> AnyView {
    let task = move |action: &'static str| Callback::new(move |_| act(action_url("stacks", &n.get_value(), action), None, busy));
    let idle = RwSignal::new(false);
    if managed {
        view! {
            <div class="sec-actions">
                {bar_btn(icons::PLAY, "dk.a.up", busy, task("up"))}
                {bar_btn(icons::REFRESH_CW, "dk.a.restart", busy, task("restart"))}
                {bar_btn(icons::SQUARE, "dk.a.stop", busy, task("stop"))}
                {bar_btn(icons::DOWNLOAD, "dk.a.pull", busy, task("pull"))}
                {bar_btn(icons::DOWNLOAD, "dk.a.update", idle, Callback::new(move |_| ask.set(Some(Ask::Update))))}
                {bar_btn(icons::CIRCLE_X, "dk.a.down", busy, task("down"))}
                {bar_btn(icons::PENCIL, "dk.a.edit", idle, Callback::new(move |_| edit.set(Some(Some(n.get_value())))))}
                <button class="btn ghost sm danger-text" on:click=move |_| ask.set(Some(Ask::Remove))><Icon icon=icons::TRASH_2 size=14 />{tr("dk.a.remove")}</button>
            </div>
        }
        .into_any()
    } else {
        view! {
            <div class="sec-actions">
                {bar_btn(icons::PLAY, "dk.a.start", busy, task("start"))}
                {bar_btn(icons::SQUARE, "dk.a.stop", busy, task("stop"))}
                {bar_btn(icons::REFRESH_CW, "dk.a.restart", busy, task("restart"))}
            </div>
        }
        .into_any()
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
    let kind = Memo::new(move |_| d.data.with(|d| d.as_ref().map(|d| (d.summary.editable, d.summary.snapshots, d.summary.manager))));
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
        let Some((managed, can_snap, manager)) = kind.get() else { return ().into_any() };
        let summary = Memo::new(move |_| d.data.with(|d| d.as_ref().map(|d| d.summary.clone())));
        let new_images = move || {
            summary.with(|s| s.as_ref().map(|s| s.images.iter().filter(|i| upd.with(|u| u.contains(i))).cloned().collect::<Vec<_>>()).unwrap_or_default())
        };
        view! {
            <div class="chips">
                <span class="chip">{t(manager_key(manager))}</span>
                {move || summary.with(|s| s.as_ref().map(|s| state_pill(s.state)))}
            </div>
            {move || summary.with(|s| s.as_ref().map(|s| view! {
                <dl class="kvs">
                    {s.working_dir.clone().map(|w| kv_row("dk.s.folder", Signal::stored(w), "mono"))}
                    {(!s.config_files.is_empty()).then(|| kv_row("dk.s.files", Signal::stored(s.config_files.join(", ")), "mono"))}
                </dl>
            }))}
            {(!managed).then(|| ui::note(Tone::Info, icons::INFO, ui::text(t(if manager == StackManager::Rynas { "dk.s.no_folder" } else { "dk.s.readonly" }).to_string())))}
            {move || { let l = new_images(); (!l.is_empty()).then(|| ui::note(Tone::Info, icons::DOWNLOAD, ui::text(tf("dk.upd.stack", &[("images", &l.join(", "))])))) }}
            {action_bar(managed, n, busy, ask, edit)}
            <div class="mini-title">{tr("dk.s.containers")}</div>
            {move || d.data.with(|d| d.as_ref().map(stack_containers))}
            {managed.then(|| view! {
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
        let can_snap = kind.get_untracked()?.1;
        Some(ask_dialog(a, n.get_value(), can_snap, settings, ask, close, d))
    };
    let body = view! {
        {move || d.view(8, content)}
        {dialogs}
    }
    .into_any();
    let foot = view! { <button type="button" class="btn primary" on:click=move |_| close.run(())>{tr("close")}</button> }.into_any();
    ui::drawer(Signal::stored(tf("dk.s.title", &[("name", &name)])), close, body, foot)
}

fn ask_dialog(a: Ask, name: String, can_snap: bool, settings: Remote<DockerSettingsState>, ask: RwSignal<Option<Ask>>, close_drawer: Callback<()>, d: Remote<StackDetail>) -> AnyView {
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let cancel = Callback::new(move |_| ask.set(None));
    let defaults = settings.data.with_untracked(|s| s.as_ref().map(|s| (s.applied.snapshot_before_update, s.applied.stop_for_snapshot)).unwrap_or((true, false)));
    match a {
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
            ui::confirm(Confirm { extra: Some(extra), busy, error, ..Confirm::new(tf("dk.upd.title", &[("name", &name)]), t("dk.upd.text").to_string(), "dk.a.update", false, go, cancel) })
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
            let extra = view! {
                <label class="check">
                    <input type="checkbox" prop:checked=move || files.get() on:change=move |ev| files.set(event_target_checked(&ev)) />
                    <span>{tr("dk.rm.files")}</span>
                </label>
            }
            .into_any();
            ui::confirm(Confirm {
                extra: Some(extra),
                typed: Signal::derive(move || files.get().then(|| typed_name.clone())),
                busy,
                error,
                ..Confirm::new(tf("dk.rm.title", &[("name", &name)]), t("dk.rm.text").to_string(), "dk.a.remove", true, go, cancel)
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
            ui::confirm(Confirm { busy, error, ..Confirm::new(t("dk.snap.rollback").to_string(), tf("dk.snap.rollback_text", &[("when", &when)]), "dk.snap.rollback", true, go, cancel) })
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
            ui::confirm(Confirm { busy, error, ..Confirm::new(t("dk.snap.delete").to_string(), tf("dk.snap.delete_text", &[("when", &when)]), "delete", true, go, cancel) })
        }
    }
}

// ------------------------------------------------------------------ editor

fn validation_view(v: StackValidation) -> AnyView {
    let mut out: Vec<AnyView> = Vec::new();
    if !v.ok {
        out.push(ui::note(Tone::Err, icons::CIRCLE_X, view! { {t("dk.val.bad")}{v.error.clone().map(ui::tech_detail)} }.into_any()));
        return out.into_any();
    }
    let summary = tf("dk.val.ok", &[("services", &v.services.join(", ")), ("ports", &if v.ports.is_empty() { "—".to_string() } else { v.ports.join(", ") })]);
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
    let can_save = Signal::derive(move || !busy.get() && compose.with(|c| !c.trim().is_empty()) && (!creating || n.with(|v| valid_stack_name(v))));
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
                let body = StackFiles { compose: c, env: e, compose_file: compose_file.get_value() };
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
                {code_field("dk.ed.compose", compose.into(), Callback::new(move |v| { compose.set(v); validation.set(None); }), compose_err.into(), 16)}
                {code_field("dk.ed.env", env.into(), Callback::new(move |v| { env.set(v); validation.set(None); }), Signal::stored(None), 5)}
                <p class="field-help">{tr("dk.ed.env_help")}</p>
                {creating.then(|| switch("dk.ed.start", None, start.into(), Callback::new(move |v| start.set(v))))}
                {move || validation.get().map(validation_view)}
            </div>
        }
        .into_any()
    };
    let foot = view! {
        {form_error(error)}
        <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("cancel")}</button>
        <button type="button" class="btn ghost" disabled=move || checking.get() || !loaded.get() on:click=validate>{spinner(checking)}<Icon icon=icons::CHECK size=16 />{tr("dk.ed.validate")}</button>
        <button type="button" class="btn primary" disabled=move || !can_save.get() on:click=move |_| save.run(())>{spinner(busy)}{tr(if creating { "create" } else { "save" })}</button>
    }
    .into_any();
    let title = match &name {
        Some(n) => tf("edit_named", &[("name", n)]),
        None => t("dk.s.create").to_string(),
    };
    ui::drawer(Signal::stored(title), close, view! { {body} }.into_any(), foot)
}
