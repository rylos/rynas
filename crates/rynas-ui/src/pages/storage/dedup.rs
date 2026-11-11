//! Deduplication (D-DEDUP, `docs/phase2/dedup.md`): the card of a filesystem's job (volume
//! detail, next to Scrub; jobs of filesystems without a volume page on the Storage page),
//! its dialogs (run or estimate with an end, settings with the 7×24 window, history) and
//! the share editor's switch (`share_fields`).
//!
//! Everything comes from `GET /storage/dedup` (rynasd's own state, never the hashfile):
//! read again every 3 s while a job runs.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::dedup::{
    DedupBlock, DedupJobView, DedupKind, DedupOutcome, DedupOverview, DedupProgress, DedupRecord, DedupResetRequest,
    DedupRunRequest, DedupWarning, ShareDedup, UUID_KEY_PREFIX, VolumeDedup,
};
use rynas_api_types::{Share, TaskStatus};

use crate::api;
use crate::form::{form_error, lens, lens_bool, spinner, switch};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::toast::toast;
use crate::ui::{self, Modal, Remote, Status, Tone, WidgetHead, kv_row, typed_confirm};
use crate::week_grid::{GridLevel, GridTone, summary_text, week_grid};

const URL: &str = "/api/v1/storage/dedup";

/// Window levels: off first (the base level, left out of the summary).
pub const LEVELS: &[GridLevel] = &[
    GridLevel { code: '0', label_key: "dd.grid_off", tone: GridTone::Off },
    GridLevel { code: '1', label_key: "dd.grid_on", tone: GridTone::Full },
];

/// A job's name: the volume's, or the start of the filesystem's UUID.
pub fn job_name(key: &str) -> String {
    match key.strip_prefix(UUID_KEY_PREFIX) {
        Some(u) => tf("dd.fs_name", &[("id", &u.chars().take(8).collect::<String>())]),
        None => crate::places::name(key),
    }
}

/// Where a job's card is: the volume page, or the Storage page.
pub fn job_href(key: &str) -> String {
    if key.starts_with(UUID_KEY_PREFIX) || key == rynas_api_types::storage::SYSTEM_VOLUME {
        "/storage".into()
    } else {
        format!("/storage/volumes/{}", api::encode(key))
    }
}

fn job_url(key: &str, what: &str) -> String {
    format!("{URL}/jobs/{}/{what}", api::encode(key))
}

/// The overview, read again every 3 s while a job runs and after each change.
pub fn overview() -> Remote<DedupOverview> {
    let r = Remote::<DedupOverview>::with_config(URL);
    let polling = StoredValue::new(false);
    Effect::new(move |_| {
        let busy = r.data.with(|d| d.as_ref().is_some_and(|o| o.jobs.iter().any(|j| j.running.is_some())));
        if !busy || polling.get_value() {
            return;
        }
        polling.set_value(true);
        spawn_local(async move {
            loop {
                api::sleep(3000).await;
                let still = r.data.try_with_untracked(|d| d.as_ref().is_some_and(|o| o.jobs.iter().any(|j| j.running.is_some())));
                if still != Some(true) {
                    break;
                }
                r.reload();
            }
            let _ = polling.try_set_value(false);
        });
    });
    r
}

fn outcome_text(o: DedupOutcome) -> (Tone, &'static str) {
    match o {
        DedupOutcome::Done => (Tone::Ok, "dd.out.done"),
        DedupOutcome::WindowEnded => (Tone::Info, "dd.out.window_ended"),
        DedupOutcome::Preempted => (Tone::Info, "dd.out.preempted"),
        DedupOutcome::Stopped => (Tone::Unknown, "dd.out.stopped"),
        DedupOutcome::Partial => (Tone::Warn, "dd.out.partial"),
        DedupOutcome::Failed => (Tone::Err, "dd.out.failed"),
        DedupOutcome::Skipped => (Tone::Unknown, "dd.out.skipped"),
    }
}

fn block_key(b: DedupBlock) -> &'static str {
    match b {
        DedupBlock::Encrypted => "dd.block.encrypted",
        DedupBlock::Nocow => "dd.block.nocow",
        DedupBlock::Filesystem => "dd.block.filesystem",
        DedupBlock::XfsNoReflink => "dd.block.xfs_no_reflink",
        DedupBlock::OtherFilesystem => "dd.block.other_fs",
        DedupBlock::NotMounted => "dd.block.not_mounted",
    }
}

fn warning_key(w: DedupWarning) -> &'static str {
    match w {
        DedupWarning::Qgroups => "dd.warn.qgroups",
        DedupWarning::Replicated => "dd.warn.replicated",
        DedupWarning::NoShares => "dd.warn.no_shares",
        DedupWarning::SharesBlocked => "dd.warn.blocked",
        DedupWarning::Flash => "dd.warn.flash",
    }
}

fn progress_text(p: &DedupProgress) -> String {
    let phase = t(match p.phase.as_str() {
        "scanning" => "dd.ph.scanning",
        "hashing" => "dd.ph.hashing",
        _ => "dd.ph.dedupe",
    });
    let mut s = phase.to_string();
    if p.total > 0 {
        s.push_str(&format!(" · {}", i18n::pct(p.done as f64 * 100.0 / p.total as f64)));
    }
    if let Some(e) = p.eta_s {
        s.push_str(&format!(" · {}", tf("dd.eta", &[("time", &i18n::duration(e))])));
    }
    if p.reclaimed_bytes > 0 {
        s.push_str(&format!(" · {}", tf("dd.freed", &[("size", &i18n::bytes(p.reclaimed_bytes))])));
    }
    s
}

fn record_line(r: &DedupRecord) -> String {
    let mut s = i18n::short_when(r.start_ms);
    if r.kind == DedupKind::Estimate {
        s.push_str(&format!(" · {}", t("dd.estimate")));
    }
    if r.reclaimed_bytes > 0 {
        s.push_str(&format!(" · {}", tf("dd.freed", &[("size", &i18n::bytes(r.reclaimed_bytes))])));
    }
    s
}

#[derive(Clone, Copy, PartialEq)]
enum Open {
    None,
    Run(DedupKind),
    Settings,
    History,
}

/// The card of the job `key`, or nothing when the volume's filesystem cannot be
/// deduplicated.
#[component]
pub fn DedupCard(key: String) -> impl IntoView {
    let ov = overview();
    view! { <JobCard key=key ov=ov /> }
}

/// Cards of the jobs that have no volume page (the system volume, folders on other
/// filesystems), shown once something is set up there.
#[component]
pub fn OtherDedupJobs(#[prop(into)] volumes: Signal<Vec<String>>) -> impl IntoView {
    let ov = overview();
    let keys = Memo::new(move |_| {
        let vols = volumes.get();
        ov.data.with(|d| {
            d.as_ref()
                .map(|o| {
                    o.jobs
                        .iter()
                        .filter(|j| !vols.contains(&j.key))
                        .filter(|j| j.settings.is_some() || j.shares.iter().any(|s| s.enabled))
                        .map(|j| j.key.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    move || {
        let ks = keys.get();
        (!ks.is_empty()).then(|| {
            view! { <div class="grid vol-detail">{ks.into_iter().map(|k| view! { <JobCard key=k ov=ov /> }).collect_view()}</div> }
        })
    }
}

/// One job: the card follows the overview; its dialogs stay open across reloads.
#[component]
fn JobCard(key: String, ov: Remote<DedupOverview>) -> impl IntoView {
    let modal = RwSignal::new(Open::None);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let key = StoredValue::new(key);
    let find = move |d: &Option<DedupOverview>| d.as_ref().and_then(|o| o.jobs.iter().find(|j| j.key == key.get_value()).cloned());
    let body = move || {
        ov.data.with(|d| {
            let o = d.as_ref()?;
            let j = o.jobs.iter().find(|j| j.key == key.get_value())?.clone();
            Some(card(o, j, ov, modal, busy, error))
        })
    };
    let dialogs = move || {
        let m = modal.get();
        let Some(j) = ov.data.with_untracked(find) else { return ().into_any() };
        let excluded = ov.data.with_untracked(|d| d.as_ref().map(|o| o.always_excluded.clone()).unwrap_or_default());
        match m {
            Open::None => ().into_any(),
            Open::Run(kind) => run_dialog(j, kind, modal, ov),
            Open::Settings => settings_dialog(j, excluded, modal, ov),
            Open::History => history_dialog(j.key, modal),
        }
    };
    view! { {body} {dialogs} }
}

fn card(
    o: &DedupOverview,
    j: DedupJobView,
    ov: Remote<DedupOverview>,
    modal: RwSignal<Open>,
    busy: RwSignal<bool>,
    error: RwSignal<Option<String>>,
) -> AnyView {
    let engine = o.engine.clone();
    let key = StoredValue::new(j.key.clone());
    let install = move |_| {
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = crate::tasks::run_task(&format!("{URL}/install"), None).await;
            let _ = busy.try_set(false);
            match r {
                Ok(task) if task.status == TaskStatus::Ok => ov.reload(),
                Ok(task) => {
                    let _ = error.try_set(Some(crate::tasks::task_error(&task)));
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    };
    let stop = move |_| {
        busy.set(true);
        spawn_local(async move {
            let r = api::post_empty::<()>(&job_url(&key.get_value(), "stop"), None).await;
            let _ = busy.try_set(false);
            match r {
                Ok(()) => ov.reload(),
                Err(e) => toast(Tone::Err, e.text()),
            }
        });
    };
    let on = j.settings.as_ref().is_some_and(|s| s.enabled);
    let (tone, state) = match (&j.running, &j.settings) {
        (Some(r), _) if r.waiting_for.is_some() => (Tone::Warn, "dd.st.waiting"),
        (Some(r), _) if r.kind == DedupKind::Estimate => (Tone::Info, "dd.st.estimating"),
        (Some(_), _) => (Tone::Info, "dd.st.running"),
        (None, Some(_)) if on => (Tone::Ok, "dd.st.on"),
        (None, Some(_)) => (Tone::Unknown, "dd.st.paused"),
        (None, None) => (Tone::Unknown, "dd.st.off"),
    };
    let enabled: Vec<String> = j.shares.iter().filter(|s| s.enabled && s.block.is_none()).map(|s| s.name.clone()).collect();
    let usable = j.shares.iter().any(|s| s.block.is_none());
    let running = j.running.clone();
    let can_run = engine.installed && engine.supported && running.is_none();
    let window = j.settings.as_ref().map(|s| summary_text(&s.window, LEVELS));
    let report = j.report.clone();
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::COPY title="dd.title"><Status tone=tone text=t(state).to_string() /></WidgetHead>
            <p class="muted small">{tr("dd.text")}</p>
            {(!engine.installed).then(|| {
                let can = engine.installable;
                ui::note(Tone::Info, icons::INFO, view! {
                    <div>
                        <p>{tr(if can { "dd.engine_missing" } else { "dd.engine_unavailable" })}</p>
                        {can.then(|| view! {
                            <button class="btn ghost sm" type="button" disabled=move || busy.get() on:click=install>{spinner(busy)}{tr("dd.install")}</button>
                        })}
                    </div>
                }.into_any())
            })}
            {(engine.installed && !engine.supported).then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! {
                {tf("dd.engine_unsupported", &[("version", engine.version.as_deref().unwrap_or("?")), ("min", &engine.min_version), ("max", &engine.max_version)])}
            }.into_any()))}
            {running.clone().map(|r| {
                let line = match (&r.waiting_for, &r.progress) {
                    (Some(w), _) => tf("dd.waiting_for", &[("task", &crate::tasks::task_label(w))]),
                    (None, Some(p)) => progress_text(p),
                    (None, None) => t("dd.starting").to_string(),
                };
                let until = tf("dd.until", &[("when", &i18n::short_when(r.until_ms))]);
                view! {
                    <dl class="kvs">
                        {kv_row("sto.progress", Signal::stored(line), "")}
                        {kv_row("dd.ends", Signal::stored(until), "tnum")}
                    </dl>
                    {r.stopping.then(|| view! { <p class="muted small">{tr("dd.stopping")}</p> })}
                }
            })}
            {(running.is_none()).then(|| view! {
                <dl class="kvs">
                    {window.clone().map(|w| kv_row("dd.window", Signal::stored(w), ""))}
                    {j.settings.is_some().then(|| kv_row("apps.d.shares", Signal::stored(if enabled.is_empty() { t("dd.none").to_string() } else { enabled.join(", ") }), ""))}
                    {j.last.as_ref().map(|r| {
                        let (tn, k) = outcome_text(r.outcome);
                        view! { <div class="kv"><dt>{tr("sch.col.last")}</dt><dd><Status tone=tn text=t(k).to_string() />" "<span class="small tnum">{record_line(r)}</span></dd></div> }
                    })}
                    {report.as_ref().filter(|r| r.reclaimed_total_bytes > 0).map(|r| kv_row("dd.total", Signal::stored(i18n::bytes(r.reclaimed_total_bytes)), "tnum"))}
                    {report.as_ref().filter(|r| r.reclaimable_bytes > 0 && r.dedupe_configured == Some(true)).map(|r| kv_row("dd.left", Signal::stored(i18n::bytes(r.reclaimable_bytes)), "tnum"))}
                    {j.next_window_ms.map(|m| kv_row("dd.next", Signal::stored(i18n::short_when(m)), "tnum"))}
                    {j.estimate.as_ref().map(|e| {
                        let pct = if e.logical_bytes > 0 { e.reclaimable_bytes as f64 * 100.0 / e.logical_bytes as f64 } else { 0.0 };
                        kv_row("dd.estimate_result", Signal::stored(tf("dd.estimate_line", &[
                            ("size", &i18n::bytes(e.reclaimable_bytes)), ("files", &e.dup_files.to_string()), ("pct", &i18n::pct(pct)), ("total", &i18n::bytes(e.logical_bytes)),
                        ])), "")
                    })}
                </dl>
            })}
            {j.warnings.iter().map(|w| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tr(warning_key(*w))} }.into_any())).collect_view()}
            {j.estimate.is_some().then(|| view! { <p class="muted small">{tr("dd.logical_note")}</p> })}
            <div class="card-actions">
                {match running.is_some() {
                    true => view! {
                        <button class="btn ghost sm" type="button" disabled=move || busy.get() on:click=stop>{spinner(busy)}{tr("dd.stop")}</button>
                    }.into_any(),
                    false => view! {
                        <button class="btn ghost sm" type="button" disabled=!(can_run && usable) on:click=move |_| modal.set(Open::Run(DedupKind::Estimate))>{tr("dd.estimate")}</button>
                        {j.settings.is_some().then(|| view! {
                            <button class="btn ghost sm" type="button" disabled=!(can_run && !enabled.is_empty()) on:click=move |_| modal.set(Open::Run(DedupKind::Run))>
                                <Icon icon=icons::PLAY size=14 />{tr("run_now")}
                            </button>
                        })}
                    }.into_any(),
                }}
                <button class="btn ghost sm" type="button" on:click=move |_| modal.set(Open::History)><Icon icon=icons::HISTORY size=14 />{tr("dd.history")}</button>
                <button class=if j.settings.is_none() { "btn primary sm" } else { "btn ghost sm" } type="button" disabled=!engine.installed
                    on:click=move |_| modal.set(Open::Settings)>
                    {tr(if j.settings.is_none() { "sec.activate" } else { "dd.settings" })}
                </button>
            </div>
            {form_error(error)}
        </section>
    }
    .into_any()
}

/// Ends offered for a run or an estimate started by hand (hours; 0: the window).
const UNTIL: &[(&str, &str)] = &[("0", "dd.until_window"), ("1", "dd.h1"), ("2", "dd.h2"), ("4", "dd.h4"), ("8", "dd.h8"), ("24", "dd.h24")];

fn run_dialog(j: DedupJobView, kind: DedupKind, modal: RwSignal<Open>, ov: Remote<DedupOverview>) -> AnyView {
    let until = RwSignal::new("0".to_string());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let close = Callback::new(move |_| modal.set(Open::None));
    let key = j.key.clone();
    let on: Vec<String> = j.shares.iter().filter(|s| s.enabled && s.block.is_none()).map(|s| s.name.clone()).collect();
    let which = if kind == DedupKind::Estimate && on.is_empty() {
        j.shares.iter().filter(|s| s.block.is_none()).map(|s| s.name.clone()).collect::<Vec<_>>()
    } else {
        on
    };
    let snapshots = j.shares.iter().any(|s| which.contains(&s.name) && s.snapshots);
    let replicated = kind == DedupKind::Run && j.shares.iter().any(|s| which.contains(&s.name) && s.replicated);
    let start = move |_| {
        let h: u64 = until.get_untracked().parse().unwrap_or(0);
        let until_ms = (h > 0).then(|| js_sys::Date::now() as u64 + h * 3_600_000);
        let body = serde_json::to_string(&DedupRunRequest { until_ms, shares: Vec::new() }).ok();
        let url = job_url(&key, if kind == DedupKind::Run { "run" } else { "estimate" });
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match api::start_task(&url, body).await {
                Ok(_) => {
                    ov.reload();
                    let _ = modal.try_set(Open::None);
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
            let _ = busy.try_set(false);
        });
    };
    let title = tf(if kind == DedupKind::Run { "dd.run_title" } else { "dd.estimate_title" }, &[("name", &job_name(&j.key))]);
    view! {
        <Modal title=title on_close=close>
            <p class="muted small">{tr(if kind == DedupKind::Run { "dd.run_text" } else { "dd.estimate_text" })}</p>
            <dl class="kvs">{kv_row("apps.d.shares", Signal::stored(which.join(", ")), "")}</dl>
            {crate::form::select_field("dd.until_label", Some("dd.until_help"), UNTIL, until.into(), Callback::new(move |v| until.set(v)))}
            {snapshots.then(|| ui::note(Tone::Info, icons::INFO, view! { {tr("dd.snapshots_note")} }.into_any()))}
            {replicated.then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tr("dd.warn.replicated")} }.into_any()))}
            {form_error(error)}
            <div class="dialog-foot">
                <button class="btn ghost" type="button" on:click=move |_| close.run(())>{tr("cancel")}</button>
                <button class="btn primary" type="button" disabled=move || busy.get() on:click=start>
                    {spinner(busy)}{tr(if kind == DedupKind::Run { "dd.start" } else { "dd.estimate" })}
                </button>
            </div>
        </Modal>
    }
    .into_any()
}

const MIN_SIZES: &[(&str, &str)] = &[("4096", "dd.sz.4k"), ("16384", "dd.sz.16k"), ("65536", "dd.sz.64k"), ("262144", "dd.sz.256k"), ("1048576", "dd.sz.1m"), ("4194304", "dd.sz.4m")];
const BLOCKS: &[(&str, &str)] = &[("32768", "dd.sz.32k"), ("65536", "dd.sz.64k"), ("131072", "dd.sz.128k"), ("262144", "dd.sz.256k"), ("524288", "dd.sz.512k"), ("1048576", "dd.sz.1m")];
const THREADS: &[(&str, &str)] = &[("", "dd.auto"), ("1", "dd.n1"), ("2", "dd.n2"), ("4", "dd.n4"), ("8", "dd.n8")];
const MEMORY: &[(&str, &str)] = &[("512", "dd.mem.512"), ("1024", "dd.mem.1024"), ("2048", "dd.mem.2048"), ("4096", "dd.mem.4096")];

fn opt_n(v: Option<u8>) -> String {
    v.map(|n| n.to_string()).unwrap_or_default()
}

fn settings_dialog(j: DedupJobView, excluded: Vec<String>, modal: RwSignal<Open>, ov: Remote<DedupOverview>) -> AnyView {
    let fresh = j.settings.is_none();
    let first_block = j.settings.as_ref().map(|s| s.block_size);
    let d = RwSignal::new(j.settings.clone().unwrap_or_default());
    let grid = RwSignal::new(d.with_untracked(|s| s.window.clone()));
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let ask_off = RwSignal::new(false);
    let ask_reset = RwSignal::new(false);
    let typed = RwSignal::new(String::new());
    let close = Callback::new(move |_| modal.set(Open::None));
    let key = StoredValue::new(j.key.clone());
    let empty_window = Signal::derive(move || !grid.with(|g| g.contains('1')));
    let save = move |_| {
        let mut body = d.get_untracked();
        body.window = grid.get_untracked();
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let url = format!("{URL}/jobs/{}", api::encode(&key.get_value()));
            match api::put::<_, DedupJobView>(&url, &body).await {
                Ok(_) => {
                    if crate::config::apply_now(&[crate::config::keys::DEDUP, crate::config::keys::SCHEDULER], error).await {
                        ov.reload();
                        let _ = modal.try_set(Open::None);
                    }
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
            let _ = busy.try_set(false);
        });
    };
    let turn_off = move |_| {
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let url = format!("{URL}/jobs/{}", api::encode(&key.get_value()));
            match api::delete::<rynas_api_types::StagedChanges>(&url).await {
                Ok(_) => {
                    if crate::config::apply_now(&[crate::config::keys::DEDUP, crate::config::keys::SCHEDULER], error).await {
                        ov.reload();
                        let _ = modal.try_set(Open::None);
                    }
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
            let _ = busy.try_set(false);
        });
    };
    let reset = move |_| {
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let k = key.get_value();
            let body = serde_json::to_string(&DedupResetRequest { confirm: k.clone() }).ok();
            match crate::tasks::run_task(&job_url(&k, "reset"), body).await {
                Ok(task) if task.status == TaskStatus::Ok => {
                    toast(Tone::Ok, t("dd.reset_done").to_string());
                    ov.reload();
                    let _ = ask_reset.try_set(false);
                }
                Ok(task) => {
                    let _ = error.try_set(Some(crate::tasks::task_error(&task)));
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
            let _ = busy.try_set(false);
        });
    };
    let (enabled, set_enabled) = lens_bool(d, |s: &VolumeDedup| s.enabled, |s, v| s.enabled = v);
    let (awake, set_awake) = lens_bool(d, |s: &VolumeDedup| s.only_if_awake, |s, v| s.only_if_awake = v);
    let (partial, set_partial) = lens_bool(d, |s: &VolumeDedup| s.partial, |s, v| s.partial = v);
    let (zeroes, set_zeroes) = lens_bool(d, |s: &VolumeDedup| s.skip_zeroes, |s, v| s.skip_zeroes = v);
    let (min, set_min) = lens(d, |s: &VolumeDedup| s.min_file_size.to_string(), |s, v| s.min_file_size = v.parse().unwrap_or(s.min_file_size));
    let (block, set_block) = lens(d, |s: &VolumeDedup| s.block_size.to_string(), |s, v| s.block_size = v.parse().unwrap_or(s.block_size));
    let (cpu, set_cpu) = lens(d, |s: &VolumeDedup| opt_n(s.cpu_threads), |s, v| s.cpu_threads = v.parse().ok());
    let (io, set_io) = lens(d, |s: &VolumeDedup| opt_n(s.io_threads), |s, v| s.io_threads = v.parse().ok());
    let (mem, set_mem) = lens(d, |s: &VolumeDedup| s.memory_max_mib.to_string(), |s, v| s.memory_max_mib = v.parse().unwrap_or(s.memory_max_mib));
    let block_changed = Signal::derive(move || first_block.is_some_and(|b| d.with(|s| s.block_size != b)));
    let name = job_name(&j.key);
    let name2 = name.clone();
    view! {
        <Modal title=tf("dd.settings_title", &[("name", &name)]) on_close=close wide=true>
            <div class="form">
                {(!fresh).then(|| switch("dd.enabled", Some("dd.enabled_help"), enabled, set_enabled))}
                <div class="field">
                    <span class="field-label">{tr("dd.window")}</span>
                    {week_grid(LEVELS, grid, "dd.window", None)}
                    <span class="field-help">{tr("dd.window_help")}</span>
                </div>
                {move || empty_window.get().then(|| view! { <p class="form-error">{tr("dd.window_empty")}</p> })}
                {switch("dd.only_awake", Some("dd.only_awake_help"), awake, set_awake)}
                <details class="advanced">
                    <summary>{tr("dd.advanced")}</summary>
                    <div class="form">
                        {crate::form::select_field("dd.min_size", Some("dd.min_size_help"), MIN_SIZES, min, set_min)}
                        {crate::form::select_field("dd.block", Some("dd.block_help"), BLOCKS, block, set_block)}
                        {move || block_changed.get().then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tr("dd.block_changed")} }.into_any()))}
                        {switch("dd.partial", Some("dd.partial_help"), partial, set_partial)}
                        {switch("dd.zeroes", Some("dd.zeroes_help"), zeroes, set_zeroes)}
                        {crate::form::select_field("dd.cpu", None, THREADS, cpu, set_cpu)}
                        {crate::form::select_field("dd.io", Some("dd.io_help"), THREADS, io, set_io)}
                        {crate::form::select_field("dd.memory", None, MEMORY, mem, set_mem)}
                    </div>
                </details>
                <div class="field">
                    <span class="field-label">{tr("dd.always_excluded")}</span>
                    <span class="field-help mono wrap-anywhere">{excluded.join("  ")}</span>
                </div>
                {(!fresh).then(|| view! {
                    <div class="btn-row">
                        <button class="btn ghost sm" type="button" on:click=move |_| ask_reset.set(true)>{tr("dd.reset")}</button>
                        <button class="btn ghost sm danger" type="button" on:click=move |_| ask_off.set(true)>{tr("sec.disable")}</button>
                    </div>
                })}
                {move || ask_reset.get().then(|| view! {
                    {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tr("dd.reset_text")} }.into_any())}
                    {typed_confirm(key.get_value(), typed)}
                    <div class="btn-row">
                        <button class="btn danger sm" type="button" disabled=move || busy.get() || typed.get().trim() != key.get_value() on:click=reset>{spinner(busy)}{tr("dd.reset")}</button>
                    </div>
                })}
                {move || ask_off.get().then(|| view! {
                    {ui::note(Tone::Info, icons::INFO, view! { {tf("dd.turn_off_text", &[("name", &name2)])} }.into_any())}
                    <div class="btn-row">
                        <button class="btn danger sm" type="button" disabled=move || busy.get() on:click=turn_off>{spinner(busy)}{tr("sec.disable")}</button>
                    </div>
                })}
                {form_error(error)}
            </div>
            <div class="dialog-foot">
                <button class="btn ghost" type="button" on:click=move |_| close.run(())>{tr("cancel")}</button>
                <button class="btn primary" type="button" disabled=move || busy.get() || empty_window.get() on:click=save>{spinner(busy)}{tr("save")}</button>
            </div>
        </Modal>
    }
    .into_any()
}

fn history_dialog(key: String, modal: RwSignal<Open>) -> AnyView {
    let h = Remote::<Vec<DedupRecord>>::new(job_url(&key, "history"));
    let close = Callback::new(move |_| modal.set(Open::None));
    view! {
        <Modal title=tf("dd.history_title", &[("name", &job_name(&key))]) on_close=close>
            {h.view(4, move || {
                let list = h.data.get().unwrap_or_default();
                if list.is_empty() {
                    return view! { <p class="muted">{tr("never_run")}</p> }.into_any();
                }
                view! {
                    <ul class="plain-list dd-history">
                        {list.into_iter().map(|r| {
                            let (tn, k) = outcome_text(r.outcome);
                            let took = i18n::duration(r.end_ms.saturating_sub(r.start_ms) / 1000);
                            let detail = [
                                r.by.as_ref().map(|b| tf("dd.by", &[("task", &crate::tasks::task_label(b))])),
                                (!r.message.is_empty()).then(|| r.message.clone()),
                                (r.skipped_files > 0).then(|| tf("dd.skipped_files", &[("n", &r.skipped_files.to_string())])),
                            ]
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>()
                            .join(" · ");
                            view! {
                                <li>
                                    <div class="dd-hrow">
                                        <Status tone=tn text=t(k).to_string() />
                                        <span class="small tnum">{record_line(&r)}</span>
                                        <span class="muted small tnum">{took}</span>
                                    </div>
                                    {(!detail.is_empty()).then(|| view! { <p class="muted small wrap-anywhere">{detail}</p> })}
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                }
                .into_any()
            })}
            <div class="dialog-foot"><button class="btn primary" type="button" on:click=move |_| close.run(())>{tr("close")}</button></div>
        </Modal>
    }
    .into_any()
}

// ------------------------------------------------------------------ share editor

/// The share editor's switch (General tab, next to compression and no copy-on-write):
/// disabled with the reason on encrypted and no copy-on-write folders; the recycle bin
/// and extra excludes once on; the state of the filesystem's job.
pub fn share_fields(d: RwSignal<Share>, nocow_now: bool, name: Option<String>) -> AnyView {
    let ov = overview();
    let reason = Signal::derive(move || {
        let local = d.with(|s| {
            if s.encrypted {
                Some(DedupBlock::Encrypted)
            } else if nocow_now || s.subvolume.as_ref().is_some_and(|v| v.nocow) {
                Some(DedupBlock::Nocow)
            } else {
                None
            }
        });
        local.or_else(|| {
            let n = name.clone()?;
            ov.data.with(|o| {
                let o = o.as_ref()?;
                o.other_shares.iter().chain(o.jobs.iter().flat_map(|j| j.shares.iter())).find(|s| s.name == n).and_then(|s| s.block)
            })
        })
    });
    let job = Signal::derive(move || {
        let path = d.with(|s| s.path.clone());
        ov.data.with(|o| {
            o.as_ref()?.jobs.iter().find(|j| j.shares.iter().any(|s| s.path == path && !path.is_empty())).map(|j| (j.key.clone(), j.settings.is_some()))
        })
    });
    let get = |s: &Share| s.dedup.as_ref().is_some_and(|x| x.enabled);
    let (on, set_on) = (
        Signal::derive(move || d.with(get)),
        Callback::new(move |v: bool| {
            d.update(|s| {
                let mut x = s.dedup.clone().unwrap_or_default();
                x.enabled = v;
                s.dedup = (x != ShareDedup::default()).then_some(x);
            })
        }),
    );
    let (recycle, set_recycle) = (
        Signal::derive(move || d.with(|s| s.dedup.as_ref().is_some_and(|x| x.include_recycle))),
        Callback::new(move |v: bool| d.update(|s| s.dedup.get_or_insert_with(Default::default).include_recycle = v)),
    );
    let (ex, set_ex) = (
        Signal::derive(move || d.with(|s| s.dedup.as_ref().map(|x| x.excludes.join("\n")).unwrap_or_default())),
        Callback::new(move |v: String| {
            d.update(|s| {
                s.dedup.get_or_insert_with(Default::default).excludes =
                    v.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
            })
        }),
    );
    let bad = Signal::derive(move || {
        d.with(|s| s.dedup.as_ref().and_then(|x| x.excludes.iter().find_map(|p| rynas_api_types::dedup::exclude_valid(p).err().map(|e| format!("{p}: {e}")))))
    });
    view! {
        <div class="field-group">
            {move || match reason.get() {
                Some(b) if !on.get() => view! {
                    <div class="switch-row disabled">
                        <input type="checkbox" role="switch" class="switch" disabled=true aria-describedby="dd-why" />
                        <span class="switch-text">
                            <span class="switch-label">{tr("dd.share_switch")}</span>
                            <span class="field-help" id="dd-why">{tr(block_key(b))}</span>
                        </span>
                    </div>
                }
                .into_any(),
                _ => switch("dd.share_switch", Some("dd.share_switch_help"), on, set_on),
            }}
            <div class="form" hidden=move || !on.get()>
                {move || reason.get().map(|b| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tr(block_key(b))} }.into_any()))}
                {move || match job.get() {
                    Some((k, false)) => Some(ui::note(Tone::Info, icons::INFO, view! {
                        <span>{tr("dd.share_job_off")}" "<a href=job_href(&k)>{tr("wiz.open_volume")}</a></span>
                    }.into_any())),
                    _ => None,
                }}
                {switch("dd.include_recycle", Some("dd.include_recycle_help"), recycle, set_recycle)}
                {crate::form::area_field("dd.excludes", ex, set_ex, Some("dd.excludes_help"), "cache/\n*.iso")}
                {move || bad.get().map(|e| view! { <p class="form-error">{e}</p> })}
            </div>
        </div>
    }
    .into_any()
}
