//! "Adopt an existing Syncthing" (`docs/phase2/syncthing.md` §3, API of phase S1a): the
//! instances found on this NAS (or a home folder copied from elsewhere, with its volume
//! lines typed in), then the preview of what the adoption does (folders mapped to the
//! shares, markers, `.stignore` changes, the user the app runs as), then the task.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::UserView;
use rynas_api_types::syncthing::{
    AdoptCandidate, AdoptDatabase, AdoptFolder, AdoptSourceKind, AdoptSourceRef, AdoptVolume, MarkerState, SyncthingAdoptPreview,
    SyncthingAdoptPreviewRequest, SyncthingAdoptRequest, UnmappedReason,
};

use crate::api;
use crate::form::{In, SaveBar, form_error, save_bar_with, spinner, switch, text_field};
use crate::i18n::{t, tf, tr};
use crate::icons::{self, Icon};
use crate::tasks::{self, tasks};
use crate::toast::toast;
use crate::ui::{self, DlgKind, Remote, Tone};

const PREVIEW: &str = "/api/v1/syncthing/adopt/preview";
const ADOPT: &str = "/api/v1/syncthing/adopt";
/// The prefix of preview lines that do not block the adoption.
const INFO: &str = "info: ";

/// One typed volume line (`key` keeps the inputs while lines are removed).
#[derive(Clone, Debug, PartialEq)]
struct Line {
    key: u32,
    container: String,
    host: String,
}

/// The dialog's state.
#[derive(Clone, Copy)]
struct Adopt {
    /// The first answer: the candidates.
    found: RwSignal<Option<SyncthingAdoptPreview>>,
    /// The chosen candidate (its index), or the copied folder (`None` with `path_mode`).
    chosen: RwSignal<Option<usize>>,
    path_mode: RwSignal<bool>,
    path: RwSignal<String>,
    lines: RwSignal<Vec<Line>>,
    next_key: StoredValue<u32>,
    preview: RwSignal<Option<SyncthingAdoptPreview>>,
    run_as: RwSignal<Option<String>>,
    stop_source: RwSignal<bool>,
    stignore: RwSignal<bool>,
    checking: RwSignal<bool>,
    busy: RwSignal<bool>,
    error: RwSignal<Option<String>>,
}

impl Adopt {
    fn source(self) -> Option<AdoptSourceRef> {
        if self.path_mode.get_untracked() {
            let p = self.path.get_untracked().trim().to_string();
            return (!p.is_empty()).then_some(AdoptSourceRef { kind: AdoptSourceKind::Path, id: None, path: Some(p) });
        }
        let i = self.chosen.get_untracked()?;
        self.found.with_untracked(|f| f.as_ref().and_then(|f| f.candidates.get(i)).map(|c| c.source.clone()))
    }

    fn volumes(self) -> Vec<AdoptVolume> {
        if !self.path_mode.get_untracked() {
            return Vec::new();
        }
        self.lines.with_untracked(|l| {
            l.iter()
                .filter(|x| !x.container.trim().is_empty() && !x.host.trim().is_empty())
                .map(|x| AdoptVolume { container: x.container.trim().to_string(), host: x.host.trim().to_string() })
                .collect()
        })
    }

    /// Asks the preview of the chosen source (again after a change of the user).
    fn check(self) {
        let Some(source) = self.source() else { return };
        let req = SyncthingAdoptPreviewRequest { source: Some(source), volumes: self.volumes(), run_as: self.run_as.get_untracked() };
        self.checking.set(true);
        self.error.set(None);
        spawn_local(async move {
            let r = api::post::<_, SyncthingAdoptPreview>(PREVIEW, Some(&req)).await;
            let _ = self.checking.try_set(false);
            match r {
                Ok(p) => {
                    if self.run_as.get_untracked().is_none() {
                        let _ = self.run_as.try_set(p.run_as.clone());
                    }
                    let _ = self.preview.try_set(Some(p));
                }
                Err(e) if e.status() == 401 => {}
                Err(e) => {
                    let _ = self.preview.try_set(None);
                    let _ = self.error.try_set(Some(e.text()));
                }
            }
        });
    }

    fn pick(self, i: Option<usize>, path: bool) {
        self.chosen.set(i);
        self.path_mode.set(path);
        self.preview.set(None);
        self.run_as.set(None);
        self.stop_source.set(false);
        self.error.set(None);
        if !path {
            self.check();
        }
    }

    fn running(self) -> bool {
        self.preview.with(|p| p.as_ref().and_then(|p| p.source.as_ref()).is_some_and(|s| s.running))
    }

    fn can_adopt(self) -> bool {
        self.preview.with(|p| p.as_ref().is_some_and(|p| p.ok)) && (!self.running() || self.stop_source.get()) && !self.checking.get()
    }
}

/// The adoption dialog; `done` after the task started (the page reads its state again).
pub(super) fn dialog(close: Callback<()>, done: Callback<()>) -> AnyView {
    let a = Adopt {
        found: RwSignal::new(None),
        chosen: RwSignal::new(None),
        path_mode: RwSignal::new(false),
        path: RwSignal::new(String::new()),
        lines: RwSignal::new(Vec::new()),
        next_key: StoredValue::new(0),
        preview: RwSignal::new(None),
        run_as: RwSignal::new(None),
        stop_source: RwSignal::new(false),
        stignore: RwSignal::new(false),
        checking: RwSignal::new(false),
        busy: RwSignal::new(false),
        error: RwSignal::new(None),
    };
    let load_err = RwSignal::new(None::<String>);
    let load = move || {
        load_err.set(None);
        spawn_local(async move {
            match api::post::<_, SyncthingAdoptPreview>(PREVIEW, Some(&SyncthingAdoptPreviewRequest::default())).await {
                Ok(p) => {
                    let _ = a.found.try_set(Some(p));
                }
                Err(e) if e.status() == 401 => {}
                Err(e) => {
                    let _ = load_err.try_set(Some(e.text()));
                }
            }
        });
    };
    load();
    let users = Remote::<Vec<UserView>>::new("/api/v1/users");
    let go = move |_| {
        if !a.can_adopt() || a.busy.get_untracked() {
            return;
        }
        let Some(source) = a.source() else { return };
        let req = SyncthingAdoptRequest {
            source,
            volumes: a.volumes(),
            stop_source: a.stop_source.get_untracked(),
            apply_stignore: a.stignore.get_untracked(),
            stack: None,
            run_as: a.run_as.get_untracked(),
        };
        a.busy.set(true);
        a.error.set(None);
        let body = serde_json::to_string(&req).ok();
        spawn_local(async move {
            let r = api::start_task(ADOPT, body).await;
            let _ = a.busy.try_set(false);
            match r {
                Ok(id) => {
                    close.try_run(());
                    tasks().show(&id);
                    let end = tasks::follow(&id).await;
                    match end {
                        Ok(tk) if tk.status == rynas_api_types::TaskStatus::Ok => toast(Tone::Ok, t("stg.ad.done").to_string()),
                        Ok(tk) => toast(Tone::Err, tasks::task_error(&tk)),
                        Err(e) if e.status() == 401 => {}
                        Err(e) => toast(Tone::Err, e.text()),
                    }
                    done.try_run(());
                }
                Err(e) => {
                    let _ = a.error.try_set(Some(e.text()));
                }
            }
        });
    };
    let body = view! {
        <p class="dialog-text">{tr("stg.ad.text")}</p>
        {move || match (a.found.get(), load_err.get()) {
            (_, Some(e)) => ui::error_box_text(e, Callback::new(move |_| load())),
            (None, None) => ui::loading(Signal::stored(None), Callback::new(move |_| load()), 3),
            (Some(f), None) => sources(a, f.candidates),
        }}
        {move || a.path_mode.get().then(|| path_form(a))}
        {move || a.checking.get().then(|| ui::loading(Signal::stored(None), Callback::new(|_| {}), 4))}
        {move || a.preview.get().map(|p| preview_view(a, p, users))}
        {form_error(a.error)}
    }
    .into_any();
    let foot = save_bar_with(SaveBar {
        can_save: Signal::derive(move || a.can_adopt()),
        icon: Some(icons::IMPORT),
        ..SaveBar::new(a.busy, "stg.ad.go", close, Callback::new(go))
    });
    ui::dialog(DlgKind::WideDrawer, Signal::derive(|| t("stg.ad.title").to_string()), close, body, Some(foot))
}

/// The instances found, and "a folder copied from elsewhere".
fn sources(a: Adopt, cands: Vec<AdoptCandidate>) -> AnyView {
    let name = crate::form::fid();
    let rows = cands
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let n = name.clone();
            let on = move || a.chosen.get() == Some(i) && !a.path_mode.get();
            let kind = match c.source.kind {
                AdoptSourceKind::App => "stg.ad.k.app",
                AdoptSourceKind::Container => "stg.ad.k.container",
                AdoptSourceKind::Service => "stg.ad.k.service",
                AdoptSourceKind::Path => "stg.ad.k.path",
            };
            view! {
                <label class="check stg-source">
                    <input type="radio" name=n prop:checked=on on:change=move |_| a.pick(Some(i), false) />
                    <span>
                        <strong>{c.name.clone()}</strong>" "
                        {ui::chip(Tone::Info, ui::text(t(kind).to_string()))}
                        {c.running.then(|| ui::chip(Tone::Warn, ui::text(t("stg.ad.running").to_string())))}
                        {(!c.detail.is_empty()).then(|| view! { <span class="cell-sub mono">" "{c.detail.clone()}</span> })}
                        {c.home.clone().map(|h| view! { <span class="cell-sub">{ui::breakable(&h, "mono")}</span> })}
                    </span>
                </label>
            }
        })
        .collect_view();
    view! {
        <div class="field">
            <span class="field-label">{tr("stg.ad.source")}</span>
            <div class="stg-sources">
                {rows}
                <label class="check stg-source">
                    <input type="radio" name=name prop:checked=move || a.path_mode.get() on:change=move |_| a.pick(None, true) />
                    <span><strong>{tr("stg.ad.copied")}</strong><span class="cell-sub">{tr("stg.ad.copied_help")}</span></span>
                </label>
            </div>
        </div>
    }
    .into_any()
}

/// The home folder of a copied instance and its volume lines. Any change drops the preview:
/// the adoption must be checked again with what it will send.
fn path_form(a: Adopt) -> AnyView {
    let add = move |_| {
        let key = a.next_key.get_value();
        a.next_key.set_value(key + 1);
        a.lines.update(|l| l.push(Line { key, container: String::new(), host: String::new() }));
        a.preview.set(None);
    };
    let set = move |key: u32, host: bool, v: String| {
        a.preview.set(None);
        a.lines.update(|l| {
            if let Some(x) = l.iter_mut().find(|x| x.key == key) {
                if host {
                    x.host = v;
                } else {
                    x.container = v;
                }
            }
        })
    };
    view! {
        <div class="form">
            {text_field("users.home", a.path.into(), Callback::new(move |v: String| {
                a.path.set(v);
                a.preview.set(None);
            }), Signal::stored(None), In { mono: true, help: Some("stg.ad.home_help"), maxlength: 1024, ..In::default() })}
            <div class="field">
                <span class="field-label">{tr("stg.ad.volumes")}</span>
                <span class="field-help">{tr("stg.ad.volumes_help")}</span>
                <For each=move || a.lines.get() key=|l| l.key let:l>
                    {
                        let k = l.key;
                        view! {
                            <div class="stg-volume">
                                <input class="input mono" aria-label=tr("stg.ad.vol_container") placeholder="/var/syncthing/data" prop:value=l.container.clone()
                                    on:input=move |ev| set(k, false, event_target_value(&ev)) />
                                <span class="muted" aria-hidden="true">"→"</span>
                                <input class="input mono" aria-label=tr("stg.ad.vol_host") placeholder="/srv/volume/share" prop:value=l.host.clone()
                                    on:input=move |ev| set(k, true, event_target_value(&ev)) />
                                {ui::icon_btn(icons::TRASH_2, t("stg.ad.vol_remove").to_string(), true, Signal::stored(false), Callback::new(move |_| {
                                    a.lines.update(|v| v.retain(|x| x.key != k));
                                    a.preview.set(None);
                                }))}
                            </div>
                        }
                    }
                </For>
                <div class="sec-actions">
                    <button type="button" class="btn ghost sm" on:click=add><Icon icon=icons::PLUS size=14 />{tr("stg.ad.vol_add")}</button>
                </div>
            </div>
            <div class="sec-actions">
                <button type="button" class="btn ghost" disabled=move || a.checking.get() || a.path.with(|p| p.trim().is_empty()) on:click=move |_| a.check()>
                    {spinner(a.checking)}<Icon icon=icons::SEARCH size=16 />{tr("bk.adopt.check")}
                </button>
            </div>
        </div>
    }
    .into_any()
}

fn marker_chip(m: MarkerState) -> AnyView {
    let (tone, key) = match m {
        MarkerState::Verified => (Tone::Ok, "stg.mk.verified"),
        MarkerState::Unverified => (Tone::Warn, "stg.mk.unverified"),
        MarkerState::Missing => (Tone::Warn, "stg.mk.missing"),
        MarkerState::Unknown => (Tone::Unknown, "stg.mk.unknown"),
    };
    ui::chip(tone, ui::text(t(key).to_string()))
}

fn unmapped_key(r: UnmappedReason) -> &'static str {
    match r {
        UnmappedReason::NoVolume => "stg.um.no_volume",
        UnmappedReason::NoShare => "stg.um.no_share",
        UnmappedReason::BadPath => "stg.um.bad_path",
    }
}

fn folder_row(f: &AdoptFolder) -> AnyView {
    let target = match (&f.new_path, f.unmapped) {
        (Some(p), _) => ui::breakable(p, "mono"),
        (None, Some(r)) => ui::text(t(unmapped_key(r)).to_string()),
        (None, None) => ui::text(ui::DASH.to_string()),
    };
    ui::row(vec![
        ui::cell("stg.ad.col.folder", "", ui::two_lines(ui::text(f.label.clone()), Some(f.id.clone()), "cell-sub mono")),
        ui::cell("stg.ad.col.from", "wrap", ui::breakable(&f.path, "mono")),
        ui::cell("stg.ad.col.to", "wrap", target),
        ui::cell(
            "stg.ad.col.state",
            "",
            view! {
                <div class="chips">
                    {marker_chip(f.marker)}
                    {f.will_pause.then(|| ui::chip(Tone::Warn, ui::text(t("stg.ad.will_pause").to_string())))}
                </div>
            }
            .into_any(),
        ),
    ])
}

fn database_key(d: AdoptDatabase) -> &'static str {
    match d {
        AdoptDatabase::None => "stg.ad.db.none",
        AdoptDatabase::V2 => "stg.ad.db.v2",
        AdoptDatabase::V1 => "stg.ad.db.v1",
    }
}

/// What the adoption will do, and its options.
fn preview_view(a: Adopt, p: SyncthingAdoptPreview, users: Remote<Vec<UserView>>) -> AnyView {
    let problems = p
        .problems
        .iter()
        .map(|x| match x.strip_prefix(INFO) {
            Some(rest) => ui::note(Tone::Info, icons::INFO, ui::text(rest.to_string())),
            None => ui::note(Tone::Err, icons::TRIANGLE_ALERT, ui::text(x.clone())),
        })
        .collect_view();
    let running = p.source.as_ref().is_some_and(|s| s.running);
    let devices = if p.devices.is_empty() { ui::DASH.to_string() } else { p.devices.iter().map(|d| format!("{} ({})", d.name, d.id_short)).collect::<Vec<_>>().join(", ") };
    let files = &p.files;
    let id_files = files.cert && files.key && files.config;
    let rows = p.folders.iter().map(folder_row).collect::<Vec<_>>();
    let changes: Vec<(String, Vec<String>, Vec<String>)> =
        p.folders.iter().filter_map(|f| f.stignore.as_ref().map(|s| (f.label.clone(), s.removed.clone(), s.added.clone()))).collect();
    let stignore = (!changes.is_empty()).then(|| {
        let list = changes
            .into_iter()
            .map(|(label, removed, added)| {
                let lines = removed
                    .into_iter()
                    .map(|l| view! { <li class="diff-old mono">"− "{l}</li> }.into_any())
                    .chain(added.into_iter().map(|l| view! { <li class="diff-new mono">"+ "{l}</li> }.into_any()))
                    .collect_view();
                view! { <li><strong>{label}</strong><ul class="plain-list small stg-lines">{lines}</ul></li> }
            })
            .collect_view();
        view! {
            {switch("stg.ad.stignore", Some("stg.ad.stignore_help"), a.stignore.into(), Callback::new(move |v| a.stignore.set(v)))}
            {ui::fold(None, "stg.ad.stignore_changes", false, None, view! { <ul class="plain-list small stg-lines">{list}</ul> }.into_any())}
        }
    });
    let owners = p.owners.clone();
    let run_as = move || {
        let id = crate::form::fid();
        let names: Vec<String> = users.data.with(|u| u.as_ref().map(|u| u.iter().map(|x| x.user.name.clone()).collect()).unwrap_or_default());
        let mut names = names;
        if let Some(cur) = a.run_as.get()
            && !names.contains(&cur)
        {
            names.insert(0, cur);
        }
        let help = owners.as_ref().map(|o| {
            let who = o.user.clone().unwrap_or_else(|| tf("stg.ad.uid", &[("uid", &o.uid.to_string())]));
            tf("stg.ad.owners", &[("user", &who), ("n", &o.others.to_string())])
        });
        view! {
            <div class="field">
                <label class="field-label" for=id.clone()>{tr("sch.f.user")}</label>
                <select id=id class="input" disabled=move || a.checking.get() on:change=move |ev| {
                    let v = event_target_value(&ev);
                    a.run_as.set((!v.is_empty()).then_some(v));
                    a.check();
                }>
                    <option value="" selected=move || a.run_as.with(Option::is_none)>{tr("stg.ad.apps_account")}</option>
                    {names.into_iter().map(|n| { let (v, val) = (n.clone(), n.clone()); view! { <option value=val selected=move || a.run_as.with(|r| r.as_deref() == Some(v.as_str()))>{n}</option> } }).collect_view()}
                </select>
                <span class="field-help">{tr("stg.ad.run_as_help")}{help.map(|h| view! { " "{h} })}</span>
            </div>
        }
    };
    view! {
        <h3 class="sec-head">{tr("stg.ad.preview")}</h3>
        {problems}
        <dl class="kvs">
            {p.home.clone().map(|h| ui::kv("users.home", ui::breakable(&h, "mono")))}
            {ui::kv_text("stg.ad.identity", t(if id_files { "stg.ad.identity_ok" } else { "stg.ad.identity_missing" }).to_string(), "")}
            {ui::kv_text("stg.ad.database", t(database_key(files.database)).to_string(), "")}
            {ui::kv_text("stg.devices", devices, "")}
            {ui::kv_text("stg.ad.shares", if p.shares.is_empty() { ui::DASH.to_string() } else { p.shares.join(", ") }, "")}
        </dl>
        {if rows.is_empty() {
            ui::empty_line("stg.no_folders")
        } else {
            ui::table("", &["stg.ad.col.folder", "stg.ad.col.from", "stg.ad.col.to", "stg.ad.col.state"], rows)
        }}
        <div class="form">
            {run_as}
            {running.then(|| switch("stg.ad.stop", Some("stg.ad.stop_help"), a.stop_source.into(), Callback::new(move |v| a.stop_source.set(v))))}
            {stignore}
        </div>
    }
    .into_any()
}
