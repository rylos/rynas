//! Docker › Overview: the engine (versions, storage, state, warnings), disk usage, detected
//! managers, the install/adopt flow, the engine settings (configuration key `docker`) and
//! the move of the data-root to a volume.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::docker::{DockerDiskUsage, DockerSettings, DockerSettingsState, DockerStatus, DockerWarning, EnginePlan, EngineSource, InstallBlock, InstallRequest, MigrateRequest};

use super::{manager_name, port_href, remote};
use crate::api;
use crate::config::{apply_now, keys};
use crate::docker::docker;
use crate::form::{Draft, In, form_error, lens, lens_bool, number_field, select_field, spinner, stale_notice, switch, text_field};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::tasks::TaskRun;
use crate::toast::saved_applied;
use crate::ui::{self, Confirm, Meter, Remote, Tone, WidgetHead, kv_row};

const SETTINGS: &str = "/api/v1/docker/settings";

pub fn panel() -> AnyView {
    let d = docker();
    let settings = super::page().settings;
    let running = Memo::new(move |_| d.running());
    let task = Memo::new(move |_| d.status.with(|s| s.as_ref().and_then(|s| s.task_id.clone())));
    view! {
        <div class="grid">
            {move || task.get().map(|id| view! {
                <section class="card widget w-12">
                    <WidgetHead icon=icons::DOWNLOAD title="dk.task_running" />
                    <TaskRun id=id on_done=Callback::new(move |_| d.resync()) />
                </section>
            })}
            {move || d.status.with(|s| s.as_ref().is_some_and(|s| s.installed && s.install_block.is_none() && s.task_id.is_none())).then(install_card)}
            {engine_card()}
            {move || running.get().then(storage_card)}
            {managers_card()}
            <section class="card widget w-12">
                <WidgetHead icon=icons::SETTINGS title="dk.set.title" />
                <p class="muted small">{tr("dk.set.text")}</p>
                {move || settings.gate(6).unwrap_or_else(|| {
                    let s = settings.data.get_untracked().unwrap_or_default();
                    settings_form(s, settings)
                })}
            </section>
            {migrate_card(settings)}
        </div>
    }
    .into_any()
}

// ------------------------------------------------------------------ engine and storage

fn warning_key(w: DockerWarning) -> &'static str {
    match w {
        DockerWarning::DaemonDown => "dk.warn.daemon_down",
        DockerWarning::ComposeMissing => "dk.warn.compose_missing",
        DockerWarning::DataRootOnSystem => "dk.warn.data_root_on_system",
        DockerWarning::DataRootAlmostFull => "dk.warn.data_root_almost_full",
        DockerWarning::NotPinned => "dk.warn.not_pinned",
        DockerWarning::EngineTooOld => "dk.warn.engine_too_old",
    }
}

fn source_key(s: EngineSource) -> &'static str {
    match s {
        EngineSource::DockerCe => "dk.src.docker_ce",
        EngineSource::DockerIo => "dk.src.docker_io",
        EngineSource::Other => "dk.src.other",
        EngineSource::None => "not_set",
    }
}

fn opt(v: &Option<String>) -> String {
    v.clone().unwrap_or_else(|| t("not_set").to_string())
}

fn engine_card() -> AnyView {
    let d = docker();
    let st = d.status;
    let f = move |get: fn(&DockerStatus) -> String| Signal::derive(move || st.with(|s| s.as_ref().map(get).unwrap_or_default()));
    let running = Memo::new(move |_| d.running());
    let warnings = Memo::new(move |_| st.with(|s| s.as_ref().map(|s| s.warnings.clone()).unwrap_or_default()));
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::CONTAINER title="dk.engine">
                {move || if running.get() { ui::status(Tone::Ok, t("dk.eng.running").to_string()) } else { ui::status(Tone::Err, t("dk.eng.down").to_string()) }}
            </WidgetHead>
            <dl class="kvs">
                {kv_row("dk.eng.version", f(|s| match (&s.version, &s.api_version) {
                    (Some(v), Some(a)) => tf("dk.eng.version_api", &[("v", v), ("api", a), ("min", s.min_api_version.as_deref().unwrap_or(ui::DASH))]),
                    (v, _) => opt(v),
                }), "mono")}
                {kv_row("dk.eng.source", f(|s| t(source_key(s.source)).to_string()), "")}
                {kv_row("dk.eng.compose", f(|s| opt(&s.compose_version)), "mono")}
                {kv_row("dk.eng.buildx", f(|s| opt(&s.buildx_version)), "mono")}
                {kv_row("dk.eng.storage", f(|s| {
                    let drv = s.storage_driver.clone().unwrap_or_default();
                    if s.containerd_snapshotter { format!("{drv} · {}", t("dk.eng.containerd_store")) } else { drv }
                }), "")}
                {kv_row("dk.eng.data_root", f(|s| opt(&s.data_root)), "mono")}
                {kv_row("dk.eng.cgroup", f(|s| opt(&s.cgroup_driver)), "")}
                {kv_row("dk.eng.logging", f(|s| opt(&s.logging_driver)), "")}
                {kv_row("dk.eng.live_restore", f(|s| t(match s.live_restore { Some(true) => "on", Some(false) => "off", None => "not_set" }).to_string()), "")}
                {kv_row("dk.eng.counts", f(|s| tf("dk.eng.counts_v", &[
                    ("n", &s.counts.containers.to_string()), ("run", &s.counts.running.to_string()),
                    ("stop", &s.counts.stopped.to_string()), ("img", &s.counts.images.to_string()),
                ])), "")}
            </dl>
            {move || warnings.get().into_iter().map(|w| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(t(warning_key(w)).to_string()))).collect_view()}
        </section>
    }
    .into_any()
}

fn storage_card() -> AnyView {
    let st = docker().status;
    let df = remote::<DockerDiskUsage>("/api/v1/docker/df", 0);
    let usage = Memo::new(move |_| st.with(|s| s.as_ref().and_then(|s| s.data_root_usage)));
    let pct = Signal::derive(move || usage.get().filter(|u| u.total_bytes > 0).map(|u| u.used_bytes as f64 * 100.0 / u.total_bytes as f64).unwrap_or(0.0));
    let b = move |f: fn(&DockerDiskUsage) -> u64| Signal::derive(move || df.data.with(|d| d.as_ref().map(|d| i18n::bytes(f(d))).unwrap_or_default()));
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::HARD_DRIVE title="dk.disk" />
            {move || usage.get().map(|u| view! {
                <div class="sub-row">
                    <span>{tr("dk.disk.fs")}</span>
                    <span class="tnum muted">{tf("dk.disk.used_of", &[("used", &i18n::bytes(u.used_bytes)), ("total", &i18n::bytes(u.total_bytes)), ("free", &i18n::bytes(u.avail_bytes))])}</span>
                </div>
                <Meter pct=pct tone=Signal::derive(move || ui::usage_tone(pct.get())) />
            })}
            {move || df.gate(4).unwrap_or_else(|| view! {
                <dl class="kvs">
                    {kv_row("dk.disk.images", b(|d| d.images_bytes), "tnum")}
                    {kv_row("dk.disk.reclaimable", b(|d| d.images_reclaimable_bytes), "tnum")}
                    {kv_row("dk.disk.containers", b(|d| d.containers_bytes), "tnum")}
                    {kv_row("dk.disk.volumes", b(|d| d.volumes_bytes), "tnum")}
                    {kv_row("dk.disk.build_cache", b(|d| d.build_cache_bytes), "tnum")}
                </dl>
            }.into_any())}
        </section>
    }
    .into_any()
}

/// Managers found among the containers, with a link to their page and the warning that they
/// hold the Docker socket.
fn managers_card() -> AnyView {
    let st = docker().status;
    let list = Memo::new(move |_| st.with(|s| s.as_ref().map(|s| s.managers.clone()).unwrap_or_default()));
    (move || {
        let l = list.get();
        (!l.is_empty()).then(|| {
            let rows = l
                .into_iter()
                .map(|m| {
                    let name = manager_name(m.kind);
                    let link = m.port.filter(|_| m.running).map(|p| {
                        let label = format!("{}: {name}", t("dk.open_web"));
                        super::open_link(port_href(p), label)
                    });
                    ui::row(vec![
                        ui::cell("dk.col.manager", "", view! { <div class="cell-main"><strong>{name}</strong><span class="cell-sub mono">{m.container.clone()}</span></div> }.into_any()),
                        ui::cell("dk.col.image", "wrap", view! { <span class="mono">{m.image.clone()}</span> }.into_any()),
                        ui::cell("col.status", "", if m.running { ui::status(Tone::Ok, t("dk.st.running").to_string()) } else { ui::status(Tone::Unknown, t("dk.st.exited").to_string()) }),
                        ui::actions(link.unwrap_or_else(|| ().into_any())),
                    ])
                })
                .collect();
            view! {
                <section class="card widget w-12">
                    <WidgetHead icon=icons::LAYOUT_DASHBOARD title="dk.managers" />
                    {ui::note(Tone::Warn, icons::SHIELD_ALERT, ui::text(t("dk.manager_root").to_string()))}
                    {ui::table("", &["dk.col.manager", "dk.col.image", "col.status", ""], rows)}
                </section>
            }
        })
    })
    .into_any()
}

// ------------------------------------------------------------------ install

fn block_key(b: InstallBlock) -> &'static str {
    match b {
        InstallBlock::Installed => "dk.block.installed",
        InstallBlock::NotDebian => "dk.block.not_debian",
        InstallBlock::UnsupportedRelease => "dk.block.unsupported_release",
        InstallBlock::UnsupportedArch => "dk.block.unsupported_arch",
    }
}

/// A volume select fed by the settings' volume list; `none_key` names the empty choice.
pub(super) fn volume_select(label: &'static str, help: Option<&'static str>, settings: Remote<DockerSettingsState>, value: Signal<String>, set: Callback<String>, none_key: Option<&'static str>) -> AnyView {
    let id = crate::form::fid();
    let hid = help.map(|_| format!("{id}h"));
    let options = move || {
        let cur = value.get();
        let vols = settings.data.with(|s| s.as_ref().map(|s| s.volumes.clone()).unwrap_or_default());
        let mut v: Vec<AnyView> = Vec::new();
        if let Some(k) = none_key {
            v.push(view! { <option value="" selected=cur.is_empty()>{tr(k)}</option> }.into_any());
        }
        let known = vols.iter().any(|x| x.label == cur);
        for x in vols {
            let text = crate::places::option_text(&x);
            let sel = x.label == cur;
            let off = x.unavailable.is_some() && !sel;
            v.push(view! { <option value=x.label selected=sel disabled=off>{text}</option> }.into_any());
        }
        // a configured volume that is not mounted now stays selectable
        if !cur.is_empty() && !known {
            let c = cur.clone();
            v.push(view! { <option value=c selected=true>{cur}</option> }.into_any());
        }
        v
    };
    view! {
        <div class="field">
            <label class="field-label" for=id.clone()>{tr(label)}</label>
            <select id=id class="input" aria-describedby=hid.clone() on:change=move |ev| set.run(event_target_value(&ev))>{options}</select>
            {help.zip(hid).map(|(h, id)| view! { <span class="field-help" id=id>{tr(h)}</span> })}
            {move || {
                let cur = value.get();
                settings.data.with(|s| crate::places::note(s.as_ref().and_then(|s| s.volumes.iter().find(|x| x.label == cur)), false))
            }}
        </div>
    }
    .into_any()
}

/// "Docker — containers and compose stacks": install (volume choice, then the task's
/// progress), its state once installed. The one entry point of the install, on the Plugins
/// page and on the Docker page while Docker is missing.
pub fn install_card() -> AnyView {
    let d = docker();
    let settings = Remote::<DockerSettingsState>::new(SETTINGS);
    let vol = RwSignal::new(String::new());
    // the first volume is the default choice
    Effect::new(move || {
        if let Some(first) = settings.data.with(|s| s.as_ref().and_then(|s| s.applied.volume.clone().or_else(|| s.volumes.first().map(|v| v.label.clone()))))
            && vol.with_untracked(String::is_empty)
        {
            vol.set(first);
        }
    });
    let started = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let task = Memo::new(move |_| started.get().or_else(|| d.status.with(|s| s.as_ref().and_then(|s| s.task_id.clone()))));
    let block = Memo::new(move |_| d.status.with(|s| s.as_ref().and_then(|s| s.install_block)));
    let installed = Memo::new(move |_| d.status.with(|s| s.as_ref().is_some_and(|s| s.installed)));
    // half-installed engine packages (a failed earlier install): Install repairs them
    let incomplete = Memo::new(move |_| d.status.with(|s| s.as_ref().map(|s| s.install_incomplete.clone()).unwrap_or_default()));
    let install = move |_| {
        busy.set(true);
        error.set(None);
        let v = vol.get_untracked();
        let body = InstallRequest { volume: (!v.is_empty()).then_some(v) };
        spawn_local(async move {
            let r = api::start_task("/api/v1/docker/install", serde_json::to_string(&body).ok()).await;
            busy.set(false);
            match r {
                Ok(id) => {
                    started.set(Some(id));
                    d.refresh();
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    };
    let body = move || {
        if let Some(id) = task.get() {
            return view! { <TaskRun id=id on_done=Callback::new(move |_| { started.set(None); d.resync(); }) /> }.into_any();
        }
        let finish_btn = move || view! {
            {form_error(error)}
            <div class="sec-actions">
                <button class="btn primary" disabled=move || busy.get() on:click=install>{spinner(busy)}<Icon icon=icons::DOWNLOAD size=16 />{tr("docker.install_finish")}</button>
            </div>
        };
        // an install rynas started whose last steps never ran
        if installed.get() && block.get().is_none() {
            return view! {
                {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(t("docker.install_unfinished").to_string()))}
                {finish_btn()}
            }
            .into_any();
        }
        let half = incomplete.get();
        if !installed.get() && !half.is_empty() {
            let rows = half.iter().map(|p| view! { <li class="mono">{format!("{} {} ({})", p.name, p.version, p.status)}</li> }).collect_view();
            return view! {
                {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! {
                    <span>{tf("docker.install_incomplete", &[("n", &half.len().to_string())])}" "{tr("docker.install_incomplete_hint")}</span>
                    <ul class="small app-list">{rows}</ul>
                }.into_any())}
                {finish_btn()}
            }
            .into_any();
        }
        if installed.get() {
            return view! {
                <div class="sec-actions">
                    {ui::status(Tone::Ok, t("dk.installed").to_string())}
                    <a class="btn primary" href="/docker">{tr("dk.w.open")}</a>
                </div>
            }
            .into_any();
        }
        if let Some(b) = block.get().filter(|b| *b != InstallBlock::Installed) {
            return ui::note(Tone::Info, icons::INFO, ui::text(t(block_key(b)).to_string()));
        }
        view! {
            {volume_select("dk.inst.volume", Some("dk.inst.volume_help"), settings, vol.into(), Callback::new(move |v| vol.set(v)), Some("dk.inst.system_disk"))}
            <ul class="dk-steps small muted">
                <li>{tr("dk.inst.step_repo")}</li>
                <li>{tr("dk.inst.step_engine")}</li>
                <li>{tr("dk.inst.step_user")}</li>
            </ul>
            {form_error(error)}
            <div class="sec-actions">
                <button class="btn primary" disabled=move || busy.get() on:click=install>{spinner(busy)}<Icon icon=icons::DOWNLOAD size=16 />{tr("dk.inst.go")}</button>
            </div>
        }
        .into_any()
    };
    view! {
        <section class="card widget w-12">
            <WidgetHead icon=icons::CONTAINER title="dk.module" />
            <p class="muted">{tr("dk.module_text")}</p>
            {body}
        </section>
    }
    .into_any()
}

// ------------------------------------------------------------------ settings (key `docker`)

const MANAGERS: &[(&str, &str)] = &[("", "dk.set.mgr_none"), ("arcane", "dk.set.mgr_arcane"), ("dockhand", "dk.set.mgr_dockhand"), ("portainer", "dk.set.mgr_portainer"), ("dockge", "dk.set.mgr_dockge"), ("komodo", "dk.set.mgr_komodo")];

/// An IPv4 network `a.b.c.d/n` with n 8-28 (the server checks the host bits).
fn valid_pool(s: &str) -> bool {
    let Some((ip, n)) = s.split_once('/') else { return false };
    let octets: Vec<&str> = ip.split('.').collect();
    octets.len() == 4 && octets.iter().all(|o| o.parse::<u8>().is_ok()) && n.parse::<u8>().is_ok_and(|n| (8..=28).contains(&n))
}

fn num_lens<T: Send + Sync + 'static>(d: RwSignal<T>, get: fn(&T) -> String, set: fn(&mut T, u32)) -> (Signal<String>, Callback<String>) {
    (Signal::derive(move || d.with(get)), Callback::new(move |v: String| if let Ok(n) = v.trim().parse::<u32>() { d.update(|x| set(x, n)) }))
}

fn settings_form(init: DockerSettingsState, settings: Remote<DockerSettingsState>) -> AnyView {
    let current = |s: &DockerSettingsState| s.pending.clone().unwrap_or_else(|| s.applied.clone());
    let draft = Draft::new(current(&init), move || settings.data.with(|s| s.as_ref().map(current)));
    let d = draft.d;
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let plan = RwSignal::new(None::<EnginePlan>);
    let (vol, set_vol) = lens(d, |s| s.volume.clone().unwrap_or_default(), |s, v| s.volume = (!v.is_empty()).then_some(v));
    let (base, set_base) = lens(d, |s| s.address_pool.base.clone(), |s, v| s.address_pool.base = v.trim().to_string());
    let (size, set_size) = num_lens(d, |s| s.address_pool.size.to_string(), |s, v| s.address_pool.size = v.min(255) as u8);
    let (snap, set_snap) = lens_bool(d, |s| s.snapshot_before_update, |s, v| s.snapshot_before_update = v);
    let (stop, set_stop) = lens_bool(d, |s| s.stop_for_snapshot, |s, v| s.stop_for_snapshot = v);
    let (online_cat, set_online_cat) = lens_bool(d, |s| s.apps_online_catalog, |s, v| s.apps_online_catalog = v);
    let (app_files, set_app_files) = lens_bool(d, |s| s.files_app_data, |s, v| s.files_app_data = v);
    let (keep, set_keep) = num_lens(d, |s| s.keep_snapshots.to_string(), |s, v| s.keep_snapshots = v);
    let (full, set_full) = num_lens(d, |s| s.data_root_full_pct.to_string(), |s, v| s.data_root_full_pct = v.min(255) as u8);
    let (restarts, set_restarts) = num_lens(d, |s| s.crash_restarts.to_string(), |s, v| s.crash_restarts = v);
    let (window, set_window) = num_lens(d, |s| s.crash_window_min.to_string(), |s, v| s.crash_window_min = v);
    let (au_hour, set_au_hour) = num_lens(d, |s| s.auto_update.hour.to_string(), |s, v| s.auto_update.hour = v.min(255) as u8);
    let (au_minute, set_au_minute) = num_lens(d, |s| s.auto_update.minute.to_string(), |s, v| s.auto_update.minute = v.min(255) as u8);
    let (au_len, set_au_len) = num_lens(d, |s| s.auto_update.window_min.to_string(), |s, v| s.auto_update.window_min = v.min(u32::from(u16::MAX)) as u16);
    let (mgr, set_mgr) = lens(
        d,
        |s| s.preferred_manager.map(|m| serde_json::to_value(m).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()).unwrap_or_default(),
        |s, v| s.preferred_manager = serde_json::from_value(serde_json::Value::String(v)).ok(),
    );
    let base_err = Signal::derive(move || (!valid_pool(&base.get())).then(|| t("dk.set.pool_invalid").to_string()));
    let dirty = draft.dirty();
    let reset = Callback::new(move |_| {
        draft.reset();
        error.set(None);
    });
    let applied_vol = move || settings.data.with_untracked(|s| s.as_ref().and_then(|s| s.applied.volume.clone()));
    // stage, then apply only `docker` (A19b)
    let save = Callback::new(move |_: ()| {
        let body: DockerSettings = d.get_untracked();
        if !valid_pool(&body.address_pool.base) {
            error.set(Some(t("form.check").to_string()));
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match api::put::<_, DockerSettingsState>(SETTINGS, &body).await {
                Ok(_) => {
                    if apply_now(&[keys::DOCKER], error).await {
                        saved_applied();
                        plan.set(None);
                    }
                }
                Err(e) => error.set(Some(e.text())),
            }
            busy.set(false);
            settings.reload();
            docker().refresh();
        });
    });
    // a new volume moves the engine: show what the apply writes and runs first
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let body = d.get_untracked();
        if body.volume == applied_vol() {
            save.run(());
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match api::post::<_, EnginePlan>("/api/v1/docker/plan/engine", Some(&body)).await {
                Ok(p) => plan.set(Some(p)),
                Err(e) => error.set(Some(e.text())),
            }
            busy.set(false);
        });
    };
    view! {
        <form class="form" on:submit=submit>
            <div class="form-row">
                {volume_select("dk.set.volume", Some("dk.set.volume_help"), settings, vol, set_vol, Some("dk.set.volume_none"))}
                {select_field("dk.set.manager", Some("dk.set.manager_help"), MANAGERS, mgr, set_mgr)}
                {text_field("dk.set.pool", base, set_base, base_err, In { mono: true, help: Some("dk.set.pool_help"), maxlength: 18, ..In::default() })}
                {number_field("dk.set.pool_size", Some("dk.set.pool_size_help"), size, set_size, 9, 30)}
            </div>
            // the snapshots of the stacks together, then the other switches
            {switch("dk.set.snapshot", Some("dk.set.snapshot_help"), snap, set_snap)}
            {switch("dk.set.stop", Some("dk.set.stop_help"), stop, set_stop)}
            <div class="form-row">
                {number_field("dk.set.keep", None, keep, set_keep, 1, 50)}
            </div>
            {switch("dk.set.online_catalog", Some("dk.set.online_catalog_help"), online_cat, set_online_cat)}
            {switch("dk.set.files_app_data", Some("dk.set.files_app_data_help"), app_files, set_app_files)}
            <div class="form-row">
                {number_field("dk.set.full", Some("dk.set.full_help"), full, set_full, 50, 99)}
                {number_field("dk.set.restarts", Some("dk.set.restarts_help"), restarts, set_restarts, 2, 100)}
                {number_field("dk.set.window", None, window, set_window, 1, 120)}
            </div>
            <h3 class="mini-title">{tr("dk.au.title")}</h3>
            <p class="field-help">{tr("dk.au.text")}</p>
            <div class="form-row">
                {number_field("dk.au.hour", None, au_hour, set_au_hour, 0, 23)}
                {number_field("dk.au.minute", None, au_minute, set_au_minute, 0, 59)}
                {number_field("dk.au.window", Some("dk.au.window_help"), au_len, set_au_len, 10, 360)}
            </div>
            {stale_notice(draft.stale, reset)}
            {form_error(error)}
            <div class="sec-actions">
                <button type="submit" class="btn primary" disabled=move || busy.get() || !dirty.get()>{spinner(busy)}{tr("save")}</button>
                <button type="button" class="btn ghost" disabled=move || !dirty.get() on:click=move |_| reset.run(())>{tr("reset")}</button>
            </div>
        </form>
        {move || plan.get().map(|p| plan_dialog(p, busy, error, save, Callback::new(move |_| plan.set(None))))}
    }
    .into_any()
}

/// What applying a new engine volume writes, creates and runs; Save and apply to go on.
fn plan_dialog(p: EnginePlan, busy: RwSignal<bool>, error: RwSignal<Option<String>>, save: Callback<()>, close: Callback<()>) -> AnyView {
    let files = p.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>();
    let extra = view! {
        <div class="dk-plan">
            {(!files.is_empty()).then(|| view! { <div class="mini-title">{tr("dk.plan.files")}</div><ul class="mono small">{files.into_iter().map(|f| view! { <li>{f}</li> }).collect_view()}</ul> })}
            {(!p.create.is_empty()).then(|| view! { <div class="mini-title">{tr("dk.plan.create")}</div><ul class="mono small">{p.create.iter().map(|f| view! { <li>{f.clone()}</li> }).collect_view()}</ul> })}
            {(!p.commands.is_empty()).then(|| view! { <div class="mini-title">{tr("dk.plan.commands")}</div><ul class="mono small">{p.commands.iter().map(|f| view! { <li>{f.clone()}</li> }).collect_view()}</ul> })}
            {p.restart.then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(t("dk.plan.restart").to_string())))}
        </div>
    }
    .into_any();
    ui::confirm(Confirm {
        title: t("dk.plan.title").to_string(),
        text: t("dk.plan.text").to_string(),
        action: "dk.plan.go",
        danger: false,
        extra: Some(extra),
        typed: Signal::stored(None),
        busy,
        error,
        on_confirm: save,
        on_cancel: close,
    })
}

// ------------------------------------------------------------------ data-root migration

fn migrate_card(settings: Remote<DockerSettingsState>) -> AnyView {
    let d = docker();
    let vol = RwSignal::new(String::new());
    Effect::new(move || {
        if let Some(v) = settings.data.with(|s| s.as_ref().and_then(|s| s.applied.volume.clone().or_else(|| s.volumes.first().map(|v| v.label.clone()))))
            && vol.with_untracked(String::is_empty)
        {
            vol.set(v);
        }
    });
    let running = Memo::new(move |_| d.status.with(|s| s.as_ref().map(|s| s.counts.running).unwrap_or(0)));
    let ask = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let task = RwSignal::new(None::<String>);
    let go = Callback::new(move |_| {
        busy.set(true);
        error.set(None);
        let body = MigrateRequest { volume: vol.get_untracked() };
        spawn_local(async move {
            let r = api::start_task("/api/v1/docker/engine/migrate", serde_json::to_string(&body).ok()).await;
            busy.set(false);
            match r {
                Ok(id) => {
                    ask.set(false);
                    task.set(Some(id));
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    });
    view! {
        <section class="card widget w-12">
            <WidgetHead icon=icons::FOLDER_SYNC title="dk.mig.title" />
            <p class="muted small">{tr("dk.mig.text")}</p>
            {move || match task.get() {
                Some(id) => view! { <TaskRun id=id on_done=Callback::new(move |_| { task.set(None); d.resync(); settings.reload(); }) /> }.into_any(),
                None => view! {
                    {volume_select("dk.mig.volume", None, settings, vol.into(), Callback::new(move |v| vol.set(v)), None)}
                    {move || (running.get() > 0).then(|| ui::note(Tone::Info, icons::INFO, ui::text(tf("dk.mig.running", &[("n", &running.get().to_string())]))))}
                    <div class="sec-actions">
                        <button class="btn ghost" disabled=move || { running.get() > 0 || vol.with(String::is_empty) } on:click=move |_| { error.set(None); ask.set(true); }>
                            <Icon icon=icons::FOLDER_SYNC size=16 />{tr("dk.mig.go")}
                        </button>
                    </div>
                }
                .into_any(),
            }}
            {move || ask.get().then(|| ui::confirm(Confirm {
                    title: t("dk.mig.title").to_string(),
                    text: tf("dk.mig.confirm", &[("vol", &crate::places::name(&vol.get_untracked()))]),
                    action: "dk.mig.go",
                    danger: false,
                    extra: None,
                    typed: Signal::stored(None),
                    busy,
                    error,
                    on_confirm: go,
                    on_cancel: Callback::new(move |_| ask.set(false)),
                }))}
        </section>
    }
    .into_any()
}
