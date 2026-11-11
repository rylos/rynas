//! Backup › PCs, file backup (D-PCBACKUP): PCs with `rynas-agent`, one restic repository per
//! backup group.
//!
//! - The tab lists the PCs (state, last good version, versions, uploaded bytes) and the
//!   groups (repository size, maintenance with its memory estimate, recovery kit).
//! - "Add a PC": a one-time pairing code (15 min) with the command for the PC and the NAS
//!   certificate's fingerprint; the PC gets a group of its own, or joins one (the dialog says
//!   that PCs of one group can read each other's data, and warns when the group's maintenance
//!   would grow past the memory budget).
//! - The drawer of a PC: status and progress, Back up now / Cancel, settings (schedule,
//!   freeze, excludes, retention, alerts), versions with Browse (download a file, a zip of a
//!   selection), the restore code for a bare-metal restore, revoke.
//! - Settings of the feature: the folder of the repositories, the default quota, the
//!   maintenance hour and memory share.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::backup::{NodeEntry, Retention, SnapshotEntry};
use rynas_api_types::pcagent::*;
use rynas_api_types::TaskStatus;

use crate::api;
use crate::form::{In, area_field, form_error, number_field, segmented, select_field, spinner, text_field};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::pages::security::copy_button;
use crate::tasks;
use crate::toast::toast;
use crate::ui::{self, Confirm, DlgKind, EmptyState, Remote, Tone};

const URL: &str = "/api/v1/pcbackup";

#[derive(Clone, Copy)]
struct Tab {
    data: Remote<PcBackupOverview>,
    open: RwSignal<Option<String>>,
    pair: RwSignal<bool>,
    settings: RwSignal<bool>,
}

fn dev_url(d: &str) -> String {
    format!("{URL}/devices/{}", api::encode(d))
}

fn group_url(g: &str) -> String {
    format!("{URL}/groups/{}", api::encode(g))
}

fn when(ms: Option<u64>) -> String {
    ms.map(i18n::datetime).unwrap_or_else(|| "—".into())
}

fn kv(label: &'static str, value: AnyView) -> AnyView {
    view! { <div class="kv"><dt>{tr(label)}</dt><dd>{value}</dd></div> }.into_any()
}

fn state_view(s: PcState, revoked: bool) -> AnyView {
    if revoked {
        return ui::status(Tone::Info, t("pca.st.revoked").to_string());
    }
    let (tone, k) = match s {
        PcState::Waiting => (Tone::Info, "bk.st.waiting"),
        PcState::Ok => (Tone::Ok, "upd.uptodate"),
        PcState::Running => (Tone::Info, "pca.st.running"),
        PcState::Late => (Tone::Warn, "pca.st.late"),
        PcState::Critical => (Tone::Err, "pca.st.critical"),
        PcState::Failed => (Tone::Err, "pca.st.failed"),
    };
    ui::status(tone, t(k).to_string())
}

fn reason_text(r: ReadyReason) -> &'static str {
    match r {
        ReadyReason::NoVersion => "pca.ready.no_version",
        ReadyReason::NoManifest => "pca.ready.no_manifest",
        ReadyReason::ManifestStale => "pca.ready.stale",
        ReadyReason::VolumeSkipped => "pca.ready.skipped",
        ReadyReason::LoaderUnknown => "pca.ready.loader",
        ReadyReason::NoPartitionTable => "pca.ready.no_table",
        ReadyReason::NoRecoveryKit => "pca.ready.no_kit",
        ReadyReason::ManualLayout => "pca.ready.manual",
    }
}

fn method_text(m: CaptureMethod) -> &'static str {
    match m {
        CaptureMethod::BtrfsSnapshot => "pca.m.btrfs",
        CaptureMethod::LvmSnapshot => "pca.m.lvm",
        CaptureMethod::Frozen => "pca.m.frozen",
        CaptureMethod::FrozenThawed => "pca.m.thawed",
        CaptureMethod::Hot => "pca.m.hot",
        CaptureMethod::Files => "pca.m.files",
        CaptureMethod::Skipped => "pca.m.skipped",
    }
}

/// The address of this NAS as the browser reaches it.
fn origin() -> String {
    window().location().origin().unwrap_or_default()
}

/// A POST that starts a task, followed to its end; toasts `ok`, reloads the tab.
fn run_task(tab: Tab, method: &'static str, url: String, ok: String, busy: RwSignal<bool>, error: RwSignal<Option<String>>, done: Callback<()>) {
    busy.set(true);
    error.set(None);
    spawn_local(async move {
        let started = match method {
            "DELETE" => api::delete::<rynas_api_types::TaskStarted>(&url).await.map(|t| t.task_id),
            _ => api::start_task(&url, None).await,
        };
        let r = match started {
            Ok(id) => tasks::follow(&id).await,
            Err(e) => Err(e),
        };
        let _ = busy.try_set(false);
        match r {
            Ok(task) if task.status == TaskStatus::Ok => {
                toast(Tone::Ok, ok);
                tab.data.reload();
                done.run(());
            }
            Ok(task) => {
                let _ = error.try_set(Some(tasks::task_error(&task)));
            }
            Err(e) => {
                let _ = error.try_set(Some(e.text()));
            }
        }
    });
}

/// The tab's actions in the page header.
pub fn header_buttons(pair: RwSignal<Option<bool>>) -> AnyView {
    view! {
        <button class="btn primary" on:click=move |_| pair.set(Some(true))><Icon icon=icons::PLUS size=16 /><span class="btn-label">{tr("bk.add")}</span></button>
        <button class="btn ghost" on:click=move |_| pair.set(Some(false))><Icon icon=icons::SETTINGS size=16 /><span class="btn-label">{tr("pca.settings")}</span></button>
    }
    .into_any()
}

/// The tab. `header` is set by the page header's buttons: `Some(true)` add a PC,
/// `Some(false)` the settings.
pub fn panel(header: RwSignal<Option<bool>>) -> AnyView {
    let tab = Tab {
        data: Remote::new(URL),
        open: RwSignal::new(None),
        pair: RwSignal::new(false),
        settings: RwSignal::new(false),
    };
    Effect::new(move |_| match header.get() {
        Some(true) => {
            tab.pair.set(true);
            header.set(None);
        }
        Some(false) => {
            tab.settings.set(true);
            header.set(None);
        }
        None => {}
    });
    // live progress: read again every 5 s while a PC backs up and the page is visible
    let ticking = RwSignal::new(false);
    Effect::new(move |_| {
        let running = tab.data.data.with(|d| d.as_ref().is_some_and(|d| d.devices.iter().any(|x| x.state == PcState::Running || x.command.is_some())));
        if running && !ticking.get_untracked() {
            ticking.set(true);
            spawn_local(async move {
                api::sleep(5000).await;
                let _ = ticking.try_set(false);
                if !document().hidden() {
                    tab.data.reload();
                }
            });
        }
    });
    let content = move || {
        let Some(o) = tab.data.data.get() else { return ().into_any() };
        if o.settings.folder.is_empty() {
            return view! {
                <EmptyState icon=icons::MONITOR title=Signal::derive(move || t("pca.setup").to_string()) text=Signal::derive(move || t("pca.setup_text").to_string())>
                    <div class="sec-actions"><button class="btn primary" on:click=move |_| tab.settings.set(true)><Icon icon=icons::FOLDER size=16 />{tr("pca.choose_folder")}</button></div>
                </EmptyState>
            }
            .into_any();
        }
        let pcs = if o.devices.is_empty() {
            view! {
                <EmptyState icon=icons::MONITOR title=Signal::derive(move || t("bkpc.empty").to_string()) text=Signal::derive(move || t("pca.empty_text").to_string())>
                    <div class="sec-actions"><button class="btn primary" on:click=move |_| tab.pair.set(true)><Icon icon=icons::PLUS size=16 />{tr("bk.add")}</button></div>
                </EmptyState>
            }
            .into_any()
        } else {
            let rows = o
                .devices
                .iter()
                .map(|d| {
                    let dev = d.device.clone();
                    let sub = if d.description.is_empty() { Some(d.hostname.clone()) } else { Some(format!("{} · {}", d.description, d.hostname)) };
                    let label = format!("{}: {}", t("bk.open"), d.device);
                    let progress = d.progress.as_ref().map(|p| {
                        let pct = (p.bytes_done * 100).checked_div(p.bytes_total).unwrap_or(0);
                        format!("{pct}% · {}/s", i18n::bytes(p.speed_bps))
                    });
                    ui::row(vec![
                        ui::cell("pca.col.pc", "wrap", ui::two_lines(view! { <strong>{d.device.clone()}</strong> }.into_any(), sub, "cell-sub small")),
                        ui::cell("col.status", "wrap", view! { <div class="cell-main">{state_view(d.state, d.revoked)}<span class="cell-sub small">{progress.unwrap_or_else(|| when(d.last_ok_ms))}</span></div> }.into_any()),
                        ui::text_cell("pca.col.group", "mono small", d.group.clone()),
                        ui::text_cell("pca.col.versions", "num tnum", i18n::count(u64::from(d.versions))),
                        ui::text_cell("pca.col.uploaded", "num tnum", i18n::bytes(d.uploaded_bytes)),
                        ui::actions(view! { <button class="btn ghost sm" aria-label=label on:click=move |_| tab.open.set(Some(dev.clone()))><Icon icon=icons::EYE size=14 />{tr("bk.open")}</button> }.into_any()),
                    ])
                })
                .collect::<Vec<_>>();
            ui::table("pca-table", &["pca.col.pc", "col.status", "pca.col.group", "#pca.col.versions", "#pca.col.uploaded", ""], rows)
        };
        let budget = o.memory_budget_bytes;
        let groups = o
            .groups
            .iter()
            .map(|g| group_row(tab, g.clone(), budget, o.devices.iter().filter(|d| d.group == g.group).all(|d| d.revoked)))
            .collect::<Vec<_>>();
        let pairings = o
            .pairings
            .iter()
            .map(|p| {
                let id = p.id.clone();
                let label = format!("{}: {}", t("pca.pairing_cancel"), p.description);
                ui::row(vec![
                    ui::text_cell("bk.f.description", "wrap", if p.description.is_empty() { "—".into() } else { p.description.clone() }),
                    ui::text_cell("pca.col.group", "mono small", p.group.clone().unwrap_or_else(|| t("pca.own_group").to_string())),
                    ui::text_cell("pca.col.expires", "tnum", i18n::datetime(p.expires_ms)),
                    ui::actions(view! { <button class="icon-btn sm" title=tr("pca.pairing_cancel") aria-label=label on:click=move |_| {
                        let u = format!("{URL}/pairings/{}", api::encode(&id));
                        spawn_local(async move {
                            if api::delete_empty(&u).await.is_ok() { tab.data.reload(); }
                        });
                    }><Icon icon=icons::X size=16 /></button> }.into_any()),
                ])
            })
            .collect::<Vec<_>>();
        view! {
            {pcs}
            {(!pairings.is_empty()).then(|| view! {
                <h3 class="mini-title">{tr("pca.pairings")}</h3>
                {ui::table("pca-pairings", &["bk.f.description", "pca.col.group", "pca.col.expires", ""], pairings)}
            })}
            {(!groups.is_empty()).then(|| view! {
                <h3 class="mini-title">{tr("pca.groups")}</h3>
                <p class="small muted">{tf("pca.groups_note", &[("budget", &i18n::bytes(budget))])}</p>
                {ui::table("pca-groups", &["pca.col.group", "pca.col.pcs", "#pca.col.size", "pca.col.maintenance", ""], groups)}
            })}
        }
        .into_any()
    };
    view! {
        <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("pca.intro")}</p>
        {move || tab.data.view(4, content)}
        {move || tab.open.get().map(|d| drawer(tab, d))}
        {move || tab.pair.get().then(|| pair_dialog(tab))}
        {move || tab.settings.get().then(|| settings_dialog(tab))}
    }
    .into_any()
}

// ------------------------------------------------------------------ groups

fn group_row(tab: Tab, g: PcGroup, budget: u64, all_revoked: bool) -> AnyView {
    let name = StoredValue::new(g.group.clone());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let kit = RwSignal::new(None::<RecoveryKit>);
    let kit_busy = RwSignal::new(false);
    let ask_delete = RwSignal::new(false);
    let size = if g.settings.quota_gib > 0 { format!("{} / {} GiB", i18n::bytes(g.used_bytes), g.settings.quota_gib) } else { i18n::bytes(g.used_bytes) };
    let m = &g.maintenance;
    let over = m.estimate_bytes > budget && budget > 0;
    let maint = view! {
        <div class="cell-main">
            <span>{match m.last_ms { Some(ms) => tf(if m.ok { "pca.maint.ok" } else { "pca.maint.failed" }, &[("when", &i18n::datetime(ms))]), None => t("pca.maint.never").to_string() }}</span>
            <span class=if over { "cell-sub small warn-text" } else { "cell-sub small" }>{tf("pca.maint.estimate", &[("mem", &i18n::bytes(m.estimate_bytes))])}</span>
            {m.postponed.clone().map(|p| view! { <span class="cell-sub small warn-text">{p}</span> })}
            {m.error.clone().map(|p| view! { <span class="cell-sub small err-text">{p}</span> })}
        </div>
    }
    .into_any();
    let maintain = move |_| {
        let n = name.get_value();
        run_task(tab, "POST", format!("{}/maintenance", group_url(&n)), tf("pca.maint.done", &[("group", &n)]), busy, error, Callback::new(|_| ()));
    };
    let make_kit = move |_| {
        kit_busy.set(true);
        error.set(None);
        let u = format!("{}/recovery-kit", group_url(&name.get_value()));
        spawn_local(async move {
            let r = api::post::<(), RecoveryKit>(&u, None).await;
            let _ = kit_busy.try_set(false);
            match r {
                Ok(k) => {
                    kit.set(Some(k));
                    tab.data.reload();
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    };
    let delete = Callback::new(move |_| {
        let n = name.get_value();
        run_task(tab, "DELETE", group_url(&n), tf("pca.group_deleted", &[("group", &n)]), busy, error, Callback::new(move |_| ask_delete.set(false)));
    });
    let label_m = format!("{}: {}", t("pca.maint.run"), g.group);
    let label_k = format!("{}: {}", t("pca.kit"), g.group);
    let label_d = format!("{}: {}", t("pca.group_delete"), g.group);
    ui::row(vec![
        ui::cell("pca.col.group", "wrap", ui::two_lines(view! { <strong class="mono">{g.group.clone()}</strong> }.into_any(), (!g.settings.description.is_empty()).then(|| g.settings.description.clone()), "cell-sub small")),
        ui::text_cell("pca.col.pcs", "wrap", g.devices.join(", ")),
        ui::text_cell("pca.col.size", "num tnum", size),
        ui::cell("pca.col.maintenance", "wrap", maint),
        ui::actions(view! {
            <button class="icon-btn sm" title=tr("pca.maint.run") aria-label=label_m disabled=move || busy.get() on:click=maintain><Icon icon=icons::REFRESH_CW size=16 /></button>
            <button class="icon-btn sm" title=tr("pca.kit") aria-label=label_k disabled=move || kit_busy.get() on:click=make_kit><Icon icon=icons::KEY size=16 /></button>
            {all_revoked.then(|| view! { <button class="icon-btn sm" title=tr("pca.group_delete") aria-label=label_d on:click=move |_| ask_delete.set(true)><Icon icon=icons::TRASH_2 size=16 /></button> })}
            {form_error(error)}
            {move || kit.get().map(|k| kit_dialog(k, Callback::new(move |_| kit.set(None))))}
            {move || ask_delete.get().then(|| ui::confirm(Confirm { busy, error, ..Confirm::new(tf("pca.group_delete_title", &[("group", &name.get_value())]), t("pca.group_delete_text").to_string(), "pca.group_delete", true, delete, Callback::new(move |_| ask_delete.set(false))) }))}
        }
        .into_any()),
    ])
}

/// The recovery kit: shown once, printable.
fn kit_dialog(k: RecoveryKit, close: Callback<()>) -> AnyView {
    let text = format!(
        "rynas recovery kit\n\nGroup: {}\nPCs: {}\nRepository (on the NAS): {}\nPassword: {}\nMade: {}\n\nWith stock restic on any machine that has a copy of the folder:\n  restic -r <folder> snapshots\n  restic -r <folder> restore <id> --target /restore\n",
        k.group,
        k.devices.join(", "),
        k.repository,
        k.password,
        i18n::datetime(k.created_ms)
    );
    let body = view! {
        {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(t("pca.kit_once").to_string()))}
        <dl class="kvs">
            {kv("pca.col.group", ui::text(k.group.clone()))}
            {kv("pca.col.pcs", ui::text(k.devices.join(", ")))}
            {kv("pca.kit.repo", ui::breakable(&k.repository, "mono"))}
            {kv("pca.kit.password", ui::breakable(&k.password, "mono"))}
        </dl>
        <p class="small muted">{tr("pca.kit_text")}</p>
        <div class="sec-actions">
            {copy_button("dns.copy", text)}
            <button class="btn ghost sm" type="button" on:click=move |_| { let _ = window().print(); }><Icon icon=icons::FILE_TEXT size=14 />{tr("pca.kit.print")}</button>
        </div>
    }
    .into_any();
    let foot = view! { <button type="button" class="btn primary" on:click=move |_| close.run(())>{tr("close")}</button> }.into_any();
    ui::dialog(DlgKind::Normal, Signal::stored(t("pca.kit").to_string()), close, body, Some(foot))
}

// ------------------------------------------------------------------ pairing

fn pair_dialog(tab: Tab) -> AnyView {
    let close = Callback::new(move |_| tab.pair.set(false));
    let description = RwSignal::new(String::new());
    let mode = RwSignal::new("own".to_string());
    let group = RwSignal::new(String::new());
    let quota = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let created = RwSignal::new(None::<PairingCreated>);
    let groups = move || tab.data.data.with(|d| d.as_ref().map(|d| d.groups.clone()).unwrap_or_default());
    let budget = move || tab.data.data.with(|d| d.as_ref().map(|d| d.memory_budget_bytes).unwrap_or(0));
    let create = move |_| {
        let join = mode.get_untracked() == "join";
        let body = PairingRequest {
            description: description.get_untracked().trim().to_string(),
            group: join.then(|| group.get_untracked()).filter(|g| !g.is_empty()),
            quota_gib: quota.get_untracked().trim().parse().ok(),
        };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = api::post::<_, PairingCreated>(&format!("{URL}/pairings"), Some(&body)).await;
            let _ = busy.try_set(false);
            match r {
                Ok(c) => {
                    created.set(Some(c));
                    tab.data.reload();
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    };
    const MODES: &[(&str, &str)] = &[("own", "pca.own_group"), ("join", "pca.join_group")];
    let join_view = move || {
        (mode.get() == "join").then(|| {
            let gs = groups();
            if gs.is_empty() {
                return view! { <p class="small muted">{tr("pca.no_groups")}</p> }.into_any();
            }
            if group.get_untracked().is_empty()
                && let Some(g) = gs.first()
            {
                group.set(g.group.clone());
            }
            let chosen = gs.iter().find(|g| g.group == group.get()).cloned();
            let warn = chosen.as_ref().map(|g| {
                let n = g.devices.len().max(1) as u64;
                let after = g.maintenance.estimate_bytes / n * (n + 1);
                (after > budget() && budget() > 0).then(|| ui::note(Tone::Warn, icons::MEMORY_STICK, ui::text(tf("pca.join_memory", &[("mem", &i18n::bytes(after)), ("budget", &i18n::bytes(budget()))]))))
            });
            view! {
                <div class="field">
                    <label class="field-label" for="pca-group">{tr("pca.col.group")}</label>
                    <select id="pca-group" class="input" on:change=move |ev| group.set(event_target_value(&ev))>
                        {gs.iter().map(|g| {
                            let v = g.group.clone();
                            let sel = v == group.get_untracked();
                            view! { <option value=v.clone() selected=sel>{format!("{} ({})", v, g.devices.join(", "))}</option> }
                        }).collect_view()}
                    </select>
                </div>
                {ui::note(Tone::Warn, icons::SHIELD_ALERT, ui::text(t("pca.join_warning").to_string()))}
                {warn.flatten()}
            }
            .into_any()
        })
    };
    let form = move || {
        view! {
            {text_field("bk.f.description", description.into(), Callback::new(move |v| description.set(v)), Signal::stored(None), In { maxlength: 120, placeholder: "Office laptop", ..In::default() })}
            {segmented("pca.col.group", MODES, mode.into(), Callback::new(move |v| mode.set(v)))}
            <p class="field-help">{move || if mode.get() == "join" { t("pca.join_help") } else { t("pca.own_help") }}</p>
            {join_view}
            <Show when=move || mode.get() == "own">
                {number_field("pca.f.quota", Some("pca.f.quota_help"), quota.into(), Callback::new(move |v| quota.set(v)), 0, 100_000)}
            </Show>
            {form_error(error)}
        }
        .into_any()
    };
    let done = move || {
        created.get().map(|c| {
            let url = origin();
            let cmd = c.command.replace("{url}", &url);
            let install = "curl -fLO https://github.com/rylos/rynas/releases/latest/download/rynas-agent-x86_64 && chmod +x rynas-agent-x86_64 && sudo install rynas-agent-x86_64 /usr/local/bin/rynas-agent".to_string();
            view! {
                {ui::note(Tone::Ok, icons::CIRCLE_CHECK, ui::text(tf("pca.code_valid", &[("until", &i18n::datetime(c.expires_ms))])))}
                <dl class="kvs">
                    {kv("pca.code", ui::breakable(&c.code, "mono"))}
                    {c.fingerprint.clone().map(|f| kv("pca.fingerprint", ui::breakable(f.as_str(), "mono small")))}
                </dl>
                <h4 class="small"><strong>{tr("pca.step.install")}</strong></h4>
                <pre class="app-code">{install.clone()}</pre>
                <div class="sec-actions">{copy_button("dns.copy", install)}</div>
                <h4 class="small"><strong>{tr("pca.step.pair")}</strong></h4>
                <pre class="app-code">{cmd.clone()}</pre>
                <div class="sec-actions">{copy_button("dns.copy", cmd)}</div>
                <p class="small muted">{tr("pca.step.note")}</p>
            }
            .into_any()
        })
    };
    let body = view! { {move || if created.with(Option::is_some) { done().unwrap_or_else(|| ().into_any()) } else { form() }} }.into_any();
    let foot = view! {
        <button type="button" class="btn ghost" on:click=move |_| close.run(())>{move || if created.with(Option::is_some) { t("close") } else { t("cancel") }}</button>
        <Show when=move || created.with(Option::is_none)>
            <button type="button" class="btn primary" disabled=move || busy.get() on:click=create>{spinner(busy)}<Icon icon=icons::KEY size=16 />{tr("pca.make_code")}</button>
        </Show>
    }
    .into_any();
    ui::dialog(DlgKind::Normal, Signal::derive(move || t("bk.add").to_string()), close, body, Some(foot))
}

// ------------------------------------------------------------------ settings of the feature

fn settings_dialog(tab: Tab) -> AnyView {
    let close = Callback::new(move |_| tab.settings.set(false));
    let cur = tab.data.data.with_untracked(|d| d.as_ref().map(|d| d.settings.clone())).unwrap_or_default();
    let folder = RwSignal::new(cur.folder.clone());
    let quota = RwSignal::new(cur.default_quota_gib.to_string());
    let hour = RwSignal::new(cur.maintenance_hour.to_string());
    let pct = RwSignal::new(if cur.maintenance_memory_pct == 0 { "40".to_string() } else { cur.maintenance_memory_pct.to_string() });
    let folder_err = RwSignal::new(None::<String>);
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let save = move |_| {
        let n = |s: RwSignal<String>| s.get_untracked().trim().parse::<u32>().unwrap_or(0);
        let body = PcAgentSettings {
            folder: folder.get_untracked().trim().to_string(),
            default_quota_gib: n(quota),
            maintenance_hour: n(hour).min(23) as u8,
            maintenance_memory_pct: n(pct).clamp(10, 80) as u8,
        };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = api::put::<_, serde_json::Value>(&format!("{URL}/settings"), &body).await;
            let _ = busy.try_set(false);
            match r {
                Ok(_) => {
                    toast(Tone::Ok, t("bk.saved").to_string());
                    tab.data.reload();
                    tab.settings.set(false);
                }
                Err(e) if e.field() == Some("folder") => folder_err.set(Some(e.text())),
                Err(e) => error.set(Some(e.text())),
            }
        });
    };
    let body = view! {
        {text_field("pca.f.folder", folder.into(), Callback::new(move |v| { folder_err.set(None); folder.set(v); }), folder_err.into(), In { mono: true, maxlength: 512, help: Some("pca.f.folder_help"), placeholder: "/volume1/pc-backup", ..In::default() })}
        {number_field("pca.f.default_quota", Some("pca.f.quota_help"), quota.into(), Callback::new(move |v| quota.set(v)), 0, 100_000)}
        <div class="form-row">
            {number_field("pca.f.hour", Some("pca.f.hour_help"), hour.into(), Callback::new(move |v| hour.set(v)), 0, 23)}
            {number_field("pca.f.memory", Some("pca.f.memory_help"), pct.into(), Callback::new(move |v| pct.set(v)), 10, 80)}
        </div>
        {form_error(error)}
    }
    .into_any();
    let foot = view! {
        <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("cancel")}</button>
        <button type="button" class="btn primary" disabled=move || busy.get() on:click=save>{spinner(busy)}{tr("save")}</button>
    }
    .into_any();
    ui::dialog(DlgKind::Normal, Signal::stored(t("pca.settings").to_string()), close, body, Some(foot))
}

// ------------------------------------------------------------------ the drawer of a PC

fn drawer(tab: Tab, device: String) -> AnyView {
    let close = Callback::new(move |_| tab.open.set(None));
    let dev = StoredValue::new(device.clone());
    let detail = Remote::<PcDeviceDetail>::new(dev_url(&device));
    let versions = Remote::<Vec<SnapshotEntry>>::new(format!("{}/versions", dev_url(&device)));
    let browse = RwSignal::new(None::<String>);
    let edit = RwSignal::new(false);
    let ask_revoke = RwSignal::new(false);
    let code = RwSignal::new(None::<RestoreCodeCreated>);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let command = move |what: &'static str| {
        let u = format!("{}/{what}", dev_url(&dev.get_value()));
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = api::post_empty::<()>(&u, None).await;
            let _ = busy.try_set(false);
            match r {
                Ok(()) => {
                    toast(Tone::Ok, t(if what == "cancel" { "pca.cancel_sent" } else { "pca.backup_sent" }).to_string());
                    detail.reload();
                    tab.data.reload();
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    };
    let make_code = move |_| {
        let u = format!("{}/restore-code", dev_url(&dev.get_value()));
        error.set(None);
        spawn_local(async move {
            match api::post::<(), RestoreCodeCreated>(&u, None).await {
                Ok(c) => code.set(Some(c)),
                Err(e) => error.set(Some(e.text())),
            }
        });
    };
    let revoke = Callback::new(move |_| {
        let d = dev.get_value();
        run_task(tab, "DELETE", dev_url(&d), tf("pca.revoked", &[("device", &d)]), busy, error, Callback::new(move |_| {
            ask_revoke.set(false);
            detail.reload();
        }));
    });
    let content = move || {
        let Some(d) = detail.data.get() else { return ().into_any() };
        let p = d.device.clone();
        let run = p.last_run.clone();
        let progress = p.progress.clone().map(|pr| {
            let pct = if pr.bytes_total > 0 { (pr.bytes_done as f64 / pr.bytes_total as f64 * 100.0).min(100.0) } else { 0.0 };
            view! {
                <div class="pca-progress">
                    <ui::Meter pct=pct />
                    <span class="small tnum">{format!("{} / {} · {}/s", i18n::bytes(pr.bytes_done), i18n::bytes(pr.bytes_total), i18n::bytes(pr.speed_bps))}</span>
                </div>
            }
        });
        let ready = if p.bare_metal.ready {
            ui::status(Tone::Ok, t("pca.ready.yes").to_string())
        } else {
            view! { <div class="cell-main">{ui::status(Tone::Warn, t("pca.ready.no").to_string())}{p.bare_metal.reasons.iter().map(|r| view! { <span class="cell-sub small">{tr(reason_text(*r))}</span> }).collect_view()}</div> }.into_any()
        };
        let captures = run.as_ref().map(|r| {
            r.captures
                .iter()
                .map(|c| view! { <li><span class="mono">{c.path.clone()}</span>" · "{c.fstype.clone()}" · "{tr(method_text(c.method))}{c.note.clone().map(|n| format!(" ({n})"))}</li> })
                .collect_view()
        });
        let history = d
            .history
            .iter()
            .take(10)
            .map(|h| {
                let what = if h.restore { t("pca.bare_metal").to_string() } else { t("pca.h.backup").to_string() };
                ui::row(vec![
                    ui::text_cell("pca.col.when", "tnum", i18n::datetime(h.started_ms)),
                    ui::text_cell("pca.col.what", "", what),
                    ui::cell("col.status", "wrap", if h.ok { ui::status(Tone::Ok, t("upd.uptodate").to_string()) } else { view! { <div class="cell-main">{ui::status(Tone::Err, t("pca.st.failed").to_string())}<span class="cell-sub small">{h.error.clone().unwrap_or_default()}</span></div> }.into_any() }),
                    ui::text_cell("pca.col.added", "num tnum", if h.restore { "—".into() } else { i18n::bytes(h.bytes_added) }),
                ])
            })
            .collect::<Vec<_>>();
        let running = p.state == PcState::Running;
        let revoked = p.revoked;
        let host = origin();
        view! {
            {p.revoked.then(|| ui::note(Tone::Info, icons::LOCK, ui::text(t("pca.revoked_note").to_string())))}
            <dl class="kvs">
                {kv("col.status", state_view(p.state, p.revoked))}
                {kv("pca.last_ok", ui::text(when(p.last_ok_ms)))}
                {kv("pca.col.group", ui::text(p.group.clone()))}
                {kv("pca.col.versions", ui::text(i18n::count(u64::from(p.versions))))}
                {kv("pca.col.uploaded", ui::text(i18n::bytes(p.uploaded_bytes)))}
                {kv("pca.os", ui::text(format!("{} {} · rynas-agent {}", p.os, p.arch, p.agent_version)))}
                {kv("pca.last_seen", ui::text(when(p.last_seen_ms)))}
                {kv("pca.bare_metal", ready)}
                {run.as_ref().and_then(|r| r.error.clone()).map(|e| kv("pca.last_error", ui::breakable(e.as_str(), "small")))}
                {run.as_ref().filter(|r| r.changed_count > 0).map(|r| kv("pca.changed", ui::text(tf("pca.changed_n", &[("n", &i18n::count(r.changed_count))]))))}
            </dl>
            {progress}
            {captures.map(|c| view! { <details class="tls-step"><summary>{tr("pca.how_read")}</summary><ul class="small">{c}</ul></details> })}
            {run.as_ref().filter(|r| !r.changed_during.is_empty()).map(|r| view! {
                <details class="tls-step"><summary>{tr("pca.changed")}</summary>
                    <p class="small muted">{tr("pca.changed_note")}</p>
                    <ul class="small mono bk-wrap">{r.changed_during.iter().map(|c| view! { <li>{c.clone()}</li> }).collect_view()}</ul>
                </details>
            })}
            {form_error(error)}
            <div class="sec-actions">
                {(!revoked).then(|| view! {
                    <button class="btn primary sm" disabled=move || busy.get() || running on:click=move |_| command("backup-now")><Icon icon=icons::PLAY size=14 />{tr("pca.backup_now")}</button>
                    <button class="btn ghost sm" disabled=move || busy.get() || !running on:click=move |_| command("cancel")><Icon icon=icons::SQUARE size=14 />{tr("pca.cancel")}</button>
                    <button class="btn ghost sm" on:click=move |_| edit.set(true)><Icon icon=icons::SETTINGS size=14 />{tr("bk.settings")}</button>
                })}
                <button class="btn ghost sm" on:click=make_code><Icon icon=icons::LIFE_BUOY size=14 />{tr("pca.restore_code")}</button>
                {(!revoked).then(|| view! { <button class="btn danger sm" on:click=move |_| { error.set(None); ask_revoke.set(true); }><Icon icon=icons::TRASH_2 size=14 />{tr("pca.revoke")}</button> })}
            </div>
            {move || code.get().map(|c| {
                let cmd = format!("curl -fLO https://github.com/rylos/rynas/releases/latest/download/rynas-agent-x86_64\nchmod +x rynas-agent-x86_64\nsudo ./rynas-agent-x86_64 restore --nas {host}");
                view! {
                    {ui::note(Tone::Ok, icons::LIFE_BUOY, ui::text(tf("pca.restore_code_valid", &[("code", &c.code), ("until", &i18n::datetime(c.expires_ms))])))}
                    <p class="small muted">{tr("pca.restore_steps")}</p>
                    <pre class="app-code">{cmd.clone()}</pre>
                    <div class="sec-actions">{copy_button("dns.copy", cmd)}</div>
                }
            })}
            <h3 class="mini-title">{tr("pca.versions")}</h3>
            {move || versions.view(3, move || {
                let list = versions.data.get().unwrap_or_default();
                if list.is_empty() {
                    return view! { <p class="small muted">{tr("pca.no_versions")}</p> }.into_any();
                }
                let rows = list.iter().map(|v| {
                    let id = v.id.clone();
                    let label = format!("{}: {}", t("bk.browse"), v.id);
                    ui::row(vec![
                        ui::text_cell("pca.col.when", "tnum", i18n::datetime(v.time_ms)),
                        ui::text_cell("pca.col.files", "num tnum", v.files.map(i18n::count).unwrap_or_default()),
                        ui::text_cell("pca.col.size", "num tnum", v.bytes.map(i18n::bytes).unwrap_or_default()),
                        ui::actions(view! { <button class="icon-btn sm" title=tr("bk.browse") aria-label=label on:click=move |_| browse.set(Some(id.clone()))><Icon icon=icons::FOLDER_OPEN size=16 /></button> }.into_any()),
                    ])
                }).collect::<Vec<_>>();
                ui::table("pca-versions", &["pca.col.when", "#pca.col.files", "#pca.col.size", ""], rows)
            })}
            {(!history.is_empty()).then(|| view! {
                <h3 class="mini-title">{tr("pca.history")}</h3>
                {ui::table("pca-history", &["pca.col.when", "pca.col.what", "col.status", "#pca.col.added"], history)}
            })}
            {move || edit.get().then(|| device_settings(p.clone(), detail, tab, Callback::new(move |_| edit.set(false))))}
        }
        .into_any()
    };
    let body = view! { {move || detail.view(8, content)} }.into_any();
    let foot = view! { <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("close")}</button> }.into_any();
    view! {
        <div class="wide-drawer">{ui::drawer(Signal::stored(device.clone()), close, body, foot)}</div>
        {move || ask_revoke.get().then(|| ui::confirm(Confirm { busy, error, ..Confirm::new(tf("pca.revoke_title", &[("device", &dev.get_value())]), t("pca.revoke_text").to_string(), "pca.revoke", true, revoke, Callback::new(move |_| ask_revoke.set(false))) }))}
        {move || browse.get().map(|v| browse_drawer(dev.get_value(), v, Callback::new(move |_| browse.set(None))))}
    }
    .into_any()
}

fn device_settings(p: PcDevice, detail: Remote<PcDeviceDetail>, tab: Tab, close: Callback<()>) -> AnyView {
    let s = p.settings.clone();
    let description = RwSignal::new(s.description.clone());
    let interval = RwSignal::new(s.plan.interval_hours.to_string());
    let delay = RwSignal::new(s.plan.start_delay_min.to_string());
    let freeze = RwSignal::new(if s.plan.freeze == FreezeMode::Freeze { "freeze" } else { "hot" }.to_string());
    let freeze_max = RwSignal::new(s.plan.freeze_max_secs.to_string());
    let excludes = RwSignal::new(s.plan.excludes.join("\n"));
    let paused = RwSignal::new(if s.plan.paused { "paused" } else { "on" }.to_string());
    let r = s.retention;
    let (last, daily, weekly, monthly, yearly) = (RwSignal::new(r.last.to_string()), RwSignal::new(r.daily.to_string()), RwSignal::new(r.weekly.to_string()), RwSignal::new(r.monthly.to_string()), RwSignal::new(r.yearly.to_string()));
    let min_age = RwSignal::new(s.min_age_days.to_string());
    let alert = RwSignal::new(s.alert_hours.to_string());
    let critical = RwSignal::new(s.critical_hours.to_string());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let dev = StoredValue::new(p.device.clone());
    let save = move |_| {
        let n = |s: RwSignal<String>| s.get_untracked().trim().parse::<u32>().unwrap_or(0);
        let body = PcDeviceSettings {
            description: description.get_untracked().trim().to_string(),
            plan: PcPlan {
                interval_hours: n(interval),
                start_delay_min: n(delay),
                excludes: excludes.get_untracked().lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect(),
                freeze: if freeze.get_untracked() == "freeze" { FreezeMode::Freeze } else { FreezeMode::Hot },
                freeze_max_secs: n(freeze_max),
                paused: paused.get_untracked() == "paused",
            },
            retention: Retention { last: n(last), hourly: 0, daily: n(daily), weekly: n(weekly), monthly: n(monthly), yearly: n(yearly) },
            min_age_days: n(min_age),
            alert_hours: n(alert),
            critical_hours: n(critical),
        };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = api::put::<_, PcDevice>(&dev_url(&dev.get_value()), &body).await;
            let _ = busy.try_set(false);
            match r {
                Ok(_) => {
                    toast(Tone::Ok, t("bk.saved").to_string());
                    detail.reload();
                    tab.data.reload();
                    close.run(());
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    };
    const FREEZE: &[(&str, &str)] = &[("hot", "pca.f.hot"), ("freeze", "pca.f.freeze")];
    const PAUSED: &[(&str, &str)] = &[("on", "pca.f.scheduled"), ("paused", "pca.f.paused")];
    let body = view! {
        {text_field("bk.f.description", description.into(), Callback::new(move |v| description.set(v)), Signal::stored(None), In { maxlength: 120, ..In::default() })}
        <h3 class="mini-title">{tr("pca.f.schedule")}</h3>
        {select_field("pca.f.state", None, PAUSED, paused.into(), Callback::new(move |v| paused.set(v)))}
        <div class="form-row">
            {number_field("pca.f.interval", Some("pca.f.interval_help"), interval.into(), Callback::new(move |v| interval.set(v)), 1, 168)}
            {number_field("pca.f.delay", Some("pca.f.delay_help"), delay.into(), Callback::new(move |v| delay.set(v)), 0, 1440)}
        </div>
        <h3 class="mini-title">{tr("pca.f.consistency")}</h3>
        {segmented("pca.f.read", FREEZE, freeze.into(), Callback::new(move |v| freeze.set(v)))}
        <p class="field-help">{move || if freeze.get() == "freeze" { t("pca.f.freeze_help") } else { t("pca.f.hot_help") }}</p>
        <Show when=move || freeze.get() == "freeze">
            {number_field("pca.f.freeze_max", Some("pca.f.freeze_max_help"), freeze_max.into(), Callback::new(move |v| freeze_max.set(v)), 5, 3600)}
        </Show>
        {area_field("pca.f.excludes", excludes.into(), Callback::new(move |v| excludes.set(v)), Some("pca.f.excludes_help"), "/home/*/Downloads\n*.iso")}
        <h3 class="mini-title">{tr("pca.f.retention")}</h3>
        <div class="form-row">
            {number_field("pca.f.last", None, last.into(), Callback::new(move |v| last.set(v)), 0, 1000)}
            {number_field("pca.f.daily", None, daily.into(), Callback::new(move |v| daily.set(v)), 0, 1000)}
            {number_field("pca.f.weekly", None, weekly.into(), Callback::new(move |v| weekly.set(v)), 0, 1000)}
        </div>
        <div class="form-row">
            {number_field("pca.f.monthly", None, monthly.into(), Callback::new(move |v| monthly.set(v)), 0, 1000)}
            {number_field("pca.f.yearly", None, yearly.into(), Callback::new(move |v| yearly.set(v)), 0, 100)}
            {number_field("pca.f.min_age", Some("pca.f.min_age_help"), min_age.into(), Callback::new(move |v| min_age.set(v)), 0, 3650)}
        </div>
        <h3 class="mini-title">{tr("pca.f.alerts")}</h3>
        <div class="form-row">
            {number_field("pca.f.alert", Some("pca.f.alert_help"), alert.into(), Callback::new(move |v| alert.set(v)), 0, 8760)}
            {number_field("pca.f.critical", None, critical.into(), Callback::new(move |v| critical.set(v)), 0, 8760)}
        </div>
        {form_error(error)}
    }
    .into_any();
    let foot = view! {
        <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("cancel")}</button>
        <button type="button" class="btn primary" disabled=move || busy.get() on:click=save>{spinner(busy)}{tr("save")}</button>
    }
    .into_any();
    ui::dialog(DlgKind::Normal, Signal::stored(tf("bk.settings_title", &[("device", &p.device)])), close, body, Some(foot))
}

// ------------------------------------------------------------------ browse a version

fn browse_drawer(device: String, version: String, close: Callback<()>) -> AnyView {
    let base = StoredValue::new((dev_url(&device), version.clone()));
    let path = RwSignal::new("/".to_string());
    let listing = Remote::<Vec<NodeEntry>>::new(format!("{}/browse?version={}&path=/", base.with_value(|b| b.0.clone()), api::encode(&version)));
    let selected = RwSignal::new(Vec::<String>::new());
    let go = move |p: String| {
        selected.set(Vec::new());
        path.set(p.clone());
        let (u, v) = base.get_value();
        let url = format!("{u}/browse?version={}&path={}", api::encode(&v), api::encode(&p));
        // a new listing for the new folder
        let r = Remote::<Vec<NodeEntry>>::new(url);
        Effect::new(move |_| {
            if let Some(d) = r.data.get() {
                listing.data.set(Some(d));
            }
            if let Some(e) = r.err.get() {
                listing.err.set(Some(e));
            }
        });
    };
    let up = move |_| {
        let p = path.get_untracked();
        let parent = p.trim_end_matches('/').rsplit_once('/').map(|(a, _)| if a.is_empty() { "/".to_string() } else { a.to_string() }).unwrap_or_else(|| "/".into());
        listing.data.set(None);
        go(parent);
    };
    let link = move |what: &str, paths: &[String]| {
        let (u, v) = base.get_value();
        let mut q = format!("{u}/{what}?version={}", api::encode(&v));
        for p in paths {
            q.push_str(&format!("&path={}", api::encode(p)));
        }
        q
    };
    let entries = move || {
        let list = listing.data.get().unwrap_or_default();
        if list.is_empty() {
            return view! { <p class="muted table-none">{tr("snap.empty_dir")}</p> }.into_any();
        }
        let rows = list
            .into_iter()
            .map(|e| {
                let p = e.path.clone();
                let p2 = p.clone();
                let p3 = p.clone();
                let is_dir = e.dir;
                let checked = move || selected.with(|s| s.contains(&p3));
                let toggle = move |_| selected.update(|s| if let Some(i) = s.iter().position(|x| *x == p2) { s.remove(i); } else { s.push(p2.clone()); });
                let name = e.name.clone();
                let dl = (!is_dir).then(|| link("download", std::slice::from_ref(&p)));
                let label = format!("{}: {}", t("pca.download"), e.name);
                ui::row(vec![
                    ui::cell("snap.col.name", "", view! {
                        <span class="entry-name">
                            <input type="checkbox" aria-label=e.name.clone() prop:checked=checked on:change=toggle />
                            <Icon icon=if is_dir { icons::FOLDER } else { icons::FILE } size=16 />
                            {if is_dir {
                                let p4 = p.clone();
                                view! { <button class="link-strong" on:click=move |_| { listing.data.set(None); go(p4.clone()); }>{name}</button> }.into_any()
                            } else {
                                view! { <span>{name}</span> }.into_any()
                            }}
                        </span>
                    }.into_any()),
                    ui::cell("snap.col.size", "num tnum", view! { {(!is_dir).then(|| i18n::bytes(e.size))} }.into_any()),
                    ui::cell("snap.col.modified", "tnum", view! { {e.mtime_ms.map(i18n::datetime)} }.into_any()),
                    ui::actions(view! { {dl.map(|href| view! { <a class="icon-btn sm" href=href title=tr("pca.download") aria-label=label download=""><Icon icon=icons::DOWNLOAD size=16 /></a> })} }.into_any()),
                ])
            })
            .collect::<Vec<_>>();
        ui::table("pca-browse", &["snap.col.name", "#snap.col.size", "snap.col.modified", ""], rows)
    };
    let zip_href = move || {
        let sel = selected.get();
        let paths = if sel.is_empty() { vec![path.get()] } else { sel };
        link("zip", &paths)
    };
    let body = view! {
        <div class="toolbar browse-bar">
            <button class="icon-btn sm" title=tr("snap.up") aria-label=tr("snap.up") disabled=move || path.with(|p| p == "/") on:click=up><Icon icon=icons::ARROW_UP size=16 /></button>
            <span class="mono small bk-where">{move || path.get()}</span>
            <a class="btn ghost sm" href=zip_href download=""><Icon icon=icons::DOWNLOAD size=14 />{move || if selected.with(Vec::is_empty) { t("pca.zip_folder").to_string() } else { tf("pca.zip_selected", &[("n", &selected.with(|s| s.len()).to_string())]) }}</a>
        </div>
        <p class="small muted">{tr("pca.browse_note")}</p>
        {move || listing.view(6, entries)}
    }
    .into_any();
    let foot = view! { <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("close")}</button> }.into_any();
    let title = Signal::stored(tf("pca.browse_title", &[("device", &device), ("id", &version[..version.len().min(8)])]));
    view! { <div class="wide-drawer">{ui::drawer(title, close, body, foot)}</div> }.into_any()
}
