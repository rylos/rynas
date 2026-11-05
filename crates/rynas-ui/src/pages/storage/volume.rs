//! Volume detail: usage, RAID layout, scrub, encryption (unlock / lock, recovery key, LUKS
//! header backup), subvolumes and the md tuning; grow, replace and scrub start from here.
//!
//! Every change follows the same flow: form → plan from `POST /storage/plan/*` → typed
//! confirmation (volume name) when it erases a disk → the task, followed until it ends.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_params_map;
use rynas_api_types::storage::{
    ScrubState,
    MdArray, MemberRole, RaidLevel, RecoveryKey, StoragePlan, SubvolumeInfo, VolumeSettings, VolumeSettingsState,
};
use rynas_api_types::{TaskInfo, TaskStarted, TaskStatus};
use serde_json::{Value, json};

use crate::tasks::TaskRun;
use super::plan::{plan_view};
use super::{
    Sto, Vol, array_health, disk_title, free_disks, health, level_key, loading, md_level, md_level_text, pct_of,
    serial_of, stale_notice, sync_row, use_refresh, volumes,
};
use crate::api;
use crate::form::{form_error, spinner};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::pages::security::copy_text;
use crate::ui::{self, EmptyState, ErrorBox, Meter, Modal, Notice, Skeleton, Status, Tone, WidgetHead, kv_row, typed_confirm, usage_tone};

// ------------------------------------------------------------------ flow: plan → confirm → task

#[derive(Clone, PartialEq)]
pub enum Stage {
    Form,
    Plan(StoragePlan),
    Running(String),
    Done(TaskInfo),
}

/// POST a planner request; the answer becomes the shown plan.
pub fn fetch_plan(url: &'static str, body: Value, stage: RwSignal<Stage>, busy: RwSignal<bool>, error: RwSignal<Option<String>>) {
    busy.set(true);
    error.set(None);
    spawn_local(async move {
        let r = api::post::<_, StoragePlan>(url, Some(&body)).await;
        let _ = busy.try_set(false);
        match r {
            Ok(p) => {
                let _ = stage.try_set(Stage::Plan(p));
            }
            Err(e) => {
                let _ = error.try_set(Some(e.text()));
            }
        }
    });
}

/// POST an operation that answers 202 `TaskStarted` (also DELETE with `delete`).
pub fn start_task(url: String, body: Option<Value>, delete: bool, stage: RwSignal<Stage>, busy: RwSignal<bool>, error: RwSignal<Option<String>>) {
    busy.set(true);
    error.set(None);
    spawn_local(async move {
        let r = if delete { api::delete::<TaskStarted>(&url).await } else { api::post::<_, TaskStarted>(&url, body.as_ref()).await };
        let _ = busy.try_set(false);
        match r {
            Ok(t) => {
                let _ = stage.try_set(Stage::Running(t.task_id));
            }
            Err(e) => {
                let _ = error.try_set(Some(e.text()));
            }
        }
    });
}

/// The stages after the form: the plan with its confirmation, the running task, the outcome.
/// `exec` gets the typed passphrase (empty when none is asked).
#[allow(clippy::too_many_arguments)]
fn flow_view(
    stage: RwSignal<Stage>,
    confirm_name: Option<String>,
    need_pass: bool,
    exec_key: &'static str,
    busy: RwSignal<bool>,
    error: RwSignal<Option<String>>,
    exec: Callback<String>,
    on_close: Callback<()>,
) -> AnyView {
    let typed = RwSignal::new(String::new());
    let pass = RwSignal::new(String::new());
    let s = super::sto();
    let confirm_name = StoredValue::new(confirm_name);
    (move || match stage.get() {
        Stage::Form => ().into_any(),
        Stage::Plan(p) => {
            let valid = p.valid;
            let destructive = p.steps.iter().any(|s| !s.destroys.is_empty());
            let need_typed = destructive && confirm_name.with_value(Option::is_some);
            let ok = move || {
                valid
                    && (!need_typed || confirm_name.with_value(|n| n.as_deref() == Some(typed.get().trim())))
                    && (!need_pass || pass.with(|p| p.chars().count() >= 8))
            };
            view! {
                {plan_view(&p)}
                {(valid && need_typed).then(|| typed_confirm(confirm_name.get_value().unwrap_or_default(), typed))}
                {(valid && need_pass).then(|| view! {
                    <label class="field">
                        <span class="field-label">{tr("sto.passphrase")}</span>
                        <input class="input" type="password" autocomplete="off" prop:value=move || pass.get() on:input=move |ev| pass.set(event_target_value(&ev)) />
                    </label>
                })}
                {form_error(error)}
                <div class="dialog-foot">
                    <button class="btn ghost" type="button" on:click=move |_| stage.set(Stage::Form)>{tr("back")}</button>
                    <button
                        class=if destructive { "btn danger" } else { "btn primary" }
                        type="button"
                        disabled=move || !ok() || busy.get()
                        on:click=move |_| exec.run(pass.get_untracked())
                    >
                        {spinner(busy)}{tr(exec_key)}
                    </button>
                </div>
            }
            .into_any()
        }
        Stage::Running(id) => view! {
            <TaskRun id=id on_done=Callback::new(move |t: TaskInfo| {
                s.load();
                stage.set(Stage::Done(t));
            }) />
        }
        .into_any(),
        Stage::Done(t) => done_view(&t, on_close),
    })
    .into_any()
}

fn done_view(t: &TaskInfo, on_close: Callback<()>) -> AnyView {
    let ok = t.status == TaskStatus::Ok;
    let (tone, key) = if ok { (Tone::Ok, "sto.done_ok") } else { (Tone::Err, "sto.done_err") };
    let err = t.error.clone();
    view! {
        {ui::note(tone, tone.icon(), view! { {tr(key)} }.into_any())}
        {err.map(|e| view! { <p class="form-error"><Icon icon=icons::CIRCLE_X size=16 /><span>{e}</span></p> })}
        <div class="dialog-foot"><button class="btn primary" type="button" on:click=move |_| on_close.run(())>{tr("close")}</button></div>
    }
    .into_any()
}

fn vol_url(id: &str, what: &str) -> String {
    format!("/api/v1/storage/volumes/{}/{what}", api::encode(id))
}

// ------------------------------------------------------------------ page

/// Parts of the page that keep their own state (forms, downloads, lists): rebuilt only when
/// these change, not on every inventory refresh.
#[derive(Clone, PartialEq)]
struct Stable {
    id: String,
    locked: bool,
    managed: bool,
    encrypted: bool,
    subvolumes: bool,
    raid56: bool,
}

#[component]
pub fn VolumePage() -> impl IntoView {
    let s = use_refresh();
    let params = use_params_map();
    let id = Memo::new(move |_| params.with(|p| p.get("name").unwrap_or_default()));
    let vol = Memo::new(move |_| {
        let id = id.get();
        volumes(s).into_iter().find(|v| v.id == id)
    });
    let stable = Memo::new(move |_| {
        vol.with(|v| {
            v.as_ref().map(|v| Stable {
                id: v.id.clone(),
                locked: v.locked,
                managed: v.managed,
                encrypted: v.encrypted,
                subvolumes: v.managed && !v.locked && v.fstype == "btrfs",
                raid56: v.managed
                    && !v.locked
                    && v.md.as_ref().and_then(md_level).is_some_and(|l| matches!(l, RaidLevel::Raid5 | RaidLevel::Raid6)),
            })
        })
    });
    // dialogs and the drive drawer live here, so a refresh of the data never closes them
    let modal = RwSignal::new(Open::None);
    let selected = RwSignal::new(None::<String>);
    let close = Callback::new(move |_| modal.set(Open::None));
    let current = move || vol.get_untracked().unwrap_or_default();
    view! {
        <div class="page">
            <Show when=move || s.ready() fallback=move || loading(s, 8)>
                {move || match vol.get() {
                    None => view! {
                        <div class="card soon">
                            <EmptyState icon=icons::DATABASE title=Signal::derive(|| t("sto.vol_missing").to_string()) text=Signal::derive(|| t("sto.vol_missing_text").to_string())>
                                <a class="btn ghost" href="/storage">{tr("sto.back_storage")}</a>
                            </EmptyState>
                        </div>
                    }
                    .into_any(),
                    Some(v) => volume_view(v, modal, selected),
                }}
                {move || stable.get().map(|st| view! {
                    <div class="grid vol-detail">
                        {st.encrypted.then(|| crypt_card(&st.id, st.locked, st.managed, modal))}
                        {st.raid56.then(|| view! { <StripeCache id=st.id.clone() /> })}
                        {st.subvolumes.then(|| view! { <Subvolumes id=st.id.clone() /> })}
                    </div>
                })}
            </Show>
            {move || match modal.get() {
                Open::None => ().into_any(),
                Open::Scrub => view! { <ScrubDialog vol=current() on_close=close /> }.into_any(),
                Open::Grow => view! { <GrowDialog vol=current() on_close=close /> }.into_any(),
                Open::Replace => view! { <ReplaceDialog vol=current() on_close=close /> }.into_any(),
                Open::Unlock => view! { <UnlockDialog id=current().id on_close=close /> }.into_any(),
                Open::Lock => view! { <LockDialog id=current().id on_close=close /> }.into_any(),
                Open::Reveal => view! { <RevealDialog id=current().id on_close=close /> }.into_any(),
            }}
            {move || selected.get().map(|name| view! { <super::disks::DiskDrawer name=name on_close=Callback::new(move |_| selected.set(None)) /> })}
        </div>
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Open {
    None,
    Scrub,
    Grow,
    Replace,
    Unlock,
    Lock,
    Reveal,
}

fn volume_view(vol: Vol, modal: RwSignal<Open>, selected: RwSignal<Option<String>>) -> AnyView {
    let s = super::sto();
    let (tone, key) = health(vol.health);
    let level = vol.md.as_ref().map(md_level_text);
    let sub = [
        vol.mountpoint.clone(),
        level.clone(),
        (vol.size > 0).then(|| if vol.locked { i18n::bytes_si(vol.size) } else { i18n::bytes(vol.size) }),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    let actions = vol.managed && !vol.locked;
    let has_md = vol.md.is_some();
    let btn = move |m: Open, icon: &'static str, key: &'static str| {
        view! { <button class="btn ghost" type="button" on:click=move |_| modal.set(m)><Icon icon=icon size=16 /><span class="btn-label">{tr(key)}</span></button> }
    };
    view! {
        <header class="page-head">
            <a class="icon-btn" href="/storage" aria-label=tr("sto.back_storage")><Icon icon=icons::ARROW_LEFT size=18 /></a>
            <div class="page-icon"><Icon icon=if vol.locked { icons::KEY } else { icons::DATABASE } size=20 /></div>
            <div class="grow">
                <h1 class="head-line">{vol.id.clone()}<Status tone=tone text=t(key).to_string() /></h1>
                <p class="page-sub">{sub}</p>
            </div>
            {actions.then(|| view! {
                <div class="page-actions">
                    {btn(Open::Scrub, icons::SHIELD_CHECK, "sto.scrub")}
                    {has_md.then(|| btn(Open::Grow, icons::PLUS, "sto.grow"))}
                    {has_md.then(|| btn(Open::Replace, icons::REFRESH_CW, "sto.replace"))}
                </div>
            })}
        </header>
        {stale_notice(s)}
        {(!vol.managed).then(|| view! { {ui::note(Tone::Info, icons::INFO, view! { {tr("sto.unmanaged")} }.into_any())} })}
        <div class="grid vol-detail">
            {(!vol.locked).then(|| usage_card(&vol))}
            {vol.md.clone().map(|a| raid_card(a, selected))}
            {(!vol.locked && vol.fs.is_some()).then(|| scrub_card(&vol, modal))}
        </div>
    }
    .into_any()
}

fn sig(v: String) -> Signal<String> {
    Signal::derive(move || v.clone())
}

fn usage_card(v: &Vol) -> AnyView {
    let pct = pct_of(v.used, v.size);
    let tone = usage_tone(pct);
    let fs = v.fs.clone().unwrap_or_default();
    let free = if v.fs.is_some() { fs.free_estimated } else { v.size.saturating_sub(v.used) };
    let profiles = fs
        .profiles
        .iter()
        .filter(|p| p.kind != "system")
        .map(|p| format!("{} {}", i18n::try_t(&format!("sto.prof.{}", p.kind)).unwrap_or(&p.kind), p.profile.to_uppercase()))
        .collect::<Vec<_>>()
        .join(" · ");
    let quota = match fs.quota {
        rynas_api_types::storage::QuotaMode::None => t("sto.off").to_string(),
        rynas_api_types::storage::QuotaMode::Qgroup => "qgroups".into(),
        rynas_api_types::storage::QuotaMode::Squota => "squota".into(),
        rynas_api_types::storage::QuotaMode::Unknown => "—".into(),
    };
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::DATABASE title="sto.usage" />
            {(v.size > 0 && v.size >= v.used).then(|| view! {
                <div class="kpi"><span class="kpi-num tnum">{i18n::pct(pct)}</span><span class="kpi-unit">{tf("sto.of", &[("n", &i18n::bytes(v.size))])}</span></div>
                <Meter pct=pct tone=Signal::derive(move || tone) />
            })}
            <dl class="kvs">
                {kv_row("sto.used", sig(i18n::bytes(v.used)), "tnum")}
                {kv_row("sto.free", sig(if v.size == 0 { "—".into() } else { i18n::bytes(free) }), "tnum")}
                {kv_row("sto.fs", sig(v.fstype.clone()), "")}
                {(!profiles.is_empty()).then(|| kv_row("sto.profiles", sig(profiles), ""))}
                {kv_row("sto.compression", sig(fs.compression.clone().unwrap_or_else(|| t("sto.off").to_string())), "mono")}
                {v.fs.is_some().then(|| kv_row("sto.quota", sig(quota), ""))}
                {kv_row("sto.mount", sig(v.mountpoint.clone().unwrap_or_else(|| "—".into())), "mono")}
            </dl>
        </section>
    }
    .into_any()
}

fn raid_card(a: MdArray, selected: RwSignal<Option<String>>) -> AnyView {
    let (tone, key) = array_health(a.health);
    let s = super::sto();
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::LAYERS title="sto.raid"><Status tone=tone text=t(key).to_string() /></WidgetHead>
            <dl class="kvs">
                {kv_row("sto.level", sig(md_level_text(&a)), "")}
                {kv_row("sto.md_state", sig(a.state.clone()), "")}
                {kv_row("sto.array", sig(a.md_name.clone().map(|n| format!("{} ({n})", a.name)).unwrap_or_else(|| a.name.clone())), "mono")}
                {kv_row("sto.size", sig(i18n::bytes_si(a.size)), "tnum")}
                {a.mismatch_cnt.map(|m| kv_row("sto.mismatch", sig(m.to_string()), "tnum"))}
            </dl>
            {a.sync.as_ref().map(sync_row)}
            <ul class="members-list">
                {a.members.iter().map(|m| {
                    let (rtone, rkey) = match m.role {
                        MemberRole::Active => (Tone::Ok, "sto.r.active"),
                        MemberRole::Spare => (Tone::Info, "sto.r.spare"),
                        MemberRole::Faulty => (Tone::Err, "sto.r.faulty"),
                        MemberRole::Rebuilding => (Tone::Info, "sto.r.rebuilding"),
                        MemberRole::Journal => (Tone::Info, "sto.r.journal"),
                        MemberRole::Unknown => (Tone::Unknown, "health.unknown"),
                    };
                    let disk = m.disk.clone().unwrap_or_else(|| m.device.clone());
                    let info = s.disk(&disk);
                    let d2 = disk.clone();
                    view! {
                        <li>
                            <button class="cell-btn" type="button" on:click=move |_| selected.set(Some(d2.clone()))>
                                <span class="mono strong">{m.device.clone()}</span>
                                <span class="muted small ellipsis">{info.map(|d| disk_title(&d)).unwrap_or(disk)}</span>
                            </button>
                            <span class="row-end"><Status tone=rtone text=t(rkey).to_string() /></span>
                        </li>
                    }
                }).collect_view()}
                {(a.degraded > 0).then(|| view! {
                    <li class="warn tone-text small"><Icon icon=icons::TRIANGLE_ALERT size=14 />{tf("sto.missing_members", &[("n", &a.degraded.to_string())])}</li>
                })}
            </ul>
        </section>
    }
    .into_any()
}

fn scrub_card(v: &Vol, modal: RwSignal<Open>) -> AnyView {
    let sc = v.fs.as_ref().and_then(|f| f.scrub.clone());
    let running = sc.as_ref().is_some_and(|s| s.status == ScrubState::Running);
    let id = v.id.clone();
    let managed = v.managed;
    let stage = RwSignal::new(Stage::Form);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let stop = move |_| {
        start_task(vol_url(&id, "scrub"), Some(json!({"action": "stop", "md_check": false, "dry_run": false})), false, stage, busy, error);
    };
    let (tone, key) = match sc.as_ref().map(|s| s.status) {
        Some(ScrubState::Finished) if sc.as_ref().is_some_and(|s| s.errors > 0) => (Tone::Err, "sto.scrub_errors"),
        Some(ScrubState::Finished) => (Tone::Ok, "sto.scrub_finished"),
        Some(ScrubState::Running) => (Tone::Info, "sto.scrub_running"),
        Some(ScrubState::Aborted | ScrubState::Interrupted) => (Tone::Warn, "sto.scrub_aborted"),
        _ => (Tone::Unknown, "never_run"),
    };
    let s2 = sc.clone().unwrap_or_default();
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::SHIELD_CHECK title="sto.scrub"><Status tone=tone text=t(key).to_string() /></WidgetHead>
            <p class="muted small">{tr("sto.scrub_text")}</p>
            {sc.map(|_| view! {
                <dl class="kvs">
                    {kv_row("sto.started", sig(s2.started.clone().unwrap_or_else(|| "—".into())), "")}
                    {kv_row("sto.duration", sig(s2.duration_s.map(i18n::duration).unwrap_or_else(|| "—".into())), "tnum")}
                    {s2.pct.filter(|_| running).map(|p| kv_row("sto.progress", sig(i18n::pct(p as f64)), "tnum"))}
                    {kv_row("sto.scrub_result", sig(s2.error_summary.clone()), "")}
                </dl>
            })}
            {managed.then(|| view! {
                <div class="card-actions">
                    {if running {
                        view! { <button class="btn ghost sm" type="button" disabled=move || busy.get() on:click=stop>{spinner(busy)}{tr("sto.scrub_stop")}</button> }.into_any()
                    } else {
                        view! { <button class="btn ghost sm" type="button" on:click=move |_| modal.set(Open::Scrub)><Icon icon=icons::SHIELD_CHECK size=14 />{tr("run_now")}</button> }.into_any()
                    }}
                </div>
            })}
            {form_error(error)}
            {move || match stage.get() {
                Stage::Running(id) => view! { <TaskRun id=id on_done=Callback::new(move |_| { super::sto().load(); stage.set(Stage::Form); }) /> }.into_any(),
                _ => ().into_any(),
            }}
        </section>
    }
    .into_any()
}

fn crypt_card(id: &str, locked: bool, managed: bool, modal: RwSignal<Open>) -> AnyView {
    let id = StoredValue::new(id.to_string());
    let busy = RwSignal::new(false);
    let msg = RwSignal::new(None::<(Tone, String)>);
    let downloaded = RwSignal::new(false);
    let download = move |_| {
        busy.set(true);
        msg.set(None);
        spawn_local(async move {
            let id = id.get_value();
            let r = api::download(&vol_url(&id, "luks-header"), &format!("luks-header-{id}.img")).await;
            let _ = busy.try_set(false);
            match r {
                Ok(()) => {
                    let _ = downloaded.try_set(true);
                    let _ = msg.try_set(Some((Tone::Ok, t("sto.hdr_saved").to_string())));
                }
                Err(e) if e.status() == 404 => {
                    let _ = msg.try_set(Some((Tone::Info, t("sto.hdr_none").to_string())));
                }
                Err(e) => {
                    let _ = msg.try_set(Some((Tone::Err, e.text())));
                }
            }
        });
    };
    let delete = move |_| {
        busy.set(true);
        spawn_local(async move {
            let r = api::delete_empty(&vol_url(&id.get_value(), "luks-header")).await;
            let _ = busy.try_set(false);
            let _ = downloaded.try_set(false);
            let _ = msg.try_set(Some(match r {
                Ok(()) => (Tone::Ok, t("sto.hdr_deleted").to_string()),
                Err(e) => (Tone::Err, e.text()),
            }));
        });
    };
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::KEY title="sto.encryption">
                {if locked {
                    view! { <Status tone=Tone::Info text=t("sto.locked").to_string() /> }.into_any()
                } else {
                    view! { <Status tone=Tone::Ok text=t("sto.unlocked").to_string() /> }.into_any()
                }}
            </WidgetHead>
            <p class="muted small">{if locked { t("sto.locked_text") } else { t("sto.crypt_text") }}</p>
            {managed.then(|| view! {
                <div class="card-actions">
                    {if locked {
                        view! { <button class="btn primary sm" type="button" on:click=move |_| modal.set(Open::Unlock)><Icon icon=icons::KEY size=14 />{tr("sto.unlock")}</button> }.into_any()
                    } else {
                        view! { <button class="btn ghost sm" type="button" on:click=move |_| modal.set(Open::Lock)>{tr("sto.lock")}</button> }.into_any()
                    }}
                    <button class="btn ghost sm" type="button" on:click=move |_| modal.set(Open::Reveal)><Icon icon=icons::KEY size=14 />{tr("sto.rk_reveal")}</button>
                    <button class="btn ghost sm" type="button" disabled=move || busy.get() on:click=download><Icon icon=icons::DOWNLOAD size=14 />{tr("sto.hdr_download")}</button>
                    {move || downloaded.get().then(|| view! {
                        <button class="btn ghost sm" type="button" disabled=move || busy.get() on:click=delete>{tr("sto.hdr_delete")}</button>
                    })}
                </div>
            })}
            <Notice msg=msg />
        </section>
    }
    .into_any()
}

// ------------------------------------------------------------------ dialogs

#[component]
pub fn UnlockDialog(id: String, on_close: Callback<()>) -> impl IntoView {
    let stage = RwSignal::new(Stage::Form);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let pass = RwSignal::new(String::new());
    let url = StoredValue::new(vol_url(&id, "unlock"));
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let p = pass.get_untracked();
        if p.is_empty() {
            return;
        }
        start_task(url.get_value(), Some(json!({ "passphrase": p })), false, stage, busy, error);
        pass.set(String::new());
    };
    let title = tf("sto.unlock_title", &[("name", &id)]);
    view! {
        <Modal title=title on_close=on_close>
            {move || match stage.get() {
                Stage::Form => view! {
                    <form class="form" on:submit=submit>
                        <p class="muted small">{tr("sto.unlock_text")}</p>
                        <label class="field">
                            <span class="field-label">{tr("sto.pass_or_key")}</span>
                            <input class="input" type="password" autocomplete="off" prop:value=move || pass.get() on:input=move |ev| pass.set(event_target_value(&ev)) />
                        </label>
                        {form_error(error)}
                        <div class="dialog-foot">
                            <button class="btn ghost" type="button" on:click=move |_| on_close.run(())>{tr("cancel")}</button>
                            <button class="btn primary" type="submit" disabled=move || busy.get() || pass.with(String::is_empty)>{spinner(busy)}{tr("sto.unlock")}</button>
                        </div>
                    </form>
                }
                .into_any(),
                _ => flow_view(stage, None, false, "sto.unlock", busy, error, Callback::new(|_| {}), on_close),
            }}
        </Modal>
    }
}

#[component]
fn LockDialog(id: String, on_close: Callback<()>) -> impl IntoView {
    let stage = RwSignal::new(Stage::Form);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let url = StoredValue::new(vol_url(&id, "lock"));
    view! {
        <Modal title=tf("sto.lock_title", &[("name", &id)]) on_close=on_close>
            {move || match stage.get() {
                Stage::Form => view! {
                    <p>{tr("sto.lock_text")}</p>
                    {form_error(error)}
                    <div class="dialog-foot">
                        <button class="btn ghost" type="button" on:click=move |_| on_close.run(())>{tr("cancel")}</button>
                        <button class="btn primary" type="button" disabled=move || busy.get()
                            on:click=move |_| start_task(url.get_value(), None, false, stage, busy, error)>
                            {spinner(busy)}{tr("sto.lock")}
                        </button>
                    </div>
                }
                .into_any(),
                _ => flow_view(stage, None, false, "sto.lock", busy, error, Callback::new(|_| {}), on_close),
            }}
        </Modal>
    }
}


/// The recovery key of a volume created encrypted: handed out once by rynasd (kept in memory
/// for one hour after the creation). Asking for it needs the typed volume name.
#[component]
pub fn RevealDialog(id: String, on_close: Callback<()>) -> impl IntoView {
    let typed = RwSignal::new(String::new());
    let key = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let stored = RwSignal::new(false);
    let copied = RwSignal::new(false);
    let idv = StoredValue::new(id.clone());
    let reveal = move |_| {
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = api::post::<(), RecoveryKey>(&vol_url(&idv.get_value(), "reveal-recovery-key"), None).await;
            let _ = busy.try_set(false);
            match r {
                Ok(k) => {
                    let _ = key.try_set(Some(k.key));
                }
                Err(e) if e.status() == 404 => {
                    let _ = error.try_set(Some(t("sto.rk_gone").to_string()));
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    };
    // the key must not linger in memory after the dialog
    on_cleanup(move || {
        let _ = key.try_set(None);
    });
    view! {
        <Modal title=tf("sto.rk_title", &[("name", &id)]) on_close=on_close>
            {move || match key.get() {
                None => view! {
                    <p class="muted small">{tr("sto.rk_text")}</p>
                    {typed_confirm(idv.get_value(), typed)}
                    {form_error(error)}
                    <div class="dialog-foot">
                        <button class="btn ghost" type="button" on:click=move |_| on_close.run(())>{tr("cancel")}</button>
                        <button class="btn primary" type="button" disabled=move || busy.get() || idv.with_value(|i| typed.get().trim() != i) on:click=reveal>
                            {spinner(busy)}{tr("sto.rk_reveal")}
                        </button>
                    </div>
                }
                .into_any(),
                Some(k) => {
                    let k2 = k.clone();
                    let k3 = k.clone();
                    view! {
                        {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tr("sto.rk_once")} }.into_any())}
                        <code class="secret mono rk">{k}</code>
                        <div class="card-actions">
                            <button class="btn ghost sm" type="button" on:click=move |_| { if copy_text(&k2) { copied.set(true); } }>
                                <Icon icon=if copied.get() { icons::CHECK } else { icons::COPY } size=14 />{move || if copied.get() { t("copied") } else { t("sto.copy") }}
                            </button>
                            <button class="btn ghost sm" type="button" on:click=move |_| api::save_text(&format!("recovery-key-{}.txt", idv.get_value()), &k3)>
                                <Icon icon=icons::DOWNLOAD size=14 />{tr("sto.download")}
                            </button>
                        </div>
                        <label class="check"><input type="checkbox" prop:checked=move || stored.get() on:change=move |ev| stored.set(event_target_checked(&ev)) />{tr("sto.rk_stored")}</label>
                        <div class="dialog-foot">
                            <button class="btn primary" type="button" disabled=move || !stored.get() on:click=move |_| on_close.run(())>{tr("sto.done")}</button>
                        </div>
                    }
                    .into_any()
                }
            }}
        </Modal>
    }
}

#[component]
fn ScrubDialog(vol: Vol, on_close: Callback<()>) -> impl IntoView {
    let stage = RwSignal::new(Stage::Form);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let md_check = RwSignal::new(vol.md.is_some());
    let has_md = vol.md.is_some();
    let uuid = vol.fs.as_ref().map(|f| f.uuid.clone()).unwrap_or_default();
    let exec_url = StoredValue::new(vol_url(&vol.id, "scrub"));
    let exec = move |_: String| {
        start_task(exec_url.get_value(), Some(json!({"action": "start", "md_check": md_check.get_untracked(), "dry_run": false})), false, stage, busy, error)
    };
    let uuid = StoredValue::new(uuid);
    view! {
        <Modal title=tf("sto.scrub_title", &[("name", &vol.id)]) on_close=on_close>
            {move || match stage.get() {
                Stage::Form => view! {
                    <p class="muted small">{tr("sto.scrub_text")}</p>
                    {has_md.then(|| view! {
                        <label class="check"><input type="checkbox" prop:checked=move || md_check.get() on:change=move |ev| md_check.set(event_target_checked(&ev)) />{tr("sto.md_check")}</label>
                    })}
                    {form_error(error)}
                    <div class="dialog-foot">
                        <button class="btn ghost" type="button" on:click=move |_| on_close.run(())>{tr("cancel")}</button>
                        <button class="btn primary" type="button" disabled=move || busy.get()
                            on:click=move |_| fetch_plan("/api/v1/storage/plan/scrub", json!({"filesystem": uuid.get_value(), "md_check": md_check.get_untracked()}), stage, busy, error)>
                            {spinner(busy)}{tr("sto.review_plan")}
                        </button>
                    </div>
                }
                .into_any(),
                _ => flow_view(stage, None, false, "sto.scrub_start", busy, error, Callback::new(exec), on_close),
            }}
        </Modal>
    }
}

/// Free disks as a checklist (grow) — shared look with the wizard.
pub fn disk_checklist(disks: Vec<rynas_api_types::storage::DiskSummary>, chosen: RwSignal<Vec<String>>) -> AnyView {
    if disks.is_empty() {
        return view! { <p class="muted">{tr("sto.no_free")}</p> }.into_any();
    }
    view! {
        <ul class="pick-list">
            {disks.into_iter().map(|d| {
                let name = d.name.clone();
                let n2 = name.clone();
                let (htone, hkey) = health(d.health);
                view! {
                    <li>
                        <label class="pick">
                            <input type="checkbox"
                                prop:checked=move || chosen.with(|c| c.contains(&name))
                                on:change=move |ev| {
                                    let on = event_target_checked(&ev);
                                    chosen.update(|c| { c.retain(|x| x != &n2); if on { c.push(n2.clone()); } });
                                } />
                            <span class="pick-main">
                                <span class="strong">{disk_title(&d)}</span>
                                <span class="muted small mono">{d.serial.clone()}</span>
                            </span>
                            {d.smr.then(|| view! { <span class="chip warn-chip" title=t("sto.smr_hint")>"SMR"</span> })}
                            {(!d.used_by.is_empty()).then(|| {
                                let what = d.used_by.join(", ");
                                view! { <span class="chip warn-chip" title=tf("wiz.will_erase", &[("what", &what)])>{tr("wiz.erase_chip")}</span> }
                            })}
                            <Status tone=htone text=t(hkey).to_string() />
                        </label>
                    </li>
                }
            }).collect_view()}
        </ul>
    }
    .into_any()
}

fn serials_for(s: Sto, names: &[String]) -> Vec<String> {
    names.iter().map(|n| s.disk(n).map(|d| serial_of(&d)).unwrap_or_else(|| n.clone())).collect()
}

const LEVELS: [RaidLevel; 6] = [RaidLevel::Single, RaidLevel::Raid1, RaidLevel::Raid5, RaidLevel::Raid6, RaidLevel::Raid10, RaidLevel::Raid0];

fn level_str(l: RaidLevel) -> &'static str {
    match l {
        RaidLevel::Single => "single",
        RaidLevel::Raid0 => "raid0",
        RaidLevel::Raid1 => "raid1",
        RaidLevel::Raid5 => "raid5",
        RaidLevel::Raid6 => "raid6",
        RaidLevel::Raid10 => "raid10",
    }
}

pub fn level_of(s: &str) -> Option<RaidLevel> {
    LEVELS.into_iter().find(|l| level_str(*l) == s)
}

pub fn level_json(l: RaidLevel) -> Value {
    Value::String(level_str(l).into())
}

#[component]
fn GrowDialog(vol: Vol, on_close: Callback<()>) -> impl IntoView {
    let s = super::sto();
    let stage = RwSignal::new(Stage::Form);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let chosen = RwSignal::new(Vec::<String>::new());
    let level = RwSignal::new(String::new());
    let md = vol.md.clone().unwrap_or_default();
    let current = md_level(&md);
    let array = StoredValue::new(md.name.clone());
    let exec_url = StoredValue::new(vol_url(&vol.id, "grow"));
    let id = StoredValue::new(vol.id.clone());
    let lvl = move || level_of(&level.get_untracked());
    let exec = move |pass: String| {
        let disks = chosen.get_untracked();
        let mut body = json!({
            "disks": disks, "confirm": id.get_value(), "serials": serials_for(s, &disks), "dry_run": false,
        });
        if let Some(l) = lvl() {
            body["level"] = level_json(l);
        }
        if !pass.is_empty() {
            body["passphrase"] = Value::String(pass);
        }
        start_task(exec_url.get_value(), Some(body), false, stage, busy, error);
    };
    let review = move |_| {
        let mut body = json!({ "array": array.get_value(), "disks": chosen.get_untracked() });
        if let Some(l) = lvl() {
            body["level"] = level_json(l);
        }
        fetch_plan("/api/v1/storage/plan/grow", body, stage, busy, error);
    };
    let encrypted = vol.encrypted;
    view! {
        <Modal title=tf("sto.grow_title", &[("name", &vol.id)]) on_close=on_close wide=true>
            {move || match stage.get() {
                Stage::Form => view! {
                    <p class="muted small">{tr("sto.grow_text")}</p>
                    {disk_checklist(free_disks(s), chosen)}
                    <label class="field">
                        <span class="field-label">{tr("sto.level")}</span>
                        <select class="input" prop:value=move || level.get() on:change=move |ev| level.set(event_target_value(&ev))>
                            <option value="">{tf("sto.keep_level", &[("l", current.map(|l| t(level_key(l))).unwrap_or("—"))])}</option>
                            {LEVELS.iter().filter(|l| Some(**l) != current && **l != RaidLevel::Single).map(|l| view! {
                                <option value=level_str(*l)>{tr(level_key(*l))}</option>
                            }).collect_view()}
                        </select>
                    </label>
                    {form_error(error)}
                    <div class="dialog-foot">
                        <button class="btn ghost" type="button" on:click=move |_| on_close.run(())>{tr("cancel")}</button>
                        <button class="btn primary" type="button" disabled=move || busy.get() || chosen.with(Vec::is_empty) on:click=review>
                            {spinner(busy)}{tr("sto.review_plan")}
                        </button>
                    </div>
                }
                .into_any(),
                _ => flow_view(stage, Some(id.get_value()), encrypted, "sto.grow", busy, error, Callback::new(exec), on_close),
            }}
        </Modal>
    }
}

#[component]
fn ReplaceDialog(vol: Vol, on_close: Callback<()>) -> impl IntoView {
    let s = super::sto();
    let stage = RwSignal::new(Stage::Form);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let md = vol.md.clone().unwrap_or_default();
    let mut members = md.members.clone();
    members.sort_by_key(|m| m.role != MemberRole::Faulty);
    let old = RwSignal::new(members.first().map(|m| m.device.clone()).unwrap_or_default());
    let new_disk = RwSignal::new(String::new());
    let array = StoredValue::new(md.name.clone());
    let exec_url = StoredValue::new(vol_url(&vol.id, "replace"));
    let id = StoredValue::new(vol.id.clone());
    let free = free_disks(s);
    if let Some(d) = free.first() {
        new_disk.set(d.name.clone());
    }
    let exec = move |_: String| {
        let nd = new_disk.get_untracked();
        start_task(
            exec_url.get_value(),
            Some(json!({"old": old.get_untracked(), "new_disk": nd, "confirm": id.get_value(), "serials": serials_for(s, std::slice::from_ref(&nd)), "dry_run": false})),
            false,
            stage,
            busy,
            error,
        );
    };
    let review = move |_| {
        fetch_plan(
            "/api/v1/storage/plan/replace",
            json!({"array": array.get_value(), "old": old.get_untracked(), "new_disk": new_disk.get_untracked()}),
            stage,
            busy,
            error,
        );
    };
    let members = StoredValue::new(members);
    let free = StoredValue::new(free);
    view! {
        <Modal title=tf("sto.replace_title", &[("name", &vol.id)]) on_close=on_close wide=true>
            {move || match stage.get() {
                Stage::Form => view! {
                    <p class="muted small">{tr("sto.replace_text")}</p>
                    <label class="field">
                        <span class="field-label">{tr("sto.replace_old")}</span>
                        <select class="input" prop:value=move || old.get() on:change=move |ev| old.set(event_target_value(&ev))>
                            {members.get_value().into_iter().map(|m| {
                                let role = format!("{:?}", m.role).to_lowercase();
                                let label = format!("{} · {}", m.device, i18n::try_t(&format!("sto.r.{role}")).unwrap_or(&role));
                                view! { <option value=m.device>{label}</option> }
                            }).collect_view()}
                        </select>
                    </label>
                    <label class="field">
                        <span class="field-label">{tr("sto.replace_new")}</span>
                        <select class="input" prop:value=move || new_disk.get() on:change=move |ev| new_disk.set(event_target_value(&ev))>
                            {free.get_value().into_iter().map(|d| view! { <option value=d.name.clone()>{disk_title(&d)}</option> }).collect_view()}
                        </select>
                    </label>
                    {free.with_value(Vec::is_empty).then(|| view! { <p class="muted small">{tr("sto.no_free")}</p> })}
                    {form_error(error)}
                    <div class="dialog-foot">
                        <button class="btn ghost" type="button" on:click=move |_| on_close.run(())>{tr("cancel")}</button>
                        <button class="btn primary" type="button" disabled=move || busy.get() || new_disk.with(String::is_empty) on:click=review>
                            {spinner(busy)}{tr("sto.review_plan")}
                        </button>
                    </div>
                }
                .into_any(),
                _ => flow_view(stage, Some(id.get_value()), false, "sto.replace_go", busy, error, Callback::new(exec), on_close),
            }}
        </Modal>
    }
}

// ------------------------------------------------------------------ subvolumes

#[derive(Clone, PartialEq)]
enum SubModal {
    None,
    Create,
    Edit(SubvolumeInfo),
    Delete(SubvolumeInfo),
}

#[component]
fn Subvolumes(id: String) -> impl IntoView {
    let url = StoredValue::new(vol_url(&id, "subvolumes"));
    let list = RwSignal::new(None::<Result<Vec<SubvolumeInfo>, String>>);
    let modal = RwSignal::new(SubModal::None);
    let load = move || {
        spawn_local(async move {
            let r = api::get::<Vec<SubvolumeInfo>>(&url.get_value()).await;
            let _ = list.try_set(Some(r.map_err(|e| e.code().to_string())));
        });
    };
    load();
    let close = Callback::new(move |_| {
        modal.set(SubModal::None);
        load();
    });
    view! {
        <section class="card widget w-12">
            <WidgetHead icon=icons::FOLDER title="sto.subvolumes">
                <button class="btn ghost sm" type="button" on:click=move |_| modal.set(SubModal::Create)><Icon icon=icons::PLUS size=14 />{tr("sto.sub_create")}</button>
            </WidgetHead>
            {move || match list.get() {
                None => view! { <Skeleton lines=3 /> }.into_any(),
                Some(Err(code)) => view! { <ErrorBox code=code on_retry=Callback::new(move |_| load()) /> }.into_any(),
                Some(Ok(v)) if v.is_empty() => view! { <p class="muted">{tr("sto.sub_none")}</p> }.into_any(),
                Some(Ok(v)) => view! {
                    {ui::table("", &["sto.name", "sto.path", "sto.compression", "sto.readonly", ""], v.into_iter().map(|sv| {
                                    let comp = match sv.compression.as_deref() { None => t("sto.inherit").to_string(), Some("none") => t("sto.off").to_string(), Some(c) => c.to_string() };
                                    let e = sv.clone();
                                    let d = sv.clone();
                                    ui::row(vec![
                                        ui::cell("sto.name", "strong", view! { {sv.name.clone()}{sv.system.then(|| view! { <span class="chip">{tr("sto.system")}</span> })} }.into_any()),
                                        ui::cell("sto.path", "mono muted", view! { {sv.path.clone()} }.into_any()),
                                        ui::cell("sto.compression", "", view! { {comp} }.into_any()),
                                        ui::cell("sto.readonly", "", view! { {if sv.readonly { t("sto.yes") } else { t("sto.no") }} }.into_any()),
                                        ui::cell("", "num row-actions", view! {
                                            {(!sv.system).then(|| view! {
                                                <button class="btn ghost sm" type="button" on:click=move |_| modal.set(SubModal::Edit(e.clone()))>{tr("edit")}</button>
                                                <button class="btn ghost sm danger-text" type="button" on:click=move |_| modal.set(SubModal::Delete(d.clone()))>{tr("delete")}</button>
                                            })}
                                        }
                                        .into_any()),
                                    ])
                                }).collect::<Vec<_>>())}
                }
                .into_any(),
            }}
            {move || match modal.get() {
                SubModal::None => ().into_any(),
                SubModal::Create => view! { <SubvolDialog base=url.get_value() edit=None on_close=close /> }.into_any(),
                SubModal::Edit(sv) => view! { <SubvolDialog base=url.get_value() edit=Some(sv) on_close=close /> }.into_any(),
                SubModal::Delete(sv) => view! { <SubvolDelete base=url.get_value() sub=sv on_close=close /> }.into_any(),
            }}
        </section>
    }
}

fn valid_sub(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

#[component]
fn SubvolDialog(base: String, edit: Option<SubvolumeInfo>, on_close: Callback<()>) -> impl IntoView {
    let stage = RwSignal::new(Stage::Form);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let creating = edit.is_none();
    let name = RwSignal::new(edit.as_ref().map(|e| e.name.clone()).unwrap_or_default());
    let comp = RwSignal::new(edit.as_ref().is_none_or(|e| e.compression.as_deref() != Some("none")));
    let ro = RwSignal::new(edit.as_ref().is_some_and(|e| e.readonly));
    let base = StoredValue::new(base);
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let n = name.get_untracked();
        if !valid_sub(&n) {
            error.set(Some(t("sto.sub_invalid").to_string()));
            return;
        }
        let body = json!({"name": n, "compression": comp.get_untracked(), "readonly": ro.get_untracked()});
        if creating {
            start_task(base.get_value(), Some(body), false, stage, busy, error);
        } else {
            // PUT answers 202 like POST: go through the generic path with the PUT method
            busy.set(true);
            error.set(None);
            let url = format!("{}/{}", base.get_value(), api::encode(&n));
            spawn_local(async move {
                let r = api::put::<_, TaskStarted>(&url, &body).await;
                let _ = busy.try_set(false);
                match r {
                    Ok(t) => {
                        let _ = stage.try_set(Stage::Running(t.task_id));
                    }
                    Err(e) => {
                        let _ = error.try_set(Some(e.text()));
                    }
                }
            });
        }
    };
    let title = if creating { t("sto.sub_create").to_string() } else { tf("edit_named", &[("name", &name.get_untracked())]) };
    view! {
        <Modal title=title on_close=on_close>
            {move || match stage.get() {
                Stage::Form => view! {
                    <form class="form" on:submit=submit>
                        <label class="field">
                            <span class="field-label">{tr("sto.name")}</span>
                            <input class="input" disabled=!creating maxlength="64" autocomplete="off" spellcheck="false"
                                prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) />
                            <span class="muted small">{tr("sto.sub_name_hint")}</span>
                        </label>
                        <label class="check"><input type="checkbox" prop:checked=move || comp.get() on:change=move |ev| comp.set(event_target_checked(&ev)) />{tr("sto.sub_compress")}</label>
                        <p class="muted small">{tr("sto.sub_compress_hint")}</p>
                        <label class="check"><input type="checkbox" prop:checked=move || ro.get() on:change=move |ev| ro.set(event_target_checked(&ev)) />{tr("sto.readonly")}</label>
                        {form_error(error)}
                        <div class="dialog-foot">
                            <button class="btn ghost" type="button" on:click=move |_| on_close.run(())>{tr("cancel")}</button>
                            <button class="btn primary" type="submit" disabled=move || busy.get()>{spinner(busy)}{if creating { tr("create")() } else { tr("save")() }}</button>
                        </div>
                    </form>
                }
                .into_any(),
                _ => flow_view(stage, None, false, "save", busy, error, Callback::new(|_| {}), on_close),
            }}
        </Modal>
    }
}

#[component]
fn SubvolDelete(base: String, sub: SubvolumeInfo, on_close: Callback<()>) -> impl IntoView {
    let stage = RwSignal::new(Stage::Form);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let typed = RwSignal::new(String::new());
    let name = StoredValue::new(sub.name.clone());
    let url = StoredValue::new(format!("{base}/{0}?confirm={0}", api::encode(&sub.name)));
    view! {
        <Modal title=tf("sto.sub_delete", &[("name", &sub.name)]) on_close=on_close>
            {move || match stage.get() {
                Stage::Form => view! {
                    {ui::note(Tone::Err, icons::TRIANGLE_ALERT, view! { {tr("sto.sub_delete_text")} }.into_any())}
                    {typed_confirm(name.get_value(), typed)}
                    {form_error(error)}
                    <div class="dialog-foot">
                        <button class="btn ghost" type="button" on:click=move |_| on_close.run(())>{tr("cancel")}</button>
                        <button class="btn danger" type="button" disabled=move || busy.get() || name.with_value(|n| typed.get().trim() != n)
                            on:click=move |_| start_task(url.get_value(), None, true, stage, busy, error)>
                            {spinner(busy)}{tr("delete")}
                        </button>
                    </div>
                }
                .into_any(),
                _ => flow_view(stage, None, false, "delete", busy, error, Callback::new(|_| {}), on_close),
            }}
        </Modal>
    }
}

// ------------------------------------------------------------------ md tuning

#[component]
fn StripeCache(id: String) -> impl IntoView {
    let url = StoredValue::new(vol_url(&id, "settings"));
    let state = RwSignal::new(None::<Result<VolumeSettingsState, String>>);
    let choice = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let fill = move |st: &VolumeSettingsState| {
        let v = st.pending.clone().unwrap_or_else(|| st.applied.clone());
        choice.set(v.stripe_cache_size.map(|n| n.to_string()).unwrap_or_default());
    };
    let load = move || {
        spawn_local(async move {
            let r = api::get::<VolumeSettingsState>(&url.get_value()).await;
            if let Ok(st) = &r {
                fill(st);
            }
            let _ = state.try_set(Some(r.map_err(|e| e.code().to_string())));
        });
    };
    load();
    let save = move |_| {
        busy.set(true);
        error.set(None);
        let body = VolumeSettings { stripe_cache_size: choice.get_untracked().parse().ok() };
        spawn_local(async move {
            let r = api::put::<_, VolumeSettingsState>(&url.get_value(), &body).await;
            match r {
                Ok(st) => {
                    fill(&st);
                    let _ = state.try_set(Some(Ok(st)));
                    // Save applies at once (guidelines A19b); read the applied state back
                    if crate::config::apply_now(&[crate::config::keys::DISKS], error).await {
                        crate::toast::saved_applied();
                        load();
                    }
                }
                Err(e) => error.set(Some(e.text())),
            }
            let _ = busy.try_set(false);
        });
    };
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::SETTINGS_2 title="sto.tuning" />
            {move || match state.get() {
                None => view! { <Skeleton lines=3 /> }.into_any(),
                Some(Err(code)) => view! { <ErrorBox code=code on_retry=Callback::new(move |_| load()) /> }.into_any(),
                Some(Ok(st)) => {
                    let prop = st.proposed_stripe_cache_size;
                    let mem = st.proposed_memory_bytes;
                    view! {
                        <p class="muted small">{tr("sto.stripe_text")}</p>
                        <dl class="kvs">
                            {kv_row("sto.current", sig(st.current_stripe_cache_size.map(|n| n.to_string()).unwrap_or_else(|| "—".into())), "tnum")}
                            {prop.map(|p| kv_row("sto.proposal", sig(format!("{p} · {}", mem.map(i18n::bytes).unwrap_or_default())), "tnum"))}
                        </dl>
                        {st.pending.is_some().then(|| view! { {ui::note(Tone::Info, icons::HISTORY, view! { {tr("sto.pending_note")} }.into_any())} })}
                        <label class="field">
                            <span class="field-label">{tr("sto.stripe")}</span>
                            <select class="input" prop:value=move || choice.get() on:change=move |ev| choice.set(event_target_value(&ev))>
                                <option value="">{tr("sto.kernel_default")}</option>
                                {prop.map(|p| view! { <option value=p.to_string()>{tf("sto.use_proposal", &[("n", &p.to_string())])}</option> })}
                                {[512u32, 1024, 2048, 4096, 8192].into_iter().filter(|n| Some(*n) != prop).map(|n| view! { <option value=n.to_string()>{n.to_string()}</option> }).collect_view()}
                            </select>
                        </label>
                        {form_error(error)}
                        <div class="card-actions"><button class="btn primary sm" type="button" disabled=move || busy.get() on:click=save>{spinner(busy)}{tr("save")}</button></div>
                    }
                    .into_any()
                }
            }}
        </section>
    }
}
