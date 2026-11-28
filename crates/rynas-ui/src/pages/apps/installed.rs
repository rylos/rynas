//! Installed and Updates: the apps installed from the catalogue (stacks with an `x-rynas`
//! block), their state from Docker, and what can be done with them: open, update with a plan
//! and a snapshot, roll back, move behind the proxy, convert to a custom stack, uninstall.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::apps::{AppRollbackRequest, AppUninstallRequest, AppUpdateManyRequest, AppUpdatePlan, AppUpdateRequest, ImageChangeKind, InstalledApp, InstalledAppDetail, PhotosKeyRequest, PhotosLibrary, PhotosState, PhotosUserRequest};
use rynas_api_types::docker::{DockerSettingsState, StackSnapshot};

use super::{AppDetail, app_icon, apps, open_href};
use crate::api;
use crate::form::{SaveBar, save_bar_with, switch};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::pages::docker::{launch, reason_key, state_pill};
use crate::schema_form::{self, SchemaForm, Values};
use crate::toast::toast;
use crate::ui::{self, Confirm, DlgKind, EmptyState, Remote, Tone};

fn inst_url(stack: &str, rest: &str) -> String {
    format!("/api/v1/apps/installed/{}{rest}", api::encode(stack))
}

fn icon_of(app: &InstalledApp) -> AnyView {
    if app.in_catalog {
        app_icon(format!("/api/v1/apps/catalog/{}/icon", api::encode(&app.app)), 32)
    } else {
        view! { <span class="app-icon app-icon-none"><Icon icon=icons::BOX size=18 /></span> }.into_any()
    }
}

fn state_view(a: &InstalledApp) -> AnyView {
    match a.state {
        Some(s) => state_pill(s),
        None => ui::status(Tone::Unknown, t("apps.inst.engine_down").to_string()),
    }
}

fn open_link(a: &InstalledApp) -> Option<AnyView> {
    open_href(a).map(|href| {
        let label = ui::verb_name("dk.open_web", &a.name);
        view! { <a class="chip accent" href=href target="_blank" rel="noopener noreferrer" aria-label=label><Icon icon=icons::EXTERNAL_LINK size=12 />{tr("dk.open_web")}</a> }.into_any()
    })
}

fn bind_chip(a: &InstalledApp) -> Option<AnyView> {
    a.bind.as_deref().map(|b| view! { <span class="chip">{tr(if b == "127.0.0.1" { "apps.bind.proxy" } else { "apps.bind.lan" })}</span> }.into_any())
}

fn no_docker() -> AnyView {
    view! { <EmptyState icon=icons::CONTAINER title=Signal::derive(|| t("apps.inst.no_docker").to_string()) text=Signal::derive(|| t("apps.docker_first").to_string()) /> }.into_any()
}

/// Installed: every app stack, with its state and the update badge.
pub fn panel() -> AnyView {
    let a = apps();
    let update = RwSignal::new(None::<String>);
    let content = move || {
        let list = a.inst.data.get().unwrap_or_default();
        if list.is_empty() {
            return view! {
                <EmptyState icon=icons::BOX title=Signal::derive(|| t("apps.inst.none").to_string()) text=Signal::derive(|| t("apps.inst.none_text").to_string())>
                    <button type="button" class="btn primary" on:click=move |_| a.tab.set(1)>{tr("apps.browse_all")}</button>
                </EmptyState>
            }
            .into_any();
        }
        let rows = list
            .into_iter()
            .map(|x| {
                let (s1, s2, s3) = (x.stack.clone(), x.stack.clone(), x.stack.clone());
                let label = ui::verb_name("apps.details", &x.name);
                let up_label = ui::verb_name("apps.update", &x.name);
                ui::row(vec![
                    ui::cell("apps.col.app", "", view! {
                        <div class="app-row-name">
                            {icon_of(&x)}
                            <div class="cell-main">
                                <button class="link-strong" on:click=move |_| a.manage.set(Some(s1.clone()))>{x.name.clone()}</button>
                                <span class="cell-sub mono">{x.stack.clone()}</span>
                            </div>
                        </div>
                    }.into_any()),
                    ui::cell("col.status", "", view! { <div class="chips">{state_view(&x)}<span class="small muted tnum">{i18n::of(&x.running.to_string(), &x.containers.to_string())}</span></div> }.into_any()),
                    ui::cell("apps.col.version", "", view! {
                        <div class="cell-main">
                            <span class="tnum">{x.app_version.clone()}</span>
                            {x.update_available.then(|| view! { <span class="chip accent"><Icon icon=icons::DOWNLOAD size=12 />{tr("dk.upd.available")}</span> })}
                            {(!x.in_catalog).then(|| view! { <span class="cell-sub">{tr("apps.inst.left")}</span> })}
                        </div>
                    }.into_any()),
                    ui::cell("apps.col.access", "", view! { <div class="chips">{open_link(&x)}{bind_chip(&x)}</div> }.into_any()),
                    ui::actions(view! {
                        {x.update_available.then(|| view! {
                            <button class="icon-btn sm" title=tr("apps.update") aria-label=up_label on:click=move |_| update.set(Some(s3.clone()))><Icon icon=icons::DOWNLOAD size=16 /></button>
                        })}
                        <button class="icon-btn sm" title=tr("apps.details") aria-label=label on:click=move |_| a.manage.set(Some(s2.clone()))><Icon icon=icons::CHEVRON_RIGHT size=16 /></button>
                    }.into_any()),
                ])
            })
            .collect::<Vec<_>>();
        ui::table("app-table", &["apps.col.app", "col.status", "apps.col.version", "apps.col.access", ""], rows)
    };
    let installed = Memo::new(move |_| a.cat.data.with(|c| c.as_ref().map(|c| c.docker_installed)));
    view! {
        <section class="card widget w-12">
            {move || match installed.get() {
                None => a.cat.view(6, || ().into_any()),
                Some(false) => no_docker(),
                Some(true) => a.inst.view(6, content),
            }}
        </section>
        {move || update.get().map(|s| update_dialog(s, Callback::new(move |_| update.set(None))))}
    }
    .into_any()
}

/// Updates: the installed apps with a newer catalogue version, one by one or all selected.
pub fn updates() -> AnyView {
    let a = apps();
    let update = RwSignal::new(None::<String>);
    let picked = RwSignal::new(Vec::<String>::new());
    let ask = RwSignal::new(false);
    // "Update selected" runs on the server: shown here whoever started it, also after a
    // reload; its end reloads the list and says how it went
    let job = crate::tasks::use_task("apps.update-many", Signal::stored(None));
    job.on_end(move |task| {
        a.reload();
        if task.is_full_success() {
            toast(Tone::Ok, t("apps.upd.all_done").to_string());
        } else if task.is_partial() {
            crate::toast::toast_task(Tone::Warn, t("apps.upd.partly").to_string(), task.id);
        } else {
            crate::toast::toast_task(crate::tasks::task_tone(&task), crate::tasks::task_error(&task), task.id);
        }
    });
    let pending = Memo::new(move |_| a.inst.data.with(|l| l.as_ref().map(|l| l.iter().filter(|x| x.update_available).cloned().collect::<Vec<_>>()).unwrap_or_default()));
    let content = move || {
        let list = pending.get();
        if list.is_empty() {
            return view! { <EmptyState icon=icons::CIRCLE_CHECK title=Signal::derive(|| t("apps.upd.none").to_string()) text=Signal::derive(|| t("apps.upd.none_text").to_string()) /> }.into_any();
        }
        let rows = list
            .into_iter()
            .map(|x| {
                let (s1, s2) = (x.stack.clone(), x.stack.clone());
                let on = Signal::derive(move || picked.with(|p| p.contains(&s1)));
                let label = ui::verb_name("apps.upd.pick", &x.name);
                ui::row(vec![
                    ui::cell("apps.upd.pick", "", view! {
                        <input type="checkbox" class="row-check" aria-label=label prop:checked=on on:change=move |ev| {
                            let s = s2.clone();
                            let v = event_target_checked(&ev);
                            picked.update(|p| { p.retain(|x| *x != s); if v { p.push(s); } });
                        } />
                    }.into_any()),
                    ui::cell("apps.col.app", "", view! { <div class="app-row-name">{icon_of(&x)}<div class="cell-main"><strong>{x.name.clone()}</strong><span class="cell-sub mono">{x.stack.clone()}</span></div></div> }.into_any()),
                    ui::cell("apps.col.version", "", view! { <span class="tnum">{format!("{} → {}", x.version, x.catalog_version.clone().unwrap_or_default())}</span> }.into_any()),
                    ui::actions({
                        let s = x.stack.clone();
                        view! { <button class="btn ghost sm" on:click=move |_| update.set(Some(s.clone()))><Icon icon=icons::DOWNLOAD size=14 />{tr("apps.update")}</button> }.into_any()
                    }),
                ])
            })
            .collect::<Vec<_>>();
        view! {
            <div class="toolbar">
                <span class="small muted">{move || tf("apps.upd.count", &[("n", &pending.with(Vec::len).to_string())])}</span>
                {move || job.task.get().filter(|t| !t.status.is_finished()).map(|t| {
                    let id = t.id.clone();
                    view! {
                        <span class="small" role="status"><span class="btn-spinner" aria-hidden="true"></span>" "{tr("apps.upd.running")}{t.progress.map(|p| format!(" · {}", i18n::pct(p as f64)))}</span>
                        <button class="btn ghost sm" on:click=move |_| crate::tasks::tasks().show(&id)>{tr("tasks.log_open")}</button>
                    }
                })}
                <button class="btn primary" disabled=move || picked.with(Vec::is_empty) || job.running.get() on:click=move |_| ask.set(true)>
                    <Icon icon=icons::DOWNLOAD size=16 /><span class="btn-label">{tr("apps.upd.selected")}</span>
                </button>
            </div>
            {ui::table("app-table", &["apps.upd.pick", "apps.col.app", "apps.col.version", ""], rows)}
        }
        .into_any()
    };
    let installed = Memo::new(move |_| a.cat.data.with(|c| c.as_ref().map(|c| c.docker_installed)));
    view! {
        <section class="card widget w-12">
            {move || match installed.get() {
                None => a.cat.view(6, || ().into_any()),
                Some(false) => no_docker(),
                Some(true) => a.inst.view(6, content),
            }}
        </section>
        {move || update.get().map(|s| update_dialog(s, Callback::new(move |_| update.set(None))))}
        {move || ask.get().then(|| update_many(picked, Callback::new(move |_| ask.set(false))))}
    }
    .into_any()
}

/// "Update selected": one server task `apps.update-many` (the default snapshot setting for
/// each, one after the other; an app whose update needs answers is skipped, left for its own
/// dialog). The dialog closes once the task started; the list follows it ([`updates`]).
fn update_many(picked: RwSignal<Vec<String>>, close: Callback<()>) -> AnyView {
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let names = picked.get_untracked().join(", ");
    let go = Callback::new(move |_| {
        busy.set(true);
        error.set(None);
        let body = serde_json::to_string(&AppUpdateManyRequest { stacks: picked.get_untracked() }).ok();
        spawn_local(async move {
            let r = api::start_task("/api/v1/apps/update-many", body).await;
            let _ = busy.try_set(false);
            match r {
                Ok(_) => {
                    let _ = picked.try_set(Vec::new());
                    close.try_run(());
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    });
    ui::confirm(Confirm { busy, error, ..Confirm::new(t("apps.upd.selected_title").to_string(), tf("apps.upd.many_text", &[("names", &names)]), "apps.update", false, go, close) })
}

// ------------------------------------------------------------------ update dialog

fn change_key(k: ImageChangeKind) -> &'static str {
    match k {
        ImageChangeKind::Added => "apps.chg.added",
        ImageChangeKind::Removed => "apps.chg.removed",
        ImageChangeKind::Tag => "upd.col_new",
        ImageChangeKind::Digest => "apps.chg.digest",
    }
}

/// The update plan (versions, image changes, migrations, missing answers, snapshot), then the
/// task.
fn update_dialog(stack: String, close: Callback<()>) -> AnyView {
    let plan = Remote::<AppUpdatePlan>::new(inst_url(&stack, "/update"));
    let settings = Remote::<DockerSettingsState>::new("/api/v1/docker/settings");
    let detail = RwSignal::new(None::<AppDetail>);
    let values = RwSignal::new(Values::new());
    let reveal = RwSignal::new(false);
    let snap = RwSignal::new(true);
    let stop = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    Effect::new(move || {
        if let Some((s, st)) = settings.data.with(|s| s.as_ref().map(|s| (s.applied.snapshot_before_update, s.applied.stop_for_snapshot))) {
            snap.set(s);
            stop.set(st);
        }
    });
    // the form of the entry, only when answers are missing
    Effect::new(move |done: Option<bool>| {
        if done == Some(true) {
            return true;
        }
        let Some(app) = plan.data.with(|p| p.as_ref().filter(|p| !p.missing_values.is_empty()).map(|p| p.app.clone())) else { return false };
        spawn_local(async move {
            match api::get::<AppDetail>(&super::texts::url(&format!("/api/v1/apps/catalog/{}", api::encode(&app)))).await {
                Ok(d) => {
                    let _ = values.try_set(schema_form::defaults(&d.form));
                    let _ = detail.try_set(Some(d));
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
        true
    });
    // the entry's texts follow the UI language (the answers stay)
    Effect::new(move |prev: Option<crate::i18n::Lang>| {
        let l = crate::i18n::lang();
        if prev.is_some_and(|p| p != l)
            && let Some(app) = detail.with_untracked(|d| d.as_ref().map(|d| d.card.id.clone()))
        {
            spawn_local(async move {
                if let Ok(d) = api::get::<AppDetail>(&super::texts::url(&format!("/api/v1/apps/catalog/{}", api::encode(&app)))).await {
                    let _ = detail.try_set(Some(d));
                }
            });
        }
        l
    });
    let n = StoredValue::new(stack.clone());
    let content = move || {
        let Some(p) = plan.data.get() else { return ().into_any() };
        let changes = p
            .image_changes
            .iter()
            .map(|c| {
                ui::row(vec![
                    ui::cell("apps.col.service", "", view! { <span class="mono">{c.service.clone()}</span> }.into_any()),
                    ui::cell("apps.col.change", "", view! { <span class="chip">{tr(change_key(c.kind))}</span> }.into_any()),
                    ui::cell("apps.col.images", "wrap", view! {
                        <div class="cell-main app-refs">
                            {c.from.clone().map(|f| view! { <span class="mono small">{f}</span> })}
                            {c.to.clone().map(|f| view! { <span class="mono small">{format!("→ {f}")}</span> })}
                        </div>
                    }.into_any()),
                ])
            })
            .collect::<Vec<_>>();
        let missing = p.missing_values.clone();
        let form = move || {
            detail.get().map(|d| {
                SchemaForm {
                    texts: super::texts::CatalogText::new(d.texts.clone()),
                    form: StoredValue::new(d.form.clone()),
                    overlays: StoredValue::new(d.overlays.clone()),
                    values,
                    server: Signal::stored(Default::default()),
                    reveal: reveal.into(),
                    only: Some(StoredValue::new(missing.clone())),
                }
                .view()
            })
        };
        let can_snap = p.snapshot;
        view! {
            {p.up_to_date.then(|| ui::note(Tone::Ok, icons::CIRCLE_CHECK, ui::text(t("apps.upd.up_to_date").to_string())))}
            {p.downgrade.then(|| ui::note(Tone::Err, icons::CIRCLE_X, ui::text(t("apps.upd.downgrade").to_string())))}
            <dl class="kvs">
                {ui::kv_row("apps.upd.app_version", Signal::stored(format!("{} → {}", p.from_app_version, p.to_app_version)), "tnum")}
                {ui::kv_row("apps.upd.pkg_version", Signal::stored(format!("{} → {}", p.from_version, p.to_version)), "tnum")}
            </dl>
            <div class="chips"><a class="chip" href=p.changelog_url.clone() target="_blank" rel="noopener noreferrer"><Icon icon=icons::EXTERNAL_LINK size=12 />{tr("apps.changelog")}</a></div>
            {(!changes.is_empty()).then(|| view! { <h4 class="mini-title">{tr("apps.upd.images")}</h4>{ui::table("", &["apps.col.service", "apps.col.change", "apps.col.images"], changes)} })}
            {p.notes.iter().map(|n| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(n.clone()))).collect_view()}
            {(!p.migrations.is_empty()).then(|| view! {
                <h4 class="mini-title">{tr("apps.upd.migrations")}</h4>
                <p class="small">{tf("apps.upd.migrations_text", &[("list", &p.migrations.join(", "))])}</p>
            })}
            {(!p.missing_values.is_empty()).then(|| view! {
                <h4 class="mini-title">{tr("apps.upd.answers")}</h4>
                <p class="small muted">{tr("apps.upd.answers_text")}</p>
                {form}
            })}
            <div class="form">
                {if can_snap {
                    view! {
                        {switch("dk.upd.snapshot", Some("apps.upd.snapshot_help"), snap.into(), Callback::new(move |v| snap.set(v)))}
                        {switch("dk.upd.stop", Some("dk.set.stop_help"), stop.into(), Callback::new(move |v| stop.set(v)))}
                    }
                    .into_any()
                } else {
                    ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(t("apps.upd.no_snapshot").to_string()))
                }}
            </div>
        }
        .into_any()
    };
    let blocked = Memo::new(move |_| plan.data.with(|p| p.as_ref().is_none_or(|p| p.up_to_date || p.downgrade)));
    let go = move |_| {
        let missing = plan.data.with_untracked(|p| p.as_ref().map(|p| p.missing_values.clone()).unwrap_or_default());
        let answers = detail.with_untracked(|d| d.as_ref().map(|d| {
            let errs = schema_form::errors(&d.form, &values.get_untracked(), Some(&missing), schema_form::js_pattern);
            (errs.is_empty(), schema_form::answers(&d.form, &values.get_untracked()).into_iter().filter(|(k, _)| missing.contains(k)).collect::<Values>())
        }));
        let answers = match answers {
            Some((false, _)) => {
                reveal.set(true);
                return;
            }
            Some((true, a)) => a,
            None if !missing.is_empty() => return,
            None => Values::new(),
        };
        let can_snap = plan.data.with_untracked(|p| p.as_ref().is_some_and(|p| p.snapshot));
        let body = AppUpdateRequest { answers, snapshot: Some(snap.get_untracked() && can_snap), stop_first: Some(stop.get_untracked()) };
        launch(inst_url(&n.get_value(), "/update"), serde_json::to_string(&body).ok(), busy, error, close);
    };
    let foot = save_bar_with(SaveBar {
        can_save: Signal::derive(move || !blocked.get()),
        icon: Some(icons::DOWNLOAD),
        error: Some(error),
        ..SaveBar::new(busy, "apps.update", close, Callback::new(go))
    });
    let body = view! { {move || plan.view(6, content)} }.into_any();
    ui::dialog(DlgKind::Wide, Signal::stored(tf("dk.upd.title", &[("name", &stack)])), close, body, Some(foot))
}

// ------------------------------------------------------------------ drawer

#[derive(Clone)]
enum Ask {
    Update,
    Rollback(StackSnapshot),
    Convert,
    Uninstall,
    Proxy,
}

/// The drawer of one installed app.
pub fn drawer(stack: String) -> AnyView {
    let a = apps();
    let d = super::remote::<InstalledAppDetail>(&inst_url(&stack, ""), crate::docker::docker().generation);
    let ask = RwSignal::new(None::<Ask>);
    let close = Callback::new(move |_| a.manage.set(None));
    let n = StoredValue::new(stack.clone());
    let content = move || {
        let Some(det) = d.data.get() else { return ().into_any() };
        let x = det.app.clone();
        let lan = x.bind.as_deref() != Some("127.0.0.1");
        let can_proxy = a.proxy.get() && lan && x.web_port.is_some();
        let snaps = det
            .snapshots
            .iter()
            .map(|sn| {
                let s = sn.clone();
                let imgs = sn.images.iter().map(|i| format!("{}: {}", i.service, i.image)).collect::<Vec<_>>();
                ui::row(vec![
                    ui::cell("dk.col.created", "", view! { <div class="cell-main"><span>{i18n::datetime(sn.created_s.max(0) as u64 * 1000)}</span><span class="cell-sub">{t(reason_key(&sn.reason))}</span></div> }.into_any()),
                    ui::cell("dk.col.images", "wrap", view! { <div class="cell-main">{imgs.into_iter().map(|i| view! { <span class="mono small">{i}</span> }).collect_view()}</div> }.into_any()),
                    ui::actions(view! { <button class="btn ghost sm" on:click=move |_| ask.set(Some(Ask::Rollback(s.clone())))><Icon icon=icons::ROTATE_CCW size=14 />{tr("snap.rollback")}</button> }.into_any()),
                ])
            })
            .collect::<Vec<_>>();
        let has_snaps = !snaps.is_empty();
        let docker_href = format!("/docker?stack={}", api::encode(&x.stack));
        view! {
            <div class="app-row-name">
                {icon_of(&x)}
                <div class="cell-main"><strong>{x.name.clone()}</strong><span class="cell-sub">{tf("apps.version_pkg", &[("v", &x.app_version), ("pkg", &x.version)])}</span></div>
            </div>
            <div class="chips">
                {state_view(&x)}
                {bind_chip(&x)}
                {x.update_available.then(|| view! { <span class="chip accent"><Icon icon=icons::DOWNLOAD size=12 />{tr("dk.upd.available")}</span> })}
                {open_link(&x)}
            </div>
            {(!x.in_catalog).then(|| ui::note(Tone::Info, icons::INFO, ui::text(t("apps.inst.left_text").to_string())))}
            {(lan && a.proxy.get() && x.web_port.is_some()).then(|| ui::note(Tone::Info, icons::SHIELD_CHECK, ui::text(t("apps.inst.on_lan").to_string())))}
            <dl class="kvs">
                {ui::kv_row("dk.ed.name", Signal::stored(x.stack.clone()), "mono")}
                {ui::kv_row("apps.w.folder", Signal::stored(x.stack_dir.clone()), "mono")}
                {ui::kv_row("apps.inst.catalog", Signal::stored(x.catalog.clone()), "mono")}
                {x.web_url.clone().map(|u| ui::kv_row("apps.w.address", Signal::stored(u), "mono"))}
                {ui::kv_row("dk.col.running", Signal::stored(i18n::of(&x.running.to_string(), &x.containers.to_string())), "tnum")}
            </dl>
            <div class="sec-actions">
                {x.update_available.then(|| view! { <button class="btn primary sm" on:click=move |_| ask.set(Some(Ask::Update))><Icon icon=icons::DOWNLOAD size=14 />{tr("apps.update")}</button> })}
                {can_proxy.then(|| view! { <button class="btn ghost sm" on:click=move |_| ask.set(Some(Ask::Proxy))><Icon icon=icons::SHIELD_CHECK size=14 />{tr("apps.proxy")}</button> })}
                <a class="btn ghost sm" href=docker_href><Icon icon=icons::CONTAINER size=14 />{tr("apps.in_docker")}</a>
                <button class="btn ghost sm" on:click=move |_| ask.set(Some(Ask::Convert))><Icon icon=icons::LAYERS size=14 />{tr("apps.convert")}</button>
            </div>
            {x.user_photos.then(|| photos_section(x.stack.clone()))}
            <div class="mini-title">{tr("dk.snap.title")}</div>
            {if !x.snapshots {
                ui::note(Tone::Info, icons::INFO, ui::text(t("dk.snap.not_subvolume").to_string()))
            } else if has_snaps {
                ui::table("", &["dk.col.created", "dk.col.images", ""], snaps)
            } else {
                ui::empty_line("apps.inst.no_snaps")
            }}
        }
        .into_any()
    };
    let dialogs = move || ask.get().map(|q| ask_dialog(q, n.get_value(), ask, close, d));
    let body = view! { {move || d.view(8, content)} {dialogs} }.into_any();
    let uninstall = ui::Danger {
        disabled: Signal::derive(move || !d.ready()),
        ..ui::Danger::new("apps.uninstall", Callback::new(move |_| ask.set(Some(Ask::Uninstall))))
    };
    ui::detail_drawer(Signal::stored(tf("apps.inst.title", &[("name", &stack)])), close, false, body, Some(uninstall), None)
}

/// Photos from the RyNAS app (D-PHOTOS): which users' Photos folders the app reads, its
/// libraries (apps with an API key), "Sync" and the key.
fn photos_section(stack: String) -> AnyView {
    let p = Remote::<PhotosState>::new(inst_url(&stack, "/photos"));
    let (busy, error, key) = (RwSignal::new(false), RwSignal::new(None::<String>), RwSignal::new(String::new()));
    let s = StoredValue::new(stack);
    let sync = move |_| {
        let url = inst_url(&s.get_value(), "/photos/sync");
        crate::tasks::run_and_report(async move { api::start_task(&url, None).await }, t("apps.ph.synced").to_string(), busy, error, Callback::new(move |_| p.reload()));
    };
    let save = move |_| {
        let url = inst_url(&s.get_value(), "/photos/key");
        let body = PhotosKeyRequest { key: key.get_untracked() };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = api::post_empty(&url, Some(&body)).await;
            let _ = busy.try_set(false);
            match r {
                Ok(()) => {
                    key.set(String::new());
                    p.reload();
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    };
    // choose a user (with an API: confirm `account`) or leave them out; a sync follows
    let choose = move |user: String, include: bool, account: Option<String>| {
        let url = inst_url(&s.get_value(), &format!("/photos/users/{}", api::encode(&user)));
        let body = serde_json::to_string(&PhotosUserRequest { include, account }).ok();
        crate::tasks::run_and_report(async move { api::start_task(&url, body).await }, t("apps.ph.synced").to_string(), busy, error, Callback::new(move |_| p.reload()));
    };
    let content = move || {
        let Some(st) = p.data.get() else { return ().into_any() };
        let api_app = st.api.is_some();
        let wired = st.users.iter().filter(|u| u.mounted).count();
        let rows = st
            .users
            .iter()
            .map(|u| {
                let state = match (u.included, u.mounted, u.library) {
                    (false, _, _) => "apps.ph.left_out",
                    (true, _, Some(PhotosLibrary::OtherOwner)) => "apps.ph.lib.other-owner",
                    (true, false, _) => "apps.ph.not_mounted",
                    (true, true, Some(PhotosLibrary::Linked)) => "apps.ph.lib.linked",
                    (true, true, Some(PhotosLibrary::Missing)) => "apps.ph.lib.missing",
                    (true, true, Some(PhotosLibrary::NoAccount)) => "apps.ph.lib.no-account",
                    (true, true, _) => "apps.ph.mounted",
                };
                let who = u.account.as_ref().or(u.proposed.as_ref()).map(|a| if a.email.is_empty() { a.name.clone() } else { a.email.clone() });
                let (user, prop) = (u.user.clone(), u.proposed.as_ref().map(|a| a.id.clone()));
                let action = match (u.included, api_app, prop) {
                    (true, _, _) => Some(("apps.ph.leave_out", false, None)),
                    (false, false, _) => Some(("apps.ph.include", true, None)),
                    (false, true, Some(id)) => Some(("apps.ph.confirm", true, Some(id))),
                    (false, true, None) => None,
                };
                view! {
                    <li>
                        <strong>{u.user.clone()}</strong>" · "{tr(state)}{who.map(|w| format!(" · {w}"))}
                        {u.problem.clone().map(|e| view! { <br /><span class="muted">{e}</span> })}
                        <br /><span class="mono muted">{u.folder.clone()}</span>
                        {action.map(|(k, inc, acc)| view! { " " <button class="btn ghost sm" disabled=move || busy.get() on:click=move |_| choose(user.clone(), inc, acc.clone())>{tr(k)}</button> })}
                    </li>
                }
            })
            .collect_view();
        view! {
            <p class="small">{tf("apps.ph.wired", &[("n", &wired.to_string()), ("total", &st.users.len().to_string())])}</p>
            <ul class="small app-list">{rows}</ul>
            {st.allow_list.map(|v| {
                let v = v.replace("{user}", st.users.first().map_or("ann", |u| u.user.as_str()));
                ui::note(Tone::Info, icons::INFO, ui::text(tf("apps.ph.allow", &[("value", &v)])))
            })}
            {st.api.map(|a| view! {
                {a.error.map(|e| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(e)))}
                <p class="small muted">{tr(if a.key { "apps.ph.key_set" } else { "apps.ph.key_hint" })}</p>
                <div class="sec-actions">
                    <input class="input" type="password" autocomplete="off" placeholder=t("dns.f.api_key") prop:value=move || key.get() on:input=move |ev| key.set(event_target_value(&ev)) />
                    <button class="btn ghost sm" disabled=move || busy.get() || key.get().trim().is_empty() on:click=save>{tr("apps.ph.key_save")}</button>
                </div>
            })}
        }
        .into_any()
    };
    view! {
        <div class="mini-title">{tr("apps.ph.title")}</div>
        <p class="small muted">{tr("apps.ph.text")}</p>
        {move || p.view(3, content)}
        {move || error.get().map(|e| ui::note(Tone::Err, icons::TRIANGLE_ALERT, ui::text(e)))}
        <div class="sec-actions">
            <button class="btn ghost sm" disabled=move || busy.get() on:click=sync><Icon icon=icons::ROTATE_CCW size=14 />{tr("apps.ph.sync")}</button>
        </div>
    }
    .into_any()
}

fn ask_dialog(q: Ask, stack: String, ask: RwSignal<Option<Ask>>, close_drawer: Callback<()>, d: Remote<InstalledAppDetail>) -> AnyView {
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let cancel = Callback::new(move |_| ask.set(None));
    match q {
        Ask::Update => update_dialog(stack, cancel),
        Ask::Rollback(sn) => {
            let url = inst_url(&stack, "/rollback");
            let id = sn.id.clone();
            let go = Callback::new(move |_| {
                let body = AppRollbackRequest { snapshot: id.clone() };
                launch(url.clone(), serde_json::to_string(&body).ok(), busy, error, cancel);
            });
            let when = i18n::datetime(sn.created_s.max(0) as u64 * 1000);
            ui::confirm(Confirm { busy, error, ..Confirm::new(tf("dk.snap.rollback_title", &[("when", &when)]), tf("apps.rb.text", &[("when", &when)]), "snap.rollback", true, go, cancel) })
        }
        Ask::Proxy => {
            let url = inst_url(&stack, "/proxy");
            let go = Callback::new(move |_| launch(url.clone(), None, busy, error, cancel));
            let future = super::proxy_url(&stack, &apps().proxy_host.get_untracked());
            let extra = view! {
                {future.map(|u| ui::note(Tone::Info, icons::LINK, ui::text(tf("apps.proxy_url", &[("url", &u)]))))}
                <p class="small muted">{tr("apps.proxy_more")}" "<a href="/network/web-access">{tr("wa.title")}</a></p>
            }
            .into_any();
            ui::confirm(Confirm { extra: Some(extra), busy, error, ..Confirm::new(t("apps.proxy_title").to_string(), t("apps.proxy_text").to_string(), "apps.proxy", false, go, cancel) })
        }
        Ask::Convert => {
            let url = inst_url(&stack, "/convert");
            let go = Callback::new(move |_| {
                busy.set(true);
                error.set(None);
                let url = url.clone();
                spawn_local(async move {
                    let r = api::post_empty::<()>(&url, None).await;
                    let _ = busy.try_set(false);
                    match r {
                        Ok(()) => {
                            toast(Tone::Ok, t("apps.conv.done").to_string());
                            ask.set(None);
                            close_drawer.try_run(());
                            crate::docker::docker().on_changed();
                        }
                        Err(e) => {
                            let _ = error.try_set(Some(e.text()));
                        }
                    }
                });
            });
            let extra = view! {
                <ul class="small app-list">
                    <li>{tr("apps.conv.p1")}</li>
                    <li>{tr("apps.conv.p2")}</li>
                    <li>{tr("apps.conv.p3")}</li>
                </ul>
            }
            .into_any();
            ui::confirm(Confirm { extra: Some(extra), busy, error, ..Confirm::new(tf("apps.conv.title", &[("name", &stack)]), t("apps.conv.text").to_string(), "apps.convert_go", true, go, cancel) })
        }
        Ask::Uninstall => {
            let del = RwSignal::new(false);
            let grants_off = RwSignal::new(false);
            let url = inst_url(&stack, "/uninstall");
            let s2 = stack.clone();
            let added = d.data.with_untracked(|x| x.as_ref().map(|x| x.app.added_grants.clone()).unwrap_or_default());
            let go = Callback::new(move |_| {
                let dd = del.get_untracked();
                let body = AppUninstallRequest { delete_data: dd, confirm: dd.then(|| s2.clone()), remove_grants: grants_off.get_untracked() };
                launch(url.clone(), serde_json::to_string(&body).ok(), busy, error, Callback::new(move |_| { ask.set(None); close_drawer.run(()); }));
            });
            let typed = stack.clone();
            let extra = view! {
                <label class="check">
                    <input type="checkbox" prop:checked=move || del.get() on:change=move |ev| del.set(event_target_checked(&ev)) />
                    <span>{tr("apps.un.delete")}</span>
                </label>
                {move || del.get().then(|| ui::note(Tone::Err, icons::TRIANGLE_ALERT, ui::text(t("apps.un.delete_text").to_string())))}
                {(!added.is_empty()).then(|| {
                    let rows = added.iter().map(|g| {
                        let who = match g.kind {
                            rynas_api_types::PrincipalKind::User => tf("apps.un.grant_user", &[("name", &g.name)]),
                            rynas_api_types::PrincipalKind::Group => tf("apps.un.grant_group", &[("name", &g.name)]),
                        };
                        let access = t(match g.access {
                            rynas_api_types::Access::Rw => "perm.rw",
                            rynas_api_types::Access::Ro => "perm.ro",
                            rynas_api_types::Access::Deny => "perm.deny",
                        });
                        view! { <li><strong>{g.share.clone()}</strong>": "{who}" ("{access}")"</li> }
                    }).collect_view();
                    view! {
                        <label class="check">
                            <input type="checkbox" prop:checked=move || grants_off.get() on:change=move |ev| grants_off.set(event_target_checked(&ev)) />
                            <span>{tr("apps.un.grants")}</span>
                        </label>
                        <ul class="small app-list">{rows}</ul>
                        <p class="small muted">{tr("apps.un.grants_text")}</p>
                    }
                })}
            }
            .into_any();
            ui::confirm(Confirm {
                extra: Some(extra),
                typed: Signal::derive(move || del.get().then(|| typed.clone())),
                busy,
                error,
                ..Confirm::new(tf("apps.un.title", &[("name", &stack)]), t("apps.un.text").to_string(), "apps.uninstall_go", true, go, cancel)
            })
        }
    }
}
