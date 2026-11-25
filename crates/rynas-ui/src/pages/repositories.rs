//! System › Software repositories (`/api/v1/repositories`, D-REPOS): the apt sources of the
//! system, their checks, the Debian (official) section, the entries grouped by file, adding
//! and removing third-party sources.
//!
//! - Every write sends back the digest it was read with (409 when the files changed) and
//!   runs as a rynasd task (`updates.repositories`), followed in a dialog; the page is read
//!   again when it ends.
//! - The Debian section and "Convert to the modern format" show what would change first
//!   (`preview: true`), then write on confirmation.
//! - Adding a source reads its key first: the user confirms the fingerprints shown.

use std::future::Future;
use std::pin::Pin;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::TaskStarted;
use rynas_api_types::repositories::*;

use crate::api::{self, ApiErr};
use crate::form::{form_error, spinner, switch, text_field, area_field, In};
use crate::i18n::{self, t, tf, tr, try_t};
use crate::icons::{self, Icon};
use crate::tasks::TaskRun;
use crate::ui::{self, cell, chip, status, text, Confirm, DlgKind, PageHeader, Remote, Tone, WidgetHead};

const URL: &str = "/api/v1/repositories";

/// A write that starts a task.
type Start = Pin<Box<dyn Future<Output = Result<TaskStarted, ApiErr>>>>;

/// `POST {URL}{path}` that starts a task.
fn post_task<B: serde::Serialize + 'static>(path: &'static str, body: Option<B>) -> Start {
    Box::pin(async move { api::post::<B, TaskStarted>(&format!("{URL}{path}"), body.as_ref()).await })
}

/// What a dialog asks.
#[derive(Clone)]
enum Ask {
    Toggle { path: String, index: usize, enabled: bool, what: String },
    Remove(String),
    /// The changes of a Debian section save or of the conversion, and how to confirm them.
    Preview { title: &'static str, changes: Vec<FileChange>, official: Option<OfficialRequest> },
}

/// The page's shared signals.
#[derive(Clone, Copy)]
struct Pg {
    st: Remote<RepositoriesState>,
    run: RwSignal<Option<(String, String)>>,
    ask: RwSignal<Option<Ask>>,
    adding: RwSignal<bool>,
    busy: RwSignal<bool>,
    err: RwSignal<Option<(Tone, String)>>,
}

impl Pg {
    fn digest(self) -> String {
        self.st.data.with_untracked(|s| s.as_ref().map(|s| s.digest.clone()).unwrap_or_default())
    }

    fn running(self) -> Signal<bool> {
        Signal::derive(move || self.busy.get() || self.st.data.with(|s| s.as_ref().is_some_and(|s| s.running_task.is_some())))
    }

    /// Starts `f`; its task is followed in the dialog. A refusal goes to `err` (or the page).
    fn launch(self, title: &'static str, f: Start, err: Option<RwSignal<Option<String>>>, on_ok: Callback<()>) {
        self.busy.set(true);
        spawn_local(async move {
            let r = f.await;
            let _ = self.busy.try_set(false);
            match r {
                Ok(t) => {
                    on_ok.run(());
                    self.run.set(Some((i18n::t(title).to_string(), t.task_id)));
                }
                Err(e) => {
                    match err {
                        Some(s) => {
                            let _ = s.try_set(Some(e.text()));
                        }
                        None => self.err.set(Some((Tone::Err, e.text()))),
                    }
                    if e.code() == rynas_api_types::error_codes::CONFLICT {
                        self.st.reload();
                    }
                }
            }
        });
    }
}

#[component]
pub fn Repositories() -> impl IntoView {
    let pg = Pg {
        st: Remote::<RepositoriesState>::new(URL),
        run: RwSignal::new(None),
        ask: RwSignal::new(None),
        adding: RwSignal::new(false),
        busy: RwSignal::new(false),
        err: RwSignal::new(None),
    };
    let check = move |_| pg.launch("task.updates.sources-check", post_task::<()>("/check", None), None, Callback::new(|_| ()));
    let running = pg.running();
    view! {
        <div class="page">
            <PageHeader icon=icons::DATABASE title="repo.title" subtitle="repo.subtitle">
                <a class="btn ghost" href="/system"><Icon icon=icons::ARROW_LEFT size=16 /><span class="btn-label">{tr("nav.system")}</span></a>
                <button class="btn primary" on:click=check disabled=move || running.get() || !pg.st.ready()>
                    {spinner(pg.busy)}<Icon icon=icons::REFRESH_CW size=16 /><span class="btn-label">{tr("repo.check")}</span>
                </button>
            </PageHeader>
            <ui::Notice msg=pg.err />
            {move || pg.st.data.with(|s| s.as_ref().map(|s| s.unsupported.clone())).and_then(|l| unsupported_note(l, false))}
            {pg.st.view(6, move || view! {
                <div class="grid">
                    {summary_card(pg)}
                    {debian_card(pg)}
                    {entries_card(pg)}
                </div>
            }.into_any())}
            {move || pg.ask.get().map(|a| ask_dialog(pg, a))}
            {move || pg.adding.get().then(|| add_dialog(pg))}
            {move || pg.run.get().map(|(title, id)| {
                let close = Callback::new(move |_| pg.run.set(None));
                let body = view! { <TaskRun id=id on_done=Callback::new(move |_| pg.st.reload()) /> }.into_any();
                ui::dialog(DlgKind::Wide, Signal::stored(title), close, body, Some(ui::close_foot(close)))
            })}
        </div>
    }
}

/// "Unsupported configuration" above a page (also System › Updates): nothing when supported.
pub fn unsupported_note(list: Vec<String>, link: bool) -> Option<AnyView> {
    (!list.is_empty()).then(|| {
        let msg = tf("repo.unsupported", &[("list", &list.join(", "))]);
        let link = link.then(|| view! { " "<a href="/system/repositories">{tr("repo.open")}</a> });
        ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { <strong>{tr("repo.unsupported_title")}</strong>" "{msg}{link} }.into_any())
    })
}

fn level_tone(l: RepoLevel) -> Tone {
    match l {
        RepoLevel::Info => Tone::Info,
        RepoLevel::Warning => Tone::Warn,
        RepoLevel::Critical => Tone::Err,
    }
}

/// The text of a finding: `repo.i.<code>` with its values, else the server's English.
fn info_text(i: &RepoInfo) -> String {
    match try_t(&format!("repo.i.{}", i.code)) {
        Some(s) => i.args.iter().fold(s.to_string(), |s, (k, v)| s.replace(&format!("{{{k}}}"), v)),
        None => i.message.clone(),
    }
}

fn fetch_text(e: &FetchError) -> String {
    let key = match e.kind {
        FetchErrorKind::NoPubkey => "repo.f.no_pubkey",
        FetchErrorKind::ExpiredKey => "repo.f.expired_key",
        FetchErrorKind::BadSignature => "repo.f.bad_signature",
        FetchErrorKind::NotSigned => "repo.f.not_signed",
        FetchErrorKind::ExpiredRelease => "repo.f.expired_release",
        FetchErrorKind::NotFound => "repo.f.not_found",
        FetchErrorKind::Network => "repo.f.network",
        FetchErrorKind::ReleaseInfoChanged => "repo.f.changed",
        FetchErrorKind::Duplicate | FetchErrorKind::Other => return e.message.clone(),
    };
    t(key).to_string()
}

// ------------------------------------------------------------------ summary

fn summary_card(pg: Pg) -> AnyView {
    let body = move || {
        let s = pg.st.data.get().unwrap_or_default();
        let (tone, key) = match s.status {
            RepoStatus::Good => (Tone::Ok, "repo.status_good"),
            RepoStatus::Warning => (Tone::Warn, "repo.status_warning"),
            RepoStatus::Critical => (Tone::Err, "repo.status_critical"),
        };
        let convert = !s.convertible.is_empty();
        let modernize = move |_| {
            let req = ModernizeRequest { digest: pg.digest(), preview: true };
            spawn_local(async move {
                match api::post::<_, RepoPreview>(&format!("{URL}/modernize"), Some(&req)).await {
                    Ok(p) => pg.ask.set(Some(Ask::Preview { title: "repo.convert_title", changes: p.changes, official: None })),
                    Err(e) => pg.err.set(Some((Tone::Err, e.text()))),
                }
            });
        };
        let running = pg.running();
        view! {
            <dl class="kvs">
                {ui::kv("repo.status", status(tone, t(key).to_string()))}
                {ui::kv_text("repo.release", s.codename.clone().map(|c| format!("Debian {c} · {}", s.arch)).unwrap_or_else(|| ui::DASH.into()), "")}
                {ui::kv_text("repo.last_check", ui::when(s.last_check.as_ref().map(|c| c.at_ms)), "tnum")}
            </dl>
            {(!s.infos.is_empty()).then(|| view! {
                <ul class="repo-infos">
                    {s.infos.iter().map(|i| {
                        let tone = level_tone(i.level);
                        let at = i.path.clone().map(|p| match i.index { Some(n) if i.code != "parse_error" => format!("{p} #{}", n + 1), _ => p });
                        view! {
                            <li class=format!("tone-text {}", tone.class())>
                                <Icon icon=tone.icon() size=14 class="inline" />
                                <span>{info_text(i)}{at.map(|a| view! { " "<span class="mono muted small">{a}</span> })}</span>
                            </li>
                        }
                    }).collect_view()}
                </ul>
            })}
            {convert.then(|| view! {
                <p class="muted small">{tr("repo.convert_help")}</p>
                <div class="card-actions">
                    <button type="button" class="btn ghost" disabled=move || running.get() on:click=modernize>
                        <Icon icon=icons::FILE_DIFF size=16 />{tr("repo.convert")}
                    </button>
                </div>
            })}
        }
        .into_any()
    };
    view! {
        <section class="card widget w-12">
            <WidgetHead icon=icons::SHIELD_CHECK title="repo.summary" />
            {body}
        </section>
    }
    .into_any()
}

// ------------------------------------------------------------------ Debian (official)

/// (value, label key, help key) of the optional components.
const COMPONENTS: &[(&str, &str, &str)] = &[
    ("contrib", "repo.c_contrib", "repo.c_contrib_help"),
    ("non-free", "repo.c_nonfree", "repo.c_nonfree_help"),
    ("non-free-firmware", "repo.c_firmware", "repo.c_firmware_help"),
];

fn debian_card(pg: Pg) -> AnyView {
    let body = move || {
        let s = pg.st.data.get().unwrap_or_default();
        let o = s.official.clone();
        if !o.manageable {
            let why = if o.reason.as_deref() == Some("system_layer") { "repo.deb_system_layer" } else { "repo.deb_not_debian" };
            return view! {
                {ui::note(Tone::Info, icons::INFO, text(t(why).to_string()))}
                {(o.reason.as_deref() == Some("system_layer")).then(|| view! {
                    <dl class="kvs">
                        {ui::kv_text("repo.mirror", o.mirror.clone(), "mono")}
                        {ui::kv_text("repo.components", o.components.join(" "), "mono")}
                    </dl>
                })}
            }
            .into_any();
        }
        official_form(pg, o)
    };
    view! {
        <section class="card widget w-12" id="debian">
            <WidgetHead icon=icons::BOX title="repo.debian" />
            {body}
        </section>
    }
    .into_any()
}

fn official_form(pg: Pg, o: OfficialState) -> AnyView {
    let security = RwSignal::new(o.security);
    let updates = RwSignal::new(o.updates);
    let backports = RwSignal::new(o.backports);
    let proposed = RwSignal::new(o.proposed);
    let sources = RwSignal::new(o.sources);
    let comps = RwSignal::new(o.components.clone());
    let mirror = RwSignal::new(o.mirror.clone());
    let error = RwSignal::new(None::<String>);
    let tests = RwSignal::new(None::<Vec<MirrorResult>>);
    let testing = RwSignal::new(false);
    let sw = |label: &'static str, help: &'static str, v: RwSignal<bool>| switch(label, Some(help), v.into(), Callback::new(move |x| v.set(x)));
    let comp_switch = move |(c, label, help): &'static (&'static str, &'static str, &'static str)| {
        let on = Signal::derive(move || comps.with(|l| l.iter().any(|x| x == c)));
        let set = Callback::new(move |v: bool| {
            comps.update(|l| {
                l.retain(|x| x != c);
                if v {
                    l.push(c.to_string());
                    // non-free also brings the firmware (both split out since Debian 12)
                    if *c == "non-free" && !l.iter().any(|x| x == "non-free-firmware") {
                        l.push("non-free-firmware".into());
                    }
                }
            })
        });
        switch(label, Some(help), on, set)
    };
    let req = move |preview: bool| {
        let mut c: Vec<String> = vec!["main".into()];
        c.extend(comps.get_untracked().into_iter().filter(|x| x != "main"));
        OfficialRequest {
            digest: pg.digest(),
            mirror: mirror.get_untracked().trim().to_string(),
            security: security.get_untracked(),
            updates: updates.get_untracked(),
            backports: backports.get_untracked(),
            proposed: proposed.get_untracked(),
            components: c,
            sources: sources.get_untracked(),
            preview,
        }
    };
    let save = move |_| {
        error.set(None);
        let r = req(true);
        spawn_local(async move {
            match api::put::<_, RepoPreview>(&format!("{URL}/official"), &r).await {
                Ok(p) if p.changes.is_empty() => {
                    let _ = error.try_set(Some(t("repo.no_change").to_string()));
                }
                Ok(p) => pg.ask.set(Some(Ask::Preview { title: "repo.deb_preview", changes: p.changes, official: Some(OfficialRequest { preview: false, ..r }) })),
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    };
    let test = move |_| {
        testing.set(true);
        let m = mirror.get_untracked();
        let req = MirrorTestRequest { mirrors: words(&m) };
        spawn_local(async move {
            let r = api::post::<_, MirrorTest>(&format!("{URL}/mirror-test"), Some(&req)).await;
            let _ = testing.try_set(false);
            match r {
                Ok(t) => {
                    let _ = tests.try_set(Some(t.results));
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    };
    let running = pg.running();
    let mirrors = o.mirrors.clone();
    view! {
        <p class="muted small">{tr("repo.deb_help")}</p>
        <fieldset class="sf-group" disabled=move || running.get()>
            <legend class="field-label">{tr("repo.suites")}</legend>
            <dl class="kvs">{ui::kv_text("repo.s_main", t("repo.s_main_help").to_string(), "")}</dl>
            {sw("repo.s_security", "repo.s_security_help", security)}
            {sw("repo.s_updates", "repo.s_updates_help", updates)}
            {sw("repo.s_backports", "repo.s_backports_help", backports)}
            {o.backports_by_system.then(|| view! { <p class="muted small">{tr("repo.bp_system")}</p> })}
            {sw("repo.s_proposed", "repo.s_proposed_help", proposed)}
            {move || proposed.get().then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, text(t("repo.proposed_warn").to_string())))}
        </fieldset>
        <fieldset class="sf-group" disabled=move || running.get()>
            <legend class="field-label">{tr("repo.components")}</legend>
            <p class="muted small">{tr("repo.c_main_help")}</p>
            {COMPONENTS.iter().map(comp_switch).collect_view()}
            {sw("repo.s_sources", "repo.s_sources_help", sources)}
        </fieldset>
        <div class="field">
            <label class="field-label" for="repo-mirror">{tr("repo.mirror")}</label>
            <input id="repo-mirror" class="input mono" list="repo-mirrors" spellcheck="false" autocomplete="off" maxlength="256"
                prop:value=move || mirror.get() on:input=move |ev| mirror.set(event_target_value(&ev)) />
            <datalist id="repo-mirrors">{mirrors.into_iter().map(|m| view! { <option value=m></option> }).collect_view()}</datalist>
            <span class="field-help">{tr("repo.mirror_help")}</span>
        </div>
        {move || tests.get().map(|rs| view! {
            <ul class="repo-infos">
                {rs.into_iter().map(|r| {
                    let uri = r.uri.clone();
                    view! {
                        <li>
                            <span class="mono">{r.uri.clone()}</span>" · "
                            {match (r.ms, r.error) {
                                (Some(ms), _) => view! { <span class="tnum">{tf("repo.ms", &[("n", &i18n::num(ms as f64, 0))])}</span> }.into_any(),
                                (None, e) => view! { <span class="tone-text err">{e.unwrap_or_default()}</span> }.into_any(),
                            }}
                            {r.ms.is_some().then(|| view! { " "<button type="button" class="btn ghost sm" on:click=move |_| mirror.set(uri.clone())>{tr("repo.use")}</button> })}
                        </li>
                    }
                }).collect_view()}
            </ul>
        })}
        {form_error(error)}
        <div class="card-actions">
            <button type="button" class="btn ghost" disabled=move || testing.get() on:click=test>
                {spinner(testing)}<Icon icon=icons::TIMER size=16 />{tr("repo.test_mirrors")}
            </button>
            <button type="button" class="btn primary" disabled=move || running.get() on:click=save>{tr("repo.deb_save")}</button>
        </div>
    }
    .into_any()
}

// ------------------------------------------------------------------ entries

fn kind_chip(e: &RepoEntry) -> AnyView {
    let (tone, key) = match e.kind {
        RepoKind::Debian => (Tone::Ok, "repo.k_debian"),
        RepoKind::Rynas => (Tone::Info, "repo.k_rynas"),
        RepoKind::SystemLayer => (Tone::Info, "repo.k_system"),
        RepoKind::ThirdParty => (Tone::Warn, "repo.k_third"),
    };
    chip(tone, text(t(key).to_string()))
}

fn read_only_text(e: &RepoEntry) -> Option<AnyView> {
    let key = match e.read_only? {
        ReadOnly::Rynas => "repo.ro_rynas",
        ReadOnly::SystemLayer => "repo.ro_system",
        ReadOnly::DebianOnSystemLayer => "repo.ro_debian_system",
    };
    Some(view! {
        <span class="muted small">{tr(key)}{e.manage_url.clone().map(|u| view! { " "<a href=u>{tr("repo.manage")}</a> })}</span>
    }.into_any())
}

fn entry_row(pg: Pg, path: String, e: RepoEntry) -> AnyView {
    let running = pg.running();
    let ro = e.read_only.is_some();
    let what = format!("{} {}", e.uris.join(" "), e.suites.join(" "));
    let (p2, idx, on) = (path.clone(), e.index, e.enabled);
    let label = ui::verb_name(if on { "repo.disable" } else { "repo.enable" }, &what);
    let toggle = view! {
        <input type="checkbox" role="switch" class="switch" aria-label=label.clone() title=label prop:checked=on
            disabled=move || ro || running.get()
            on:change=move |ev| {
                // the switch follows the server, not the click
                event_target::<web_sys::HtmlInputElement>(&ev).set_checked(on);
                pg.ask.set(Some(Ask::Toggle { path: p2.clone(), index: idx, enabled: !on, what: what.clone() }));
            } />
    }
    .into_any();
    let state = {
        let errs: Vec<AnyView> = e.errors.iter().map(|x| view! { <div class=format!("small tone-text {}", if x.error { "err" } else { "warn" })>{fetch_text(x)}</div> }.into_any()).collect();
        let fetched = e.enabled.then(|| e.last_fetch_ms.map(|m| tf("repo.fetched", &[("when", &i18n::datetime(m))])).unwrap_or_else(|| t("repo.never_fetched").to_string()));
        view! { {errs}{fetched.map(|f| view! { <div class="small muted">{f}</div> })}{read_only_text(&e)} }.into_any()
    };
    let cls = if e.enabled { "" } else { "muted-row" };
    ui::row_class(
        vec![
            cell("repo.col_on", "", toggle),
            ui::text_cell("repo.col_type", "mono", e.types.join(" ")),
            cell("repo.col_uris", "", e.uris.iter().map(|u| ui::breakable(u, "mono")).collect_view().into_any()),
            ui::text_cell("repo.col_suites", "mono", e.suites.join(" ")),
            ui::text_cell("repo.col_components", "mono", e.components.join(" ")),
            cell("repo.col_origin", "", view! { {kind_chip(&e)}{e.origin.clone().map(|o| view! { " "<span class="small">{o}</span> })} }.into_any()),
            cell("repo.col_state", "", state),
        ],
        cls,
    )
}

fn entries_card(pg: Pg) -> AnyView {
    let body = move || {
        let s = pg.st.data.get().unwrap_or_default();
        let running = pg.running();
        let files = s.files.into_iter().map(|f| {
            let name = f.path.rsplit('/').next().unwrap_or("").to_string();
            let remove = f.removable.then(|| {
                let n = name.clone();
                ui::icon_btn(icons::TRASH_2, ui::verb_name("repo.remove", &name), true, running, Callback::new(move |_| pg.ask.set(Some(Ask::Remove(n.clone())))))
            });
            let fmt = if f.format == RepoFormat::Deb822 { "deb822" } else { "one-line" };
            let rows: Vec<AnyView> = f.entries.iter().cloned().map(|e| entry_row(pg, f.path.clone(), e)).collect();
            view! {
                <div class="repo-file">
                    <h3 class="repo-file-head">
                        <span class="mono">{f.path.clone()}</span>
                        {chip(Tone::Unknown, text(fmt.into()))}
                        {f.package.clone().map(|p| chip(Tone::Unknown, text(tf("repo.package", &[("name", &p)]))))}
                        {remove}
                    </h3>
                    {f.error.clone().map(|e| ui::note(Tone::Err, icons::CIRCLE_X, text(tf("repo.i.parse_error", &[("error", &e)]))))}
                    {(!rows.is_empty()).then(|| ui::table("repo-table", &["repo.col_on", "repo.col_type", "repo.col_uris", "repo.col_suites", "repo.col_components", "repo.col_origin", "repo.col_state"], rows))}
                </div>
            }
        }).collect_view();
        let ignored = (!s.ignored.is_empty()).then(|| view! {
            <h3 class="small">{tr("repo.ignored")}</h3>
            <ul class="repo-infos">
                {s.ignored.into_iter().map(|i| view! { <li><span class="mono">{i.path}</span>" · "<span class="muted small">{t(match i.reason.as_str() { "backup" => "repo.ign_backup", "name" => "repo.ign_name", _ => "repo.ign_extension" })}</span></li> }).collect_view()}
            </ul>
        });
        view! { {files}{ignored} }.into_any()
    };
    let running = pg.running();
    view! {
        <section class="card widget w-12">
            <WidgetHead icon=icons::LAYERS title="repo.entries">
                <button type="button" class="btn ghost sm" disabled=move || running.get() on:click=move |_| pg.adding.set(true)>
                    <Icon icon=icons::PLUS size=16 />{tr("repo.add")}
                </button>
            </WidgetHead>
            {body}
        </section>
    }
    .into_any()
}

// ------------------------------------------------------------------ dialogs

fn ask_dialog(pg: Pg, a: Ask) -> AnyView {
    let cancel = Callback::new(move |_| pg.ask.set(None));
    let close_ok = Callback::new(move |_| pg.ask.set(None));
    match a {
        Ask::Toggle { path, index, enabled, what } => {
            let c = Confirm::new(
                t(if enabled { "repo.enable_title" } else { "repo.disable_title" }).to_string(),
                tf("repo.toggle_text", &[("what", &what)]),
                if enabled { "repo.enable" } else { "repo.disable" },
                !enabled,
                Callback::new(|_| ()),
                cancel,
            );
            let err = c.error;
            let req = ToggleRequest { digest: pg.digest(), path, index, enabled };
            let c = Confirm {
                on_confirm: Callback::new(move |_| pg.launch("task.updates.repositories", post_task("/entries/toggle", Some(req.clone())), Some(err), close_ok)),
                busy: pg.busy,
                ..c
            };
            ui::confirm(c)
        }
        Ask::Remove(name) => {
            let c = Confirm::new(tf("dk.rm.title", &[("name", &name)]), t("repo.remove_text").to_string(), "repo.remove", true, Callback::new(|_| ()), cancel);
            let err = c.error;
            let url = format!("{URL}/third-party/{}?digest={}", api::encode(&name), pg.digest());
            let c = Confirm {
                on_confirm: Callback::new(move |_| pg.launch("task.updates.repositories", {
                    let url = url.clone();
                    Box::pin(async move { api::delete::<TaskStarted>(&url).await })
                }, Some(err), close_ok)),
                busy: pg.busy,
                ..c
            };
            ui::confirm(c)
        }
        Ask::Preview { title, changes, official } => {
            let err = RwSignal::new(None::<String>);
            let go = Callback::new(move |_| {
                let f: Start = match &official {
                    Some(r) => {
                        let r = r.clone();
                        Box::pin(async move { api::put::<_, TaskStarted>(&format!("{URL}/official"), &r).await })
                    }
                    None => post_task("/modernize", Some(ModernizeRequest { digest: pg.digest(), preview: false })),
                };
                pg.launch("task.updates.repositories", f, Some(err), close_ok);
            });
            let body = view! {
                <p class="dialog-text">{tr("repo.preview_text")}</p>
                {changes.into_iter().map(|c| {
                    let what = match c.action.as_str() {
                        "rename" => tf("repo.ch_rename", &[("to", c.rename_to.as_deref().unwrap_or(""))]),
                        "remove" => t("repo.ch_remove").to_string(),
                        "edit" => t("repo.ch_edit").to_string(),
                        _ => t("repo.ch_write").to_string(),
                    };
                    view! {
                        <h4 class="small"><span class="mono">{c.path}</span>" · "{what}</h4>
                        {(!c.after.is_empty()).then(|| view! { <pre class="app-code">{c.after}</pre> })}
                    }
                }).collect_view()}
                {form_error(err)}
            }
            .into_any();
            let foot = view! {
                <button type="button" class="btn ghost" disabled=move || pg.busy.get() on:click=move |_| cancel.run(())>{tr("cancel")}</button>
                <button type="button" class="btn primary" disabled=move || pg.busy.get() on:click=move |_| go.run(())>{spinner(pg.busy)}{tr("repo.apply")}</button>
            }
            .into_any();
            ui::dialog(DlgKind::Wide, Signal::derive(move || t(title).to_string()), cancel, body, Some(foot))
        }
    }
}

fn words(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

fn add_dialog(pg: Pg) -> AnyView {
    let close = Callback::new(move |_| pg.adding.set(false));
    let name = RwSignal::new(String::new());
    let uris = RwSignal::new(String::new());
    let suites = RwSignal::new(String::new());
    let comps = RwSignal::new("main".to_string());
    let archs = RwSignal::new(String::new());
    let src = RwSignal::new(false);
    let key_text = RwSignal::new(String::new());
    let key_url = RwSignal::new(String::new());
    let preview = RwSignal::new(None::<KeyPreview>);
    let confirmed = RwSignal::new(false);
    let reading = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let none = Signal::stored(None::<String>);
    let set = |s: RwSignal<String>| Callback::new(move |v: String| s.set(v));
    let http = Signal::derive(move || uris.with(|u| u.split_whitespace().any(|x| x.starts_with("http://"))));
    let read_key = move |_| {
        reading.set(true);
        error.set(None);
        confirmed.set(false);
        let (a, u) = (key_text.get_untracked(), key_url.get_untracked());
        let req = KeyPreviewRequest { armored: (!a.trim().is_empty()).then_some(a), url: (!u.trim().is_empty()).then_some(u) };
        spawn_local(async move {
            let r = api::post::<_, KeyPreview>(&format!("{URL}/key-preview"), Some(&req)).await;
            let _ = reading.try_set(false);
            match r {
                Ok(p) => {
                    let _ = preview.try_set(Some(p));
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                    let _ = preview.try_set(None);
                }
            }
        });
    };
    let can = Signal::derive(move || confirmed.get() && preview.with(Option::is_some) && !name.with(String::is_empty) && !uris.with(String::is_empty) && !suites.with(String::is_empty));
    let add = Callback::new(move |_| {
        let Some(p) = preview.get_untracked() else { return };
        let req = AddRepoRequest {
            digest: pg.digest(),
            name: name.get_untracked().trim().to_string(),
            uris: words(&uris.get_untracked()),
            suites: words(&suites.get_untracked()),
            components: words(&comps.get_untracked()),
            architectures: words(&archs.get_untracked()),
            sources: src.get_untracked(),
            key: p.armored.clone(),
            fingerprints: p.keys.iter().map(|k| k.fingerprint.clone()).collect(),
        };
        pg.launch("task.updates.repositories", post_task("/third-party", Some(req)), Some(error), close);
    });
    let body = view! {
        {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, text(t("repo.add_warn").to_string()))}
        {text_field("repo.f_name", name.into(), set(name), none, In { help: Some("repo.f_name_help"), mono: true, maxlength: 63, ..In::default() })}
        {text_field("repo.f_uris", uris.into(), set(uris), none, In { help: Some("repo.f_uris_help"), mono: true, ..In::default() })}
        {move || http.get().then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, text(t("repo.http_warn").to_string())))}
        {text_field("repo.f_suites", suites.into(), set(suites), none, In { help: Some("repo.f_suites_help"), mono: true, ..In::default() })}
        {text_field("repo.f_components", comps.into(), set(comps), none, In { help: Some("repo.f_components_help"), mono: true, ..In::default() })}
        {text_field("repo.f_archs", archs.into(), set(archs), none, In { help: Some("repo.f_archs_help"), mono: true, ..In::default() })}
        {switch("repo.s_sources", None, src.into(), Callback::new(move |v| src.set(v)))}
        {area_field("repo.f_key", key_text.into(), set(key_text), Some("repo.f_key_help"), "-----BEGIN PGP PUBLIC KEY BLOCK-----")}
        {text_field("repo.f_key_url", key_url.into(), set(key_url), none, In { help: Some("repo.f_key_url_help"), mono: true, ..In::default() })}
        <div class="card-actions">
            <button type="button" class="btn ghost" disabled=move || reading.get() on:click=read_key>{spinner(reading)}<Icon icon=icons::KEY size=16 />{tr("repo.read_key")}</button>
        </div>
        {move || preview.get().map(|p| view! {
            <ul class="repo-infos">
                {p.keys.into_iter().map(|k| view! {
                    <li>
                        <div class="mono strong">{k.fingerprint.clone()}</div>
                        <div class="small">{k.uids.join(", ")}</div>
                        <div class=if k.expired { "small tone-text err" } else { "small muted" }>
                            {match (k.expires_ms, k.expired) {
                                (Some(e), true) => tf("repo.key_expired", &[("when", &i18n::datetime(e))]),
                                (Some(e), false) => tf("repo.key_expires", &[("when", &i18n::datetime(e))]),
                                (None, _) => t("repo.key_no_expiry").to_string(),
                            }}
                        </div>
                    </li>
                }).collect_view()}
            </ul>
            {switch("repo.key_confirm", Some("repo.key_confirm_help"), confirmed.into(), Callback::new(move |v| confirmed.set(v)))}
        })}
        {form_error(error)}
    }
    .into_any();
    let foot = crate::form::save_bar_with(crate::form::SaveBar { can_save: can, icon: Some(icons::PLUS), ..crate::form::SaveBar::new(pg.busy, "repo.add", close, add) });
    ui::dialog(DlgKind::Drawer, Signal::derive(|| t("repo.add").to_string()), close, body, Some(foot))
}
