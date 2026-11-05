//! Drives: the list (health, power state, temperature, what uses each disk) and a detail drawer
//! with SMART, the recommended settings and the per-disk overrides (standby, APM, scheduler,
//! read-ahead), staged like every configuration change.
//!
//! A disk in standby is never read automatically: SMART (even with `-n standby`) resets the idle
//! timer of some drives, so the drawer shows the summary and waits for an explicit request.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::storage::{
    CheckStatus, DiskDetail, DiskKind, DiskSettings, DiskSettingsState, DiskSummary, IoScheduler, Severity, SmartInfo,
};

use super::{SubNav, asleep, health, kind_key, loading, power, stale_notice, use_refresh};
use crate::api;
use crate::form::{form_error, spinner};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::ui::{self, Modal, ErrorBox, PageHeader, Skeleton, Status, Tone, kv_row, temp_tone};

#[component]
pub fn Disks() -> impl IntoView {
    let s = use_refresh();
    let selected = RwSignal::new(None::<String>);
    let rows = Memo::new(move |_| {
        let mut d = s.disks.get().unwrap_or_default();
        // bays, then M.2 slots, then the rest
        let key = |d: &DiskSummary| (d.bay.is_none(), d.bay, d.m2.is_none(), d.m2.map(|m| (m.number.is_none(), m.number)), d.name.clone());
        d.sort_by_key(key);
        d
    });
    view! {
        <div class="page">
            <PageHeader icon=icons::HARD_DRIVE title="sto.title" subtitle="sto.subtitle" />
            <SubNav />
            <Show when=move || s.disks.with(Option::is_some) fallback=move || loading(s, 6)>
                {stale_notice(s)}
                <section class="card widget w-12">
                    <h2 class="section-title"><Icon icon=icons::HARD_DRIVE size=16 />{tr("sto.disks")}</h2>
                    {move || {
                        let d = rows.get();
                        if d.is_empty() {
                            view! { <p class="muted">{tr("sto.no_disks")}</p> }.into_any()
                        } else {
                            disk_table(d, selected)
                        }
                    }}
                </section>
            </Show>
            {move || selected.get().map(|name| view! { <DiskDrawer name=name on_close=Callback::new(move |_| selected.set(None)) /> })}
        </div>
    }
}

fn disk_table(disks: Vec<DiskSummary>, selected: RwSignal<Option<String>>) -> AnyView {
    view! {
        {ui::table("", &["sto.drive", "#sto.bay", "#sto.size", "sto.type", "#sto.temp_short", "col.status", "sto.health", "sto.used_by"], disks.into_iter().map(|d| {
                        let (ptone, pkey) = power(d.power_state);
                        let (htone, hkey) = health(d.health);
                        let name = d.name.clone();
                        let used = if d.used_by.is_empty() { t("sto.unused").to_string() } else { d.used_by.join(", ") };
                        // the server's own descriptions ("mounted /home", "member of md0") are English
                        let used_lang = (!d.used_by.is_empty()).then_some("en");
                        ui::row(vec![
                            ui::cell("sto.drive", "drive-cell", view! {
                                <button class="cell-btn" type="button" on:click=move |_| selected.set(Some(name.clone()))>
                                    <span class="mono strong">{d.name.clone()}</span>
                                    <span class="muted small">{d.model.clone()}</span>
                                </button>
                            }
                            .into_any()),
                            ui::cell("sto.bay", "num", view! { {super::slot_text(&d).unwrap_or_else(|| "—".into())} }.into_any()),
                            ui::cell("sto.size", "num", view! { {i18n::bytes_si(d.size)} }.into_any()),
                            ui::cell("sto.type", "", view! {
                                {t(kind_key(d.kind))}
                                {(!d.transport.is_empty() && !d.transport.eq_ignore_ascii_case(t(kind_key(d.kind)))).then(|| format!(" · {}", d.transport.to_uppercase()))}
                                {d.smr.then(|| view! { <span class="chip warn-chip" title=t("sto.smr_hint")>"SMR"</span> })}
                            }
                            .into_any()),
                            ui::cell("sto.temp_short", "num", match d.temperature_c {
                                Some(c) => view! { <span class=format!("tone-text {}", temp_tone(&d.name, c).class())>{i18n::celsius(c)}</span> }.into_any(),
                                // no reading yet: the dash of every other empty cell, not a blank
                                None => view! { "—" }.into_any(),
                            }),
                            ui::cell("col.status", "", view! { <Status tone=ptone text=t(pkey).to_string() /> }.into_any()),
                            ui::cell("sto.health", "", view! { <Status tone=htone text=t(hkey).to_string() /> }.into_any()),
                            ui::cell("sto.used_by", "muted small used-cell", view! { <span title=used.clone() lang=used_lang>{used.clone()}</span> }.into_any()),
                        ])
                    }).collect::<Vec<_>>())}
    }
    .into_any()
}

// ------------------------------------------------------------------ drawer

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Overview,
    Smart,
    Settings,
}

#[component]
pub fn DiskDrawer(name: String, on_close: Callback<()>) -> impl IntoView {
    let s = super::sto();
    let n = StoredValue::new(name.clone());
    let summary = Memo::new(move |_| n.with_value(|n| s.disk(n)));
    let tab = RwSignal::new(Tab::Overview);
    let detail = RwSignal::new(None::<Result<DiskDetail, String>>);
    let reading = RwSignal::new(false);
    let read = move || {
        if reading.get_untracked() {
            return;
        }
        reading.set(true);
        spawn_local(async move {
            let r = api::get::<DiskDetail>(&format!("/api/v1/storage/disks/{}", api::encode(&n.get_value()))).await;
            let _ = reading.try_set(false);
            let _ = detail.try_set(Some(r.map_err(|e| e.code().to_string())));
        });
    };
    // never wake a sleeping disk: read SMART on open only when it is spinning
    if summary.get_untracked().is_some_and(|d| !asleep(d.power_state)) {
        read();
    }
    let title = Signal::derive(move || {
        summary.with(|d| d.as_ref().map(|d| format!("{} · {}", d.name, d.model))).unwrap_or_else(|| n.get_value())
    });
    let tab_btn = move |which: Tab, key: &'static str| {
        view! {
            <button
                class="tab"
                role="tab"
                type="button"
                aria-selected=move || (tab.get() == which).to_string()
                on:click=move |_| tab.set(which)
            >
                {tr(key)}
            </button>
        }
    };
    view! {
        <Modal title=title on_close=on_close drawer=true>
            <div class="tabs" role="tablist">
                {tab_btn(Tab::Overview, "sto.tab.overview")}
                {tab_btn(Tab::Smart, "sto.smart")}
                {tab_btn(Tab::Settings, "sto.settings")}
            </div>
            {move || match summary.get() {
                None => view! { <p class="muted">{tr("err.not_found")}</p> }.into_any(),
                Some(d) => match tab.get() {
                    Tab::Overview => overview_tab(d, detail, reading, read).into_any(),
                    Tab::Smart => smart_tab(d, detail, reading, read).into_any(),
                    Tab::Settings => view! { <SettingsTab name=n.get_value() kind=d.kind detail=detail /> }.into_any(),
                },
            }}
        </Modal>
    }
}

/// Banner of a disk in standby with the explicit "read now".
fn standby_note(reading: RwSignal<bool>, read: impl Fn() + Copy + Send + Sync + 'static) -> AnyView {
    view! {
        {ui::note_class(Tone::Info, icons::MOON_STAR, "standby-note", view! { <span class="grow">{tr("sto.standby_note")}</span>
            <button class="btn ghost sm" type="button" disabled=move || reading.get() on:click=move |_| read()>
                {spinner(reading)}{tr("sto.read_now")}
            </button> }.into_any())}
    }
    .into_any()
}

fn reason_text(code: &str, message: &str) -> String {
    i18n::try_t(&format!("sto.reason.{code}")).map(str::to_string).unwrap_or_else(|| message.to_string())
}

fn overview_tab(
    d: DiskSummary,
    detail: RwSignal<Option<Result<DiskDetail, String>>>,
    reading: RwSignal<bool>,
    read: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let (htone, hkey) = health(d.health);
    let (ptone, pkey) = power(d.power_state);
    let sleeping = asleep(d.power_state);
    let opt = |v: Option<String>| Signal::derive(move || v.clone().unwrap_or_else(|| "—".into()));
    let used = if d.used_by.is_empty() { t("sto.unused").to_string() } else { d.used_by.join(", ") };
    view! {
        <div class="drawer-sec">
            {(sleeping && detail.with(Option::is_none)).then(|| standby_note(reading, read))}
            <div class="health-line">
                <Status tone=htone text=t(hkey).to_string() />
                <Status tone=ptone text=t(pkey).to_string() />
                {d.temperature_c.map(|c| view! {
                    <span class=format!("chip tone-text {}", temp_tone(&d.name, c).class())><Icon icon=icons::THERMOMETER size=12 />{i18n::celsius(c)}</span>
                })}
                {d.smr.then(|| view! { <span class="chip warn-chip" title=t("sto.smr_hint")>"SMR"</span> })}
            </div>
            {(!d.health_reasons.is_empty()).then(|| view! {
                <ul class="reasons">
                    {d.health_reasons.iter().map(|r| {
                        let tone = match r.severity { Severity::Critical => Tone::Err, Severity::Warning => Tone::Warn, Severity::Info => Tone::Info };
                        view! { <li class=tone.class()><Icon icon=tone.icon() size=14 /><span>{reason_text(&r.code, &r.message)}</span></li> }
                    }).collect_view()}
                </ul>
            })}
            {d.smr.then(|| view! { <p class="muted small">{tr("sto.smr_hint")}</p> })}
            <dl class="kvs">
                {kv_row("sto.model", Signal::derive({ let v = d.model.clone(); move || v.clone() }), "")}
                {kv_row("sto.serial", Signal::derive({ let v = d.serial.clone(); move || v.clone() }), "mono")}
                {kv_row("sto.wwn", opt(d.wwn.clone()), "mono")}
                {kv_row("sto.firmware", opt(d.firmware.clone()), "mono")}
                {kv_row("sto.size", Signal::derive({ let v = i18n::bytes_si(d.size); move || v.clone() }), "tnum")}
                {kv_row("sto.type", Signal::derive({ let v = format!("{} · {}", t(kind_key(d.kind)), d.transport.to_uppercase()); move || v.clone() }), "")}
                {kv_row("sto.rpm", opt(d.rotation_rpm.map(|r| format!("{r} rpm"))), "tnum")}
                {kv_row("sto.bay", opt(super::slot_text(&d)), "tnum")}
                {kv_row("sto.poh", opt(d.power_on_hours.map(|h| i18n::duration(h * 3600))), "tnum")}
                {kv_row("sto.smart_age", opt(d.smart_age_s.map(|a| tf("sto.ago", &[("t", &i18n::duration(a))]))), "")}
                {kv_row("sto.used_by", Signal::derive(move || used.clone()), "")}
            </dl>
            {move || detail.get().and_then(Result::ok).filter(|d| !d.partitions.is_empty()).map(|dd| view! {
                <h3 class="drawer-h">{tr("sto.partitions")}</h3>
                {ui::table("compact", &["sto.name", "#sto.size", "sto.fs", "sto.used_by"], dd.partitions.into_iter().map(|p| ui::row(vec![
                    ui::cell("sto.name", "mono", view! { {p.name} }.into_any()),
                    ui::cell("sto.size", "num", view! { {i18n::bytes_si(p.size)} }.into_any()),
                    ui::cell("sto.fs", "", view! { {p.fstype.unwrap_or_default()} }.into_any()),
                    ui::cell("sto.used_by", "mono small", view! { {p.holders.into_iter().chain(p.mountpoints).collect::<Vec<_>>().join(", ")} }.into_any()),
                ])).collect::<Vec<_>>())}
            })}
        </div>
    }
}

fn smart_tab(
    d: DiskSummary,
    detail: RwSignal<Option<Result<DiskDetail, String>>>,
    reading: RwSignal<bool>,
    read: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let sleeping = asleep(d.power_state);
    move || match detail.get() {
        None if reading.get() => view! { <Skeleton lines=8 /> }.into_any(),
        None if sleeping => standby_note(reading, read),
        None => view! { <Skeleton lines=8 /> }.into_any(),
        Some(Err(code)) => view! { <ErrorBox code=code on_retry=Callback::new(move |_| read()) /> }.into_any(),
        Some(Ok(dd)) => match dd.smart {
            None => view! { <p class="muted">{tr("sto.no_smart")}</p> }.into_any(),
            Some(sm) => smart_view(sm),
        },
    }
}

fn smart_view(sm: SmartInfo) -> AnyView {
    let opt = |v: Option<String>| Signal::derive(move || v.clone().unwrap_or_else(|| "—".into()));
    let verdict = match sm.passed {
        Some(true) => t("sto.smart_passed").to_string(),
        Some(false) => t("sto.smart_failed").to_string(),
        None => "—".into(),
    };
    let attrs = sm.attributes.clone();
    let nvme = sm.nvme.clone();
    let tests = sm.self_tests.clone();
    view! {
        <div class="drawer-sec">
            <dl class="kvs">
                {kv_row("sto.smart_verdict", Signal::derive(move || verdict.clone()), "")}
                {kv_row("sto.temp", opt(sm.temperature_c.map(i18n::celsius)), "tnum")}
                {kv_row("sto.poh", opt(sm.power_on_hours.map(|h| i18n::duration(h * 3600))), "tnum")}
                {kv_row("sto.power_cycles", opt(sm.power_cycles.map(|c| c.to_string())), "tnum")}
                {kv_row("sto.life", opt(sm.life_remaining_pct.map(|p| i18n::pct(p as f64))), "tnum")}
                {kv_row("sto.written", opt(sm.bytes_written.map(i18n::bytes_si)), "tnum")}
                {kv_row("sto.error_log", opt(sm.error_log_count.map(|c| c.to_string())), "tnum")}
                {kv_row("sto.selftest_running", opt(sm.self_test_running.map(|p| tf("sto.pct_left", &[("p", &p.to_string())]))), "")}
                {kv_row("sto.read_at", Signal::derive(move || i18n::datetime(sm.read_ms)), "")}
            </dl>
            {nvme.map(|nv| view! {
                <h3 class="drawer-h">{tr("sto.nvme_health")}</h3>
                <dl class="kvs">
                    {kv_row("sto.nv_spare", Signal::derive(move || format!("{} (≥ {})", i18n::pct(nv.available_spare_pct as f64), i18n::pct(nv.available_spare_threshold_pct as f64))), "tnum")}
                    {kv_row("sto.nv_used", Signal::derive(move || i18n::pct(nv.percentage_used as f64)), "tnum")}
                    {kv_row("sto.written", Signal::derive(move || i18n::bytes_si((nv.tb_written * 1e12) as u64)), "tnum")}
                    {kv_row("sto.nv_media", Signal::derive(move || nv.media_errors.to_string()), "tnum")}
                    {kv_row("sto.nv_unsafe", Signal::derive(move || nv.unsafe_shutdowns.to_string()), "tnum")}
                    {kv_row("sto.nv_warn", Signal::derive(move || format!("{} / {}", nv.warning_temp_minutes, nv.critical_temp_minutes)), "tnum")}
                    {kv_row("sto.nv_crit", Signal::derive(move || format!("0x{:02x}", nv.critical_warning)), "mono")}
                </dl>
            })}
            {(!attrs.is_empty()).then(|| view! {
                <h3 class="drawer-h">{tr("sto.attributes")}</h3>
                {ui::table("compact", &["#smart.col_id", "sto.name", "#sto.a_value", "#sto.a_worst", "#sto.a_thresh", "#sto.a_raw", "col.status"], attrs.into_iter().map(|a| {
                                let (tone, key) = if a.when_failed == "now" {
                                    (Tone::Err, "sto.a_failing")
                                } else if a.when_failed == "past" {
                                    (Tone::Warn, "sto.a_failed_past")
                                } else if a.watched && a.raw > 0 {
                                    (Tone::Warn, "sto.a_watch")
                                } else {
                                    (Tone::Ok, "sto.a_ok")
                                };
                                ui::row_class(
                                    vec![
                                        ui::cell("smart.col_id", "num", view! { {a.id} }.into_any()),
                                        ui::cell("sto.name", "", view! { {a.name} }.into_any()),
                                        ui::cell("sto.a_value", "num", view! { {a.value} }.into_any()),
                                        ui::cell("sto.a_worst", "num", view! { {a.worst} }.into_any()),
                                        ui::cell("sto.a_thresh", "num", view! { {a.thresh} }.into_any()),
                                        ui::cell("sto.a_raw", "num mono", view! { {a.raw_string} }.into_any()),
                                        ui::cell("col.status", "", view! { <Status tone=tone text=t(key).to_string() /> }.into_any()),
                                    ],
                                    if a.watched { "watched" } else { "" },
                                )
                            }).collect::<Vec<_>>())}
            })}
            {(!tests.is_empty()).then(|| view! {
                <h3 class="drawer-h">{tr("sto.selftests")}</h3>
                {ui::table("compact", &["sto.type", "col.status", "#sto.at_hours"], tests.into_iter().map(|s| {
                                let tone = match s.passed { Some(true) => Tone::Ok, Some(false) => Tone::Err, None => Tone::Unknown };
                                ui::row(vec![
                                    ui::cell("sto.type", "", view! { {s.kind} }.into_any()),
                                    ui::cell("col.status", "", view! { <Status tone=tone text=s.status /> }.into_any()),
                                    ui::cell("sto.at_hours", "num", view! { {s.lifetime_hours.map(|h| h.to_string()).unwrap_or_default()} }.into_any()),
                                ])
                            }).collect::<Vec<_>>())}
            })}
        </div>
    }
    .into_any()
}

// ------------------------------------------------------------------ settings

const SCHEDULERS: [IoScheduler; 4] = [IoScheduler::MqDeadline, IoScheduler::Bfq, IoScheduler::Kyber, IoScheduler::None];
const APM: [(u8, &str); 5] =
    [(1, "sto.apm_1"), (127, "sto.apm_127"), (128, "sto.apm_128"), (254, "sto.apm_254"), (255, "sto.apm_255")];
const STANDBY: [u32; 8] = [0, 600, 1200, 1800, 3600, 7200, 10800, 18000];

/// Translated label of a recommended-settings check (`SettingCheck.key`); `None` for a key this
/// UI does not know yet (the server's English label is shown then).
fn check_label(key: &str) -> Option<&'static str> {
    Some(t(match key {
        "scheduler" => "sto.scheduler",
        "read_ahead_kb" => "sto.read_ahead",
        "apm" => "sto.apm",
        "temperature" => "sto.temp",
        "write_cache" => "sto.chk.write_cache",
        "trim" => "sto.chk.trim",
        "alpm" => "sto.chk.alpm",
        "apst" => "sto.chk.apst",
        _ => return None,
    }))
}

fn sched_of(s: &str) -> Option<IoScheduler> {
    SCHEDULERS.into_iter().find(|x| x.as_str() == s)
}

fn opt_str<T: ToString>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_default()
}

#[component]
fn SettingsTab(name: String, kind: DiskKind, detail: RwSignal<Option<Result<DiskDetail, String>>>) -> impl IntoView {
    let url = StoredValue::new(format!("/api/v1/storage/disks/{}/settings", api::encode(&name)));
    let state = RwSignal::new(None::<Result<DiskSettingsState, String>>);
    let sched = RwSignal::new(String::new());
    let ra = RwSignal::new(String::new());
    let apm = RwSignal::new(String::new());
    let standby = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let saved = RwSignal::new(false);
    let fill = move |st: &DiskSettingsState| {
        let o = st.pending.clone().or_else(|| st.applied.clone()).unwrap_or_default();
        sched.set(o.scheduler.map(|s| s.as_str().to_string()).unwrap_or_default());
        ra.set(opt_str(o.read_ahead_kb));
        apm.set(opt_str(o.apm));
        standby.set(opt_str(o.standby_s));
    };
    let load = move || {
        spawn_local(async move {
            let r = api::get::<DiskSettingsState>(&url.get_value()).await;
            if let Ok(st) = &r {
                fill(st);
            }
            let _ = state.try_set(Some(r.map_err(|e| e.code().to_string())));
        });
    };
    load();
    let hdd = kind == DiskKind::Hdd;
    let submit = move |body: DiskSettings| {
        busy.set(true);
        error.set(None);
        saved.set(false);
        spawn_local(async move {
            let r = api::put::<_, DiskSettingsState>(&url.get_value(), &body).await;
            let _ = busy.try_set(false);
            match r {
                Ok(st) => {
                    fill(&st);
                    state.set(Some(Ok(st)));
                    saved.set(true);
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    };
    let save = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let ra_v = ra.get_untracked();
        let read_ahead_kb = if ra_v.trim().is_empty() {
            None
        } else {
            match ra_v.trim().parse::<u32>() {
                Ok(v) if v <= 65536 => Some(v),
                _ => {
                    error.set(Some(t("sto.ra_invalid").to_string()));
                    return;
                }
            }
        };
        submit(DiskSettings {
            scheduler: sched_of(&sched.get_untracked()),
            read_ahead_kb,
            apm: if hdd { apm.get_untracked().parse().ok() } else { None },
            standby_s: if hdd { standby.get_untracked().parse().ok() } else { None },
        });
    };
    view! {
        <div class="drawer-sec">
            {move || detail.get().and_then(Result::ok).filter(|d| !d.settings.is_empty()).map(|dd| view! {
                <h3 class="drawer-h">{tr("sto.recommended")}</h3>
                <ul class="checks">
                    {dd.settings.into_iter().map(|c| {
                        let (tone, key) = match c.status {
                            CheckStatus::Ok => (Tone::Ok, "sto.chk_ok"),
                            CheckStatus::Differs => (Tone::Warn, "sto.chk_differs"),
                            CheckStatus::NotApplicable => (Tone::Unknown, "sto.chk_na"),
                            CheckStatus::Unknown => (Tone::Unknown, "health.unknown"),
                        };
                        view! {
                            <li>
                                <div class="check-top">
                                    <strong>{check_label(&c.key).map(str::to_string).unwrap_or(c.label)}</strong>
                                    <span class="row-end"><Status tone=tone text=t(key).to_string() /></span>
                                </div>
                                <div class="muted small">
                                    {tr("sto.current")}": "<span class="mono">{c.current.unwrap_or_else(|| "—".into())}</span>
                                    " · "{tr("sto.recommended_v")}": "<span class="mono">{c.recommended}</span>
                                </div>
                                {(!c.note.is_empty()).then(|| view! { <div class="muted small" lang="en">{c.note}</div> })}
                            </li>
                        }
                    }).collect_view()}
                </ul>
            })}
            <h3 class="drawer-h">{tr("sto.overrides")}</h3>
            {move || match state.get() {
                None => view! { <Skeleton lines=4 /> }.into_any(),
                Some(Err(code)) => view! { <ErrorBox code=code on_retry=Callback::new(move |_| load()) /> }.into_any(),
                Some(Ok(st)) => {
                    let def = st.defaults.clone();
                    let def_sched = def.scheduler.map(|s| s.as_str().to_string()).unwrap_or_else(|| "—".into());
                    let def_ra = opt_str(def.read_ahead_kb);
                    view! {
                        <form class="form" on:submit=save>
                            {st.pending.is_some().then(|| view! {
                                {ui::note(Tone::Info, icons::HISTORY, view! { {tr("sto.pending_note")} }.into_any())}
                            })}
                            <p class="muted small">
                                {tf("sto.kernel_now", &[
                                    ("s", &st.current_scheduler.clone().unwrap_or_else(|| "—".into())),
                                    ("r", &opt_str(st.current_read_ahead_kb)),
                                ])}
                            </p>
                            <label class="field">
                                <span class="field-label">{tr("sto.scheduler")}</span>
                                <select class="input" prop:value=move || sched.get() on:change=move |ev| sched.set(event_target_value(&ev))>
                                    <option value="">{tf("sto.default_v", &[("v", &def_sched)])}</option>
                                    {SCHEDULERS.iter().map(|s| view! { <option value=s.as_str()>{s.as_str()}</option> }).collect_view()}
                                </select>
                            </label>
                            <label class="field">
                                <span class="field-label">{tr("sto.read_ahead")}</span>
                                <input class="input" type="number" min="0" max="65536" step="128" inputmode="numeric"
                                    placeholder=tf("sto.default_v", &[("v", &def_ra)])
                                    prop:value=move || ra.get() on:input=move |ev| ra.set(event_target_value(&ev)) />
                            </label>
                            <label class="field">
                                <span class="field-label">{tr("sto.standby")}</span>
                                <select class="input" disabled=!hdd prop:value=move || standby.get() on:change=move |ev| standby.set(event_target_value(&ev))>
                                    <option value="">{tr("sto.not_set")}</option>
                                    {STANDBY.iter().map(|s| view! {
                                        <option value=s.to_string()>{if *s == 0 { t("sto.never").to_string() } else { i18n::duration(*s as u64) }}</option>
                                    }).collect_view()}
                                </select>
                                <span class="muted small">{if hdd { t("sto.standby_hint") } else { t("sto.hdd_only") }}</span>
                            </label>
                            <label class="field">
                                <span class="field-label">{tr("sto.apm")}</span>
                                <select class="input" disabled=!hdd prop:value=move || apm.get() on:change=move |ev| apm.set(event_target_value(&ev))>
                                    <option value="">{tr("sto.not_set")}</option>
                                    {APM.iter().map(|(v, k)| view! { <option value=v.to_string()>{tr(k)}</option> }).collect_view()}
                                </select>
                            </label>
                            {form_error(error)}
                            {move || saved.get().then(|| view! {
                                {ui::note(Tone::Ok, icons::CIRCLE_CHECK, view! { {tr("sto.staged")} }.into_any())}
                            })}
                            <div class="dialog-foot">
                                <button class="btn ghost" type="button" disabled=move || busy.get() on:click=move |_| submit(DiskSettings::default())>
                                    <Icon icon=icons::ROTATE_CCW size=14 />{tr("sto.reset_defaults")}
                                </button>
                                <button class="btn primary" type="submit" disabled=move || busy.get()>{spinner(busy)}{tr("sto.stage")}</button>
                            </div>
                        </form>
                    }
                    .into_any()
                }
            }}
        </div>
    }
}
