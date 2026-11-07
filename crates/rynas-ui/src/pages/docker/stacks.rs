//! Docker › Stacks: every compose project, grouped by who manages it (rynas, Portainer,
//! nobody known). rynas stacks (`/<vol>/docker/stacks/<name>/`) can be created, edited
//! (compose.yaml + .env, validated with `docker compose config`), started, pulled, updated
//! with a snapshot first, rolled back to a snapshot and removed; the others only started,
//! stopped and restarted. Image updates come from the digest check (`/docker/updates`).

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::docker::{
    CreateStackRequest, DockerSettingsState, RemoveStackRequest, RollbackRequest, StackCreated, StackDetail, StackFiles, StackManager, StackSnapshot, StackState, StackSummary,
    StackValidation, UpdateReport, UpdateStackRequest, ValidateStackRequest, valid_stack_name,
};

use super::{act, action_url, container_pill, done_toast, manager_key, remote};
use crate::api;
use crate::form::{In, code_field, form_error, search_box, spinner, switch, text_field};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::tasks::{self, tasks};
use crate::toast::toast;
use crate::ui::{self, Confirm, EmptyState, Remote, Tone, kv_row};

const SETTINGS: &str = "/api/v1/docker/settings";
const GROUPS: [(StackManager, &str); 3] = [(StackManager::Rynas, "dk.g.rynas"), (StackManager::Portainer, "dk.g.portainer"), (StackManager::Untracked, "dk.g.untracked")];

fn state_pill(s: StackState) -> AnyView {
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

/// Images of the update report with a newer registry digest.
fn updatable(u: Remote<UpdateReport>) -> Vec<String> {
    u.data.with(|u| u.as_ref().map(|u| u.images.iter().filter(|i| i.update_available).map(|i| i.image.clone()).collect()).unwrap_or_default())
}

/// Starts an action from a dialog: a refusal stays in the dialog (`error`); once started the
/// dialog closes (`started`), the task panel shows the log and a toast the end.
fn launch(url: String, body: Option<String>, busy: RwSignal<bool>, error: RwSignal<Option<String>>, started: Callback<()>) {
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

pub fn panel() -> AnyView {
    let list = remote::<Vec<StackSummary>>("/api/v1/docker/stacks");
    let updates = remote::<UpdateReport>("/api/v1/docker/updates");
    let settings = Remote::<DockerSettingsState>::with_config(SETTINGS);
    let q = RwSignal::new(String::new());
    let detail = RwSignal::new(None::<String>);
    let edit = RwSignal::new(None::<Option<String>>);
    let checking = RwSignal::new(false);
    let checked = move || {
        updates.data.with(|u| u.as_ref().map(|u| if u.checked_at_ms == 0 { t("dk.upd.never").to_string() } else { tf("dk.upd.checked", &[("when", &i18n::short_when(u.checked_at_ms))]) }))
    };
    let groups = move || {
        let all = list.data.get().unwrap_or_default();
        if all.is_empty() {
            return view! { <EmptyState icon=icons::LAYERS title=Signal::derive(|| t("dk.s.none").to_string()) text=Signal::derive(|| t("dk.s.none_text").to_string()) /> }.into_any();
        }
        let qv = q.get().trim().to_lowercase();
        let upd = updatable(updates);
        GROUPS
            .iter()
            .filter_map(|(m, key)| {
                let rows: Vec<AnyView> =
                    all.iter().filter(|s| s.manager == *m && (qv.is_empty() || s.name.contains(&qv))).map(|s| { let u = s.images.iter().any(|i| upd.contains(i)); row(s.clone(), u, detail) }).collect();
                (!rows.is_empty()).then(|| {
                    view! {
                        <h3 class="mini-title dk-group">{tr(key)}</h3>
                        {ui::table("dk-table", &["dk.col.stack", "col.status", "#dk.col.running", "dk.col.images", ""], rows)}
                    }
                    .into_any()
                })
            })
            .collect_view()
            .into_any()
    };
    view! {
        <section class="card widget w-12">
            <div class="toolbar">
                {search_box("dk.s.search", q)}
                <span class="small muted">{checked}</span>
                <button class="btn ghost" disabled=move || checking.get() on:click=move |_| act("/api/v1/docker/updates/check".into(), None, checking)>
                    {crate::form::spinner(checking)}<Icon icon=icons::REFRESH_CW size=16 /><span class="btn-label">{tr("dk.upd.check")}</span>
                </button>
                <button class="btn primary" on:click=move |_| edit.set(Some(None))>
                    <Icon icon=icons::PLUS size=16 /><span class="btn-label">{tr("dk.s.create")}</span>
                </button>
            </div>
            {move || list.view(6, groups)}
        </section>
        {move || detail.get().map(|n| detail_drawer(n, settings, updates, edit, Callback::new(move |_| detail.set(None))))}
        {move || edit.get().map(|n| editor(n, settings, Callback::new(move |_| edit.set(None))))}
    }
    .into_any()
}

fn row(s: StackSummary, update: bool, detail: RwSignal<Option<String>>) -> AnyView {
    let busy = RwSignal::new(false);
    let managed = s.editable;
    let name = s.name.clone();
    let btn = move |icon: &'static str, key: &'static str, action: &'static str| {
        let (n, label) = (name.clone(), format!("{}: {}", t(key), name));
        view! {
            <button class="icon-btn sm" title=tr(key) aria-label=label disabled=move || busy.get()
                on:click=move |_| act(action_url("stacks", &n, action), None, busy)>
                <Icon icon=icon size=16 />
            </button>
        }
    };
    let start = if managed { "up" } else { "start" };
    let open = s.name.clone();
    let open_label = format!("{}: {}", t("dk.a.details"), s.name);
    ui::row(vec![
        ui::cell("dk.col.stack", "", view! {
            <div class="cell-main">
                <button class="link-strong" on:click={ let n = open.clone(); move |_| detail.set(Some(n.clone())) }>{s.name.clone()}</button>
                {s.working_dir.clone().map(|w| view! { <span class="cell-sub mono">{w}</span> })}
            </div>
        }.into_any()),
        ui::cell("col.status", "", view! {
            <div class="chips">
                {state_pill(s.state)}
                {(s.unhealthy > 0).then(|| ui::status(Tone::Err, tf("dk.s.unhealthy_n", &[("n", &s.unhealthy.to_string())])))}
            </div>
        }.into_any()),
        ui::cell("dk.col.running", "num", ui::text(format!("{} / {}", s.running, s.containers))),
        ui::cell("dk.col.images", "wrap", view! {
            <div class="cell-main">
                {s.images.iter().map(|i| view! { <span class="mono small">{i.clone()}</span> }).collect_view()}
                {update.then(|| view! { <span class="chip accent"><Icon icon=icons::DOWNLOAD size=12 />{tr("dk.upd.available")}</span> })}
            </div>
        }.into_any()),
        ui::actions(view! {
            {(s.state != StackState::Running).then(|| btn(icons::PLAY, "dk.a.start", start))}
            {(s.running > 0).then(|| view! { {btn(icons::SQUARE, "dk.a.stop", "stop")}{btn(icons::REFRESH_CW, "dk.a.restart", "restart")} })}
            <button class="icon-btn sm" title=tr("dk.a.details") aria-label=open_label on:click=move |_| detail.set(Some(open.clone()))>
                <Icon icon=icons::CHEVRON_RIGHT size=16 />
            </button>
        }.into_any()),
    ])
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

fn reason_key(r: &str) -> &'static str {
    match r {
        "update" => "dk.snap.r_update",
        "pull" => "dk.snap.r_pull",
        "before-rollback" => "dk.snap.r_rollback",
        _ => "dk.snap.r_manual",
    }
}

fn detail_drawer(name: String, settings: Remote<DockerSettingsState>, updates: Remote<UpdateReport>, edit: RwSignal<Option<Option<String>>>, close: Callback<()>) -> AnyView {
    let d = remote::<StackDetail>(&stack_url(&name, ""));
    let busy = RwSignal::new(false);
    let ask = RwSignal::new(None::<Ask>);
    let snap_busy = RwSignal::new(false);
    let snap_err = RwSignal::new(None::<String>);
    let n = StoredValue::new(name.clone());
    let content = move || {
        let Some(det) = d.data.get() else { return ().into_any() };
        let s = det.summary.clone();
        let managed = s.editable;
        let upd = updatable(updates);
        let new_images: Vec<String> = s.images.iter().filter(|i| upd.contains(i)).cloned().collect();
        let btn = move |icon: &'static str, key: &'static str, action: &'static str| {
            view! {
                <button class="btn ghost sm" disabled=move || busy.get() on:click=move |_| act(action_url("stacks", &n.get_value(), action), None, busy)>
                    <Icon icon=icon size=14 />{tr(key)}
                </button>
            }
        };
        let containers = det
            .containers
            .iter()
            .map(|c| {
                ui::row(vec![
                    ui::cell("dk.col.name", "", view! { <div class="cell-main"><strong>{c.name.clone()}</strong><span class="cell-sub mono">{c.image.clone()}</span></div> }.into_any()),
                    ui::cell("col.status", "", container_pill(c.state, c.health)),
                ])
            })
            .collect::<Vec<_>>();
        let snaps = det
            .snapshots
            .iter()
            .map(|sn| {
                let (a, b) = (sn.clone(), sn.clone());
                let imgs = sn.images.iter().map(|i| format!("{}: {}", i.service, i.image)).collect::<Vec<_>>();
                ui::row(vec![
                    ui::cell("dk.col.created", "", view! { <div class="cell-main"><span>{i18n::datetime(sn.created_s.max(0) as u64 * 1000)}</span><span class="cell-sub">{t(reason_key(&sn.reason))}</span></div> }.into_any()),
                    ui::cell("dk.col.images", "wrap", view! { <div class="cell-main">{imgs.into_iter().map(|i| view! { <span class="mono small">{i}</span> }).collect_view()}</div> }.into_any()),
                    ui::actions(view! {
                        <button class="btn ghost sm" on:click=move |_| ask.set(Some(Ask::Rollback(a.clone())))><Icon icon=icons::ROTATE_CCW size=14 />{tr("dk.snap.rollback")}</button>
                        <button class="icon-btn sm" title=tr("delete") aria-label=format!("{}: {}", t("delete"), sn.id) on:click=move |_| ask.set(Some(Ask::DeleteSnapshot(b.clone())))><Icon icon=icons::TRASH_2 size=16 /></button>
                    }.into_any()),
                ])
            })
            .collect::<Vec<_>>();
        let has_snap_rows = !snaps.is_empty();
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
        view! {
            <div class="chips">
                <span class="chip">{t(manager_key(s.manager))}</span>
                {state_pill(s.state)}
            </div>
            <dl class="kvs">
                {s.working_dir.clone().map(|w| kv_row("dk.s.folder", Signal::stored(w), "mono"))}
                {(!s.config_files.is_empty()).then(|| kv_row("dk.s.files", Signal::stored(s.config_files.join(", ")), "mono"))}
            </dl>
            {(!managed).then(|| ui::note(Tone::Info, icons::INFO, ui::text(t(if s.manager == StackManager::Rynas { "dk.s.no_folder" } else { "dk.s.readonly" }).to_string())))}
            {(!new_images.is_empty()).then(|| ui::note(Tone::Info, icons::DOWNLOAD, ui::text(tf("dk.upd.stack", &[("images", &new_images.join(", "))]))))}
            <div class="sec-actions">
                {if managed {
                    view! {
                        {btn(icons::PLAY, "dk.a.up", "up")}
                        {btn(icons::REFRESH_CW, "dk.a.restart", "restart")}
                        {btn(icons::SQUARE, "dk.a.stop", "stop")}
                        {btn(icons::DOWNLOAD, "dk.a.pull", "pull")}
                        <button class="btn ghost sm" on:click=move |_| ask.set(Some(Ask::Update))><Icon icon=icons::DOWNLOAD size=14 />{tr("dk.a.update")}</button>
                        {btn(icons::CIRCLE_X, "dk.a.down", "down")}
                        <button class="btn ghost sm" on:click=move |_| edit.set(Some(Some(n.get_value())))><Icon icon=icons::PENCIL size=14 />{tr("dk.a.edit")}</button>
                        <button class="btn ghost sm danger-text" on:click=move |_| ask.set(Some(Ask::Remove))><Icon icon=icons::TRASH_2 size=14 />{tr("dk.a.remove")}</button>
                    }
                    .into_any()
                } else {
                    view! {
                        {btn(icons::PLAY, "dk.a.start", "start")}
                        {btn(icons::SQUARE, "dk.a.stop", "stop")}
                        {btn(icons::REFRESH_CW, "dk.a.restart", "restart")}
                    }
                    .into_any()
                }}
            </div>
            <div class="mini-title">{tr("dk.s.containers")}</div>
            {if containers.is_empty() { view! { <p class="muted small">{tr("dk.s.no_containers")}</p> }.into_any() } else { ui::table("", &["dk.col.name", "col.status"], containers) }}
            {managed.then(|| view! {
                <div class="dk-snap-head">
                    <div class="mini-title">{tr("dk.snap.title")}</div>
                    {s.snapshots.then(|| view! {
                        <button class="btn ghost sm" disabled=move || snap_busy.get() on:click=take_snapshot>{spinner(snap_busy)}<Icon icon=icons::CAMERA size=14 />{tr("dk.snap.take")}</button>
                    })}
                </div>
                {form_error(snap_err)}
                {if !s.snapshots {
                    ui::note(Tone::Info, icons::INFO, ui::text(t("dk.snap.not_subvolume").to_string()))
                } else if has_snap_rows {
                    ui::table("", &["dk.col.created", "dk.col.images", ""], snaps)
                } else {
                    view! { <p class="muted small">{tr("dk.snap.none")}</p> }.into_any()
                }}
            })}
        }
        .into_any()
    };
    let dialogs = move || {
        let det = d.data.get_untracked()?;
        ask.get().map(|a| ask_dialog(a, n.get_value(), det.summary.snapshots, settings, ask, close, d))
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
