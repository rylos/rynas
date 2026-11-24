//! Syncthing (`docs/phase2/syncthing.md` §4): the catalogue app's folders and devices next to
//! the shares, read from rynasd's event-fed state (`GET /api/v1/syncthing`, polled every 5 s
//! while the page is visible).
//!
//! - Not installed: a link to the app in the catalogue and "Adopt an existing Syncthing"
//!   (the adoption dialog, [`adopt`]).
//! - Installed but stopped or not answering: why, and a link to the app; the settings stay.
//! - Normal: this NAS as a device, the folders (state, errors, conflict copies, devices with
//!   their completion; Pause, Resume, Rescan), the devices, a note per share without a
//!   snapshot schedule (versions come from the share's snapshots), the alert settings.
//!
//! Adding devices and folders stays in Syncthing's own interface ("Open Syncthing").

mod adopt;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::apps::InstalledAppDetail;
use rynas_api_types::syncthing::{MAX_UNSEEN_DAYS, SyncthingDevice, SyncthingFolder, SyncthingSettings, SyncthingState};

use crate::api;
use crate::form::{SaveBar, form_error, number_field, save_bar_with, switch};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::toast::toast;
use crate::ui::{self, DlgKind, EmptyState, Folds, PageHeader, Remote, Tone, WidgetHead};

/// How often the page reads the state again while visible.
const POLL: std::time::Duration = std::time::Duration::from_secs(5);
/// Item errors shown per folder (the server keeps the first 20).
const SHOWN_ERRORS: usize = 20;

fn state_url(stack: Option<&str>) -> String {
    match stack {
        Some(s) => format!("/api/v1/syncthing?stack={}", api::encode(s)),
        None => "/api/v1/syncthing".to_string(),
    }
}

/// What the page's parts share.
#[derive(Clone, Copy)]
struct Page {
    st: Remote<SyncthingState>,
    /// The folder whose action runs.
    busy: RwSignal<Option<String>>,
    folds: Folds,
    /// Where the app's own interface answers (its web route or port).
    open: RwSignal<Option<String>>,
    adopt: RwSignal<bool>,
    settings: RwSignal<bool>,
}

impl Page {
    /// `?stack=` of the folder actions when more than one stack of the app is installed.
    fn stack_query(self) -> String {
        self.st.data.with_untracked(|s| {
            s.as_ref()
                .filter(|s| !s.other_stacks.is_empty())
                .and_then(|s| s.installed.as_ref())
                .map(|i| format!("?stack={}", api::encode(&i.stack)))
                .unwrap_or_default()
        })
    }

    fn act(self, id: String, action: &'static str) {
        if self.busy.get_untracked().is_some() {
            return;
        }
        self.busy.set(Some(id.clone()));
        let url = format!("/api/v1/syncthing/folders/{}/{action}{}", api::encode(&id), self.stack_query());
        spawn_local(async move {
            let r = api::post_empty::<()>(&url, None).await;
            let _ = self.busy.try_set(None);
            match r {
                Ok(()) => {
                    if action == "rescan" {
                        toast(Tone::Ok, t("stg.rescan_asked").to_string());
                    }
                    self.st.reload();
                }
                Err(e) if e.status() == 401 => {}
                Err(e) => toast(Tone::Err, e.text()),
            }
        });
    }
}

#[component]
pub fn SyncthingPage() -> impl IntoView {
    let st = Remote::<SyncthingState>::new(state_url(None));
    crate::live::poll(POLL, move || st.reload());
    let pg = Page { st, busy: RwSignal::new(None), folds: Folds::new(), open: RwSignal::new(None), adopt: RwSignal::new(false), settings: RwSignal::new(false) };
    // the app's address, read again only when the installed stack changes
    let stack = Memo::new(move |_| st.data.with(|s| s.as_ref().and_then(|s| s.installed.as_ref().map(|i| i.stack.clone()))));
    Effect::new(move |_| {
        let Some(stack) = stack.get() else {
            pg.open.set(None);
            return;
        };
        spawn_local(async move {
            let url = format!("/api/v1/apps/installed/{}", api::encode(&stack));
            if let Ok(d) = api::get::<InstalledAppDetail>(&url).await {
                let _ = pg.open.try_set(super::apps::open_href(&d.app));
            }
        });
    });
    view! {
        <div class="page">
            <PageHeader icon=icons::FOLDER_SYNC title="nav.syncthing" subtitle="stg.subtitle">
                {move || pg.open.get().map(|href| view! {
                    <a class="btn ghost" href=href target="_blank" rel="noopener noreferrer"><Icon icon=icons::EXTERNAL_LINK size=16 />{tr("stg.open")}</a>
                })}
                <button type="button" class="btn ghost" on:click=move |_| pg.adopt.set(true)><Icon icon=icons::IMPORT size=16 />{tr("stg.adopt")}</button>
            </PageHeader>
            {st.view(6, move || content(pg))}
            {move || pg.adopt.get().then(|| adopt::dialog(Callback::new(move |_| pg.adopt.set(false)), Callback::new(move |_| st.reload())))}
            {move || pg.settings.get().then(|| {
                let cur = st.data.with_untracked(|s| s.as_ref().map(|s| s.settings.clone()).unwrap_or_default());
                settings_dialog(cur, Callback::new(move |_| pg.settings.set(false)), Callback::new(move |_| st.reload()))
            })}
        </div>
    }
}

/// The page once the state is known.
fn content(pg: Page) -> AnyView {
    (move || {
        let Some(s) = pg.st.data.get() else { return ().into_any() };
        let Some(inst) = s.installed.clone() else { return not_installed(pg) };
        let up = inst.running && s.reachable;
        let (pill, picker, card) = (app_pill(&s), stack_picker(pg, &s), app_card(&s));
        let notes = if up { share_notes(&s) } else { None };
        let folders = up.then(|| folders_card(pg, &s));
        let devices = up.then(|| devices_card(&s));
        let settings = settings_card(pg, &s.settings);
        view! {
            <div class="grid">
                <section class="card widget w-12">
                    <WidgetHead icon=icons::FOLDER_SYNC title="stg.app">{pill}</WidgetHead>
                    {picker}
                    {card}
                </section>
                {notes}
                {folders}
                {devices}
                {settings}
            </div>
        }
        .into_any()
    })
    .into_any()
}

fn not_installed(pg: Page) -> AnyView {
    view! {
        <div class="card">
            <EmptyState
                icon=icons::FOLDER_SYNC
                title=Signal::derive(move || t("stg.none_title").to_string())
                text=Signal::derive(move || t("stg.none_text").to_string())
            >
                <div class="sec-actions">
                    <a class="btn primary" href="/apps?app=syncthing"><Icon icon=icons::PUZZLE size=16 />{tr("stg.install")}</a>
                    <button type="button" class="btn ghost" on:click=move |_| pg.adopt.set(true)><Icon icon=icons::IMPORT size=16 />{tr("stg.adopt")}</button>
                </div>
            </EmptyState>
        </div>
    }
    .into_any()
}

/// Running and answering, stopped, or not answering.
fn app_pill(s: &SyncthingState) -> AnyView {
    let (tone, key) = match &s.installed {
        Some(i) if !i.running => (Tone::Unknown, "stg.stopped"),
        Some(_) if !s.reachable => (Tone::Err, "dk.eng.down"),
        _ => (Tone::Ok, "stg.running"),
    };
    ui::status(tone, t(key).to_string())
}

/// A choice between the installed stacks of the app (rare: usually one).
fn stack_picker(pg: Page, s: &SyncthingState) -> Option<AnyView> {
    if s.other_stacks.is_empty() {
        return None;
    }
    let cur = s.installed.as_ref().map(|i| i.stack.clone()).unwrap_or_default();
    let mut all: Vec<String> = s.other_stacks.clone();
    all.push(cur.clone());
    all.sort();
    let id = crate::form::fid();
    Some(
        view! {
            <div class="field">
                <label class="field-label" for=id.clone()>{tr("stg.stack")}</label>
                <select id=id class="input" on:change=move |ev| pg.st.load(state_url(Some(&event_target_value(&ev))))>
                    {all.into_iter().map(|x| { let sel = x == cur; view! { <option value=x.clone() selected=sel>{x.clone()}</option> } }).collect_view()}
                </select>
            </div>
        }
        .into_any(),
    )
}

fn app_card(s: &SyncthingState) -> AnyView {
    let inst = s.installed.clone().unwrap_or_default();
    let apps_href = format!("/apps?stack={}", api::encode(&inst.stack));
    let problem = match (inst.running, s.reachable) {
        (false, _) => Some(ui::note(Tone::Info, icons::INFO, ui::text(t("stg.stopped_text").to_string()))),
        (true, false) => Some(ui::note(
            Tone::Err,
            icons::TRIANGLE_ALERT,
            ui::text(match &s.error {
                Some(e) => tf("stg.unreachable_why", &[("error", e)]),
                None => t("stg.unreachable_text").to_string(),
            }),
        )),
        _ => None,
    };
    let device = s.device.clone();
    view! {
        {problem}
        <dl class="kvs">
            {device.as_ref().map(|d| ui::kv_text("stg.this_device", format!("{} ({})", d.name, d.id_short), ""))}
            {device.as_ref().filter(|d| !d.version.is_empty()).map(|d| ui::kv_text("stg.version", d.version.clone(), "mono"))}
            {ui::kv_text("stg.stack", inst.stack.clone(), "mono")}
            {s.read_ms.map(|ms| ui::kv_text("stg.read", i18n::datetime(ms), ""))}
        </dl>
        <div class="sec-actions">
            <a class="btn ghost sm" href=apps_href><Icon icon=icons::PUZZLE size=14 />{tr("stg.in_apps")}</a>
        </div>
    }
    .into_any()
}

/// A note per synchronized share without scheduled snapshots: versions come from them.
fn share_notes(s: &SyncthingState) -> Option<AnyView> {
    let missing: Vec<String> = s.shares.iter().filter(|n| !n.snapshots_scheduled).map(|n| n.share.clone()).collect();
    if missing.is_empty() {
        return None;
    }
    Some(
        view! {
            <div class="w-12">
                {missing.into_iter().map(|share| ui::note(Tone::Warn, icons::CAMERA, view! {
                    {tf("stg.no_snapshots", &[("share", &share)])}" "
                    <a href="/snapshots">{tr("stg.to_snapshots")}</a>
                }.into_any())).collect_view()}
            </div>
        }
        .into_any(),
    )
}

// ------------------------------------------------------------------ folders

fn type_key(t: &str) -> &'static str {
    match t {
        "sendreceive" => "stg.type.sendreceive",
        "sendonly" => "stg.type.sendonly",
        "receiveonly" => "stg.type.receiveonly",
        "receiveencrypted" => "stg.type.receiveencrypted",
        _ => "stg.type.other",
    }
}

/// The local completion of a folder, 0–100.
fn local_pct(f: &SyncthingFolder) -> f64 {
    if f.global_bytes == 0 {
        return 100.0;
    }
    (f.global_bytes.saturating_sub(f.need_bytes) as f64 / f.global_bytes as f64 * 100.0).clamp(0.0, 100.0)
}

/// The state pill of a folder.
fn folder_state(f: &SyncthingFolder) -> (Tone, String) {
    if f.paused && (f.paused_by_rynas || f.locked) {
        return (Tone::Info, t("stg.st.locked").to_string());
    }
    if f.paused {
        return (Tone::Unknown, t("stg.st.paused").to_string());
    }
    match f.state.as_str() {
        "error" => (Tone::Err, t("stg.st.error").to_string()),
        "syncing" | "sync-preparing" => (Tone::Info, tf("stg.st.syncing", &[("pct", &i18n::pct(local_pct(f)))])),
        "scanning" | "scan-waiting" => (Tone::Info, t("stg.st.scanning").to_string()),
        "sync-waiting" => (Tone::Info, t("stg.st.waiting").to_string()),
        "cleaning" => (Tone::Info, t("stg.st.cleaning").to_string()),
        "idle" if f.need_bytes > 0 => (Tone::Warn, t("stg.st.behind").to_string()),
        "idle" => (Tone::Ok, t("stg.st.idle").to_string()),
        _ => (Tone::Unknown, t("stg.st.unknown").to_string()),
    }
}

/// "share › subfolder", else the path inside the app.
fn place(f: &SyncthingFolder) -> String {
    match (&f.share, f.sub_path.as_deref().filter(|s| !s.is_empty())) {
        (Some(s), Some(sub)) => format!("{s} › {sub}"),
        (Some(s), None) => s.clone(),
        _ => f.path.clone(),
    }
}

fn ago_ms(ms: u64) -> String {
    let now = js_sys::Date::now() as u64;
    i18n::ago(now.saturating_sub(ms) / 1000)
}

/// A fold of one folder's part (errors, conflict copies, devices) whose open state survives
/// the polls.
fn sub_fold(folds: Folds, key: String, title: String, open: bool, body: AnyView) -> AnyView {
    let is_open = folds.open(key.clone(), open);
    let k = StoredValue::new(key);
    view! {
        <details class="fold sub" open=is_open on:toggle=move |ev| k.with_value(|k| folds.toggled(k, &ev))>
            <summary><Icon icon=icons::CHEVRON_RIGHT size=14 /><span class="fold-title">{title}</span></summary>
            <div class="fold-body">{body}</div>
        </details>
    }
    .into_any()
}

fn folders_card(pg: Page, s: &SyncthingState) -> AnyView {
    let list = if s.folders.is_empty() {
        ui::empty_line("stg.no_folders")
    } else {
        s.folders.iter().map(|f| folder_view(pg, f.clone())).collect_view().into_any()
    };
    view! {
        <section class="card widget w-12">
            <WidgetHead icon=icons::FOLDER title="stg.folders" />
            <div class="stg-list">{list}</div>
        </section>
    }
    .into_any()
}

fn folder_view(pg: Page, f: SyncthingFolder) -> AnyView {
    let (tone, state) = folder_state(&f);
    let id = f.id.clone();
    let meta_scan = f.last_scan_ms.map(|ms| tf("stg.last_scan", &[("when", &ago_ms(ms))]));
    let meta_file = f.last_file.clone().map(|p| tf("stg.last_file", &[("file", &p)]));
    let error = f.error.clone().map(|e| ui::note(Tone::Err, icons::TRIANGLE_ALERT, ui::breakable(&e, "small")));
    let errors = (!f.errors.is_empty()).then(|| {
        let more = (f.error_count as usize).saturating_sub(f.errors.len().min(SHOWN_ERRORS));
        let rows = f
            .errors
            .iter()
            .take(SHOWN_ERRORS)
            .map(|e| view! { <li>{ui::breakable(&e.path, "mono")}": "{e.error.clone()}</li> })
            .collect_view();
        let body = view! {
            <ul class="plain-list small stg-lines">{rows}</ul>
            {(more > 0).then(|| view! { <p class="small muted">{tf("stg.errors_more", &[("n", &i18n::count(more as u64))])}</p> })}
        }
        .into_any();
        sub_fold(pg.folds, format!("{id}:errors"), tf("stg.errors", &[("n", &i18n::count(f.error_count as u64))]), true, body)
    });
    let conflicts = (f.conflicts > 0 || !f.conflict_files.is_empty()).then(|| {
        let body = if f.conflict_files.is_empty() {
            ui::empty_line("stg.conflicts_gone")
        } else {
            let rows = f.conflict_files.iter().map(|p| view! { <li>{ui::breakable(p, "mono")}</li> }).collect_view();
            view! {
                <p class="small muted">{tr("stg.conflicts_help")}</p>
                <ul class="plain-list small stg-lines">{rows}</ul>
            }
            .into_any()
        };
        sub_fold(pg.folds, format!("{id}:conflicts"), tf("stg.conflicts", &[("n", &i18n::count(f.conflicts as u64))]), false, body)
    });
    let devices = (!f.completion.is_empty()).then(|| {
        let rows = f
            .completion
            .iter()
            .map(|c| {
                let (tone, text) = if !c.connected {
                    (Tone::Unknown, t("stg.dev_offline").to_string())
                } else if c.completion >= 100.0 {
                    (Tone::Ok, t("stg.dev_in_sync").to_string())
                } else {
                    (Tone::Info, tf("stg.dev_pct", &[("pct", &i18n::pct(c.completion)), ("size", &i18n::bytes(c.need_bytes))]))
                };
                view! { <li class="stg-dev"><span>{c.name.clone()}" "<span class="muted mono">{c.id_short.clone()}</span></span>{ui::status(tone, text)}</li> }
            })
            .collect_view();
        sub_fold(pg.folds, format!("{id}:devices"), tf("stg.shared_with", &[("n", &i18n::count(f.completion.len() as u64))]), false, view! { <ul class="plain-list small stg-lines">{rows}</ul> }.into_any())
    });
    let paused = f.paused;
    let locked = f.locked;
    let busy = pg.busy;
    let off = Signal::derive(move || busy.with(Option::is_some));
    let (id_p, id_s, label) = (id.clone(), id.clone(), f.label.clone());
    let toggle = if paused {
        let go = move |_| pg.act(id_p.clone(), "resume");
        view! { <button type="button" class="btn ghost sm" disabled=move || off.get() || locked title=locked.then(|| t("stg.locked_help")) on:click=go><Icon icon=icons::PLAY size=14 />{tr("stg.resume")}</button> }.into_any()
    } else {
        let go = move |_| pg.act(id_p.clone(), "pause");
        view! { <button type="button" class="btn ghost sm" disabled=move || off.get() on:click=go><Icon icon=icons::PAUSE size=14 />{tr("stg.pause")}</button> }.into_any()
    };
    let rescan = move |_| pg.act(id_s.clone(), "rescan");
    view! {
        <section class="status-card stg-folder" aria-label=label.clone()>
            <div class="status-card-head">
                <strong>{label.clone()}</strong>
                {ui::status(tone, state)}
                {ui::chip(Tone::Info, ui::text(t(type_key(&f.folder_type)).to_string()))}
                {(f.conflicts > 0).then(|| ui::chip(Tone::Warn, ui::text(tf("stg.conflicts", &[("n", &i18n::count(f.conflicts as u64))]))))}
            </div>
            <p class="status-meta small">
                {ui::breakable(&place(&f), "")}
                {meta_scan.map(|s| view! { " · "<span>{s}</span> })}
                {meta_file.map(|s| view! { " · "<span>{s}</span> })}
            </p>
            {f.paused_by_rynas.then(|| ui::note(Tone::Info, icons::LOCK, ui::text(t("stg.paused_by_rynas").to_string())))}
            {error}
            {errors}
            {conflicts}
            {devices}
            <div class="sec-actions">
                {toggle}
                <button type="button" class="btn ghost sm" disabled=move || off.get() || paused on:click=rescan><Icon icon=icons::REFRESH_CW size=14 />{tr("stg.rescan")}</button>
            </div>
        </section>
    }
    .into_any()
}

// ------------------------------------------------------------------ devices

fn device_connection(d: &SyncthingDevice) -> AnyView {
    if d.paused {
        return ui::status(Tone::Unknown, t("stg.dev_paused").to_string());
    }
    if d.connected {
        let how = [d.address.clone(), d.connection_type.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ");
        return view! {
            {ui::status(Tone::Ok, t("stg.connected").to_string())}
            {(!how.is_empty()).then(|| view! { <span class="cell-sub mono">{how}</span> })}
        }
        .into_any();
    }
    let seen = match d.last_seen_ms {
        Some(ms) => tf("stg.last_seen", &[("when", &ago_ms(ms))]),
        None => t("stg.never_seen").to_string(),
    };
    view! {
        {ui::status(Tone::Unknown, t("stg.disconnected").to_string())}
        <span class="cell-sub">{seen}</span>
    }
    .into_any()
}

fn devices_card(s: &SyncthingState) -> AnyView {
    let body = if s.devices.is_empty() {
        ui::empty_line("stg.no_devices")
    } else {
        let rows = s
            .devices
            .iter()
            .map(|d| {
                ui::row(vec![
                    ui::cell("stg.col.device", "", ui::two_lines(ui::text(d.name.clone()), Some(d.id_short.clone()), "cell-sub mono")),
                    ui::cell("stg.col.connection", "", view! { <div class="stg-conn">{device_connection(d)}</div> }.into_any()),
                    ui::cell("stg.col.folders", "wrap", ui::text(if d.folders.is_empty() { ui::DASH.to_string() } else { d.folders.join(", ") })),
                ])
            })
            .collect::<Vec<_>>();
        ui::table("", &["stg.col.device", "stg.col.connection", "stg.col.folders"], rows)
    };
    view! {
        <section class="card widget w-12">
            <WidgetHead icon=icons::LAPTOP title="stg.devices" />
            {body}
        </section>
    }
    .into_any()
}

// ------------------------------------------------------------------ settings

fn settings_card(pg: Page, set: &SyncthingSettings) -> AnyView {
    let days = if set.device_unseen_days == 0 { t("stg.off").to_string() } else { tf("pcb.age.d", &[("n", &i18n::count(set.device_unseen_days as u64))]) };
    let conflicts = t(if set.conflicts_alert { "stg.on" } else { "stg.off" }).to_string();
    view! {
        <section class="card widget w-12">
            <WidgetHead icon=icons::BELL title="stg.alerts">
                <button type="button" class="btn ghost sm" on:click=move |_| pg.settings.set(true)><Icon icon=icons::PENCIL size=14 />{tr("edit")}</button>
            </WidgetHead>
            <dl class="kvs">
                {ui::kv_text("stg.unseen_days", days, "")}
                {ui::kv_text("stg.conflicts_alert", conflicts, "")}
            </dl>
        </section>
    }
    .into_any()
}

fn settings_dialog(cur: SyncthingSettings, close: Callback<()>, saved: Callback<()>) -> AnyView {
    let days = RwSignal::new(cur.device_unseen_days.to_string());
    let alert = RwSignal::new(cur.conflicts_alert);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let parsed = move || days.with(|d| d.trim().parse::<u32>().ok().filter(|n| *n <= MAX_UNSEEN_DAYS));
    let save = move |_| {
        let Some(n) = parsed() else {
            error.set(Some(tf("stg.days_invalid", &[("max", &MAX_UNSEEN_DAYS.to_string())])));
            return;
        };
        busy.set(true);
        error.set(None);
        let body = SyncthingSettings { device_unseen_days: n, conflicts_alert: alert.get_untracked() };
        spawn_local(async move {
            let r = api::put::<_, SyncthingSettings>("/api/v1/syncthing/settings", &body).await;
            let _ = busy.try_set(false);
            match r {
                Ok(_) => {
                    saved.try_run(());
                    close.try_run(());
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    };
    let body = view! {
        <form class="form" on:submit=move |ev| { ev.prevent_default(); save(()); }>
            {number_field("stg.unseen_days", Some("stg.unseen_days_help"), days.into(), Callback::new(move |v| days.set(v)), 0, MAX_UNSEEN_DAYS as i64)}
            {switch("stg.conflicts_alert", Some("stg.conflicts_alert_help"), alert.into(), Callback::new(move |v| alert.set(v)))}
            {form_error(error)}
        </form>
    }
    .into_any();
    let foot = save_bar_with(SaveBar { can_save: Signal::derive(move || parsed().is_some()), ..SaveBar::new(busy, "save", close, Callback::new(save)) });
    ui::dialog(DlgKind::Normal, Signal::derive(|| t("stg.alerts_edit").to_string()), close, body, Some(foot))
}
