//! Apps & Plugins (D-APPCAT, wave A2 of `docs/phase2/docker-apps.md`): the curated app
//! catalogue (`/api/v1/apps/...`, §5.2) and the modules rynas can add, on one page with the
//! sections Featured · Apps · Plugins · Installed · Updates.
//!
//! - Without Docker the Docker module card comes first: no app installs before the engine
//!   (the catalogue and the details stay readable).
//! - An install is a wizard (Options from `form.json` through [`crate::schema_form`], Folders &
//!   shares, Network, Review) whose answers the server plans before anything happens; the
//!   install itself is a task, and the secrets the entry shows come back once at its end.
//! - An installed app is an ordinary stack: update with a plan and a snapshot, rollback,
//!   "Move behind the proxy", convert to a custom stack, uninstall keeping the data.
//! - Lists are read again on `docker.changed` and when an `apps.*` or `docker.*` task ends.

mod catalog;
mod installed;
mod wizard;

use leptos::prelude::*;
use rynas_api_types::apps::{AppAntifeature, AppCard, AppCatalog, AppCategory, InstalledApp, RamGate};
use rynas_api_types::web_access::ProxyMode;
use serde::de::DeserializeOwned;

use crate::docker::docker;
use crate::form::tabs;
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::ui::{self, EmptyState, PageHeader, Remote, Tone};

const TABS: &[&str] = &["apps.tab.featured", "apps.tab.apps", "apps.tab.plugins", "apps.tab.installed", "apps.tab.updates"];

/// The fields of `GET /api/v1/web-access` (`WebAccessState`) this page reads: the whole
/// state's deserializer is not worth its wasm here.
#[derive(serde::Deserialize)]
struct WebAccess {
    applied: WebAccessApplied,
    engine: WebAccessEngine,
    #[serde(default)]
    host: String,
}

#[derive(serde::Deserialize)]
struct WebAccessApplied {
    #[serde(default)]
    settings: WebAccessMode,
}

#[derive(serde::Deserialize, Default)]
struct WebAccessMode {
    #[serde(default)]
    mode: ProxyMode,
}

#[derive(serde::Deserialize)]
struct WebAccessEngine {
    installed: bool,
}

/// `GET /api/v1/apps/catalog/{id}` (`rynas_api_types::apps::AppDetail`, same fields) with the
/// install form kept as JSON for [`crate::schema_form`]: no typed copy of the schema's
/// deserializer in the wasm.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub(super) struct AppDetail {
    pub card: AppCard,
    pub description: String,
    pub homepage: String,
    pub source: Option<String>,
    pub changelog_url: String,
    pub maintainers: Vec<String>,
    pub host_network: bool,
    pub form: crate::schema_form::Form,
    pub web: Option<rynas_api_types::apps::AppWeb>,
    pub folders: Vec<rynas_api_types::apps::AppFolder>,
    pub shares: Vec<rynas_api_types::apps::AppShare>,
    pub secrets: Vec<rynas_api_types::apps::AppSecret>,
    pub hardware: rynas_api_types::apps::AppHardware,
    pub overlays: Vec<rynas_api_types::apps::AppOverlay>,
    pub named_volumes: Vec<rynas_api_types::apps::AppNamedVolume>,
    pub notes: Vec<rynas_api_types::apps::AppNote>,
    pub screenshots: Vec<String>,
}

/// What the sections share: the catalogue, the installed apps, web access, and which drawer
/// or wizard is open.
#[derive(Clone, Copy)]
pub(super) struct Apps {
    pub cat: Remote<AppCatalog>,
    pub inst: Remote<Vec<InstalledApp>>,
    /// Web access is active (rynas-caddy installed, built-in mode).
    pub proxy: RwSignal<bool>,
    /// The web access state has been read (or found absent): `proxy` is final, the wizard
    /// can count its steps.
    pub proxy_known: RwSignal<bool>,
    /// Web access runs in external proxy mode (or rynasd has none): 80/443 are free, so a
    /// reverse proxy app (`AppCard::reverse_proxy`) may be installed.
    pub external: RwSignal<bool>,
    /// The host label of web access names (`<label>-<host>.local`).
    pub proxy_host: RwSignal<String>,
    /// Entry id of the open detail drawer.
    pub detail: RwSignal<Option<String>>,
    /// Entry id of the open install wizard.
    pub wizard: RwSignal<Option<String>>,
    /// Stack of the open installed-app drawer.
    pub manage: RwSignal<Option<String>>,
    pub tab: RwSignal<usize>,
}

impl Apps {
    /// Docker is installed and has a volume: installs are possible.
    pub fn can_install(self) -> bool {
        self.cat.data.with(|c| c.as_ref().is_some_and(|c| c.docker_installed && c.stacks_dir.is_some()))
    }

    pub fn reload(self) {
        self.cat.reload();
        self.inst.reload();
    }
}

fn apps() -> Apps {
    expect_context::<Apps>()
}

/// A list read again (debounced) after `trigger`: [`crate::docker::Docker::generation`] for
/// what containers change, [`crate::docker::Docker::ended`] for what only tasks change.
fn remote<T: DeserializeOwned + Send + Sync + 'static>(url: &str, trigger: RwSignal<u64>) -> Remote<T> {
    let r = Remote::<T>::new(url);
    crate::docker::follow(trigger, Signal::stored(true), Callback::new(move |_| r.reload()));
    r
}

#[component]
pub fn AppsPage() -> impl IntoView {
    let d = docker();
    d.refresh();
    // `/plugins` (older links) opens the Plugins section
    let plugins_route = leptos_router::hooks::use_location().pathname.with_untracked(|p| p.starts_with("/plugins"));
    let a = Apps {
        // the catalogue's installed/update marks change only through a task
        cat: remote::<AppCatalog>("/api/v1/apps/catalog", d.ended),
        inst: remote::<Vec<InstalledApp>>("/api/v1/apps/installed", d.generation),
        proxy: RwSignal::new(false),
        proxy_known: RwSignal::new(false),
        proxy_host: RwSignal::new(String::new()),
        external: RwSignal::new(false),
        detail: RwSignal::new(None),
        wizard: RwSignal::new(None),
        manage: RwSignal::new(None),
        tab: RwSignal::new(if plugins_route { 2 } else { 0 }),
    };
    provide_context(a);
    // web access state only decides defaults and "Move behind the proxy": rynasd without web
    // access answers 503 (inactive); any other failure is shown
    let proxy_err = RwSignal::new(None::<String>);
    leptos::task::spawn_local(async move {
        let r = crate::api::get::<WebAccess>("/api/v1/web-access").await;
        match r {
            Ok(w) => {
                let _ = a.proxy.try_set(w.engine.installed && w.applied.settings.mode == ProxyMode::Builtin);
                let _ = a.proxy_host.try_set(w.host);
                let _ = a.external.try_set(w.applied.settings.mode == ProxyMode::External);
            }
            Err(e) if e.status() == 503 => {
                let _ = a.external.try_set(true);
            }
            Err(e) if e.status() == 401 => {}
            Err(e) => {
                let _ = proxy_err.try_set(Some(e.text()));
            }
        }
        let _ = a.proxy_known.try_set(true);
    });
    let no_docker = Memo::new(move |_| d.status.with(|s| s.as_ref().is_some_and(|s| !s.installed && s.task_id.is_none())));
    let tab = a.tab;
    let seen = RwSignal::new([!plugins_route, false, plugins_route, false, false]);
    Effect::new(move || {
        let i = tab.get();
        if !seen.with_untracked(|s| s[i]) {
            seen.update(|s| s[i] = true);
        }
    });
    let panel = move |i: usize, body: fn() -> AnyView| {
        let mounted = Memo::new(move |_| seen.with(|s| s[i]));
        view! {
            <div class="hw-panel" role="tabpanel" id=format!("apt-p{i}") aria-labelledby=format!("apt-t{i}") hidden=move || tab.get() != i>
                {move || mounted.get().then(body)}
            </div>
        }
    };
    view! {
        <div class="page">
            <PageHeader icon=icons::PUZZLE title="nav.apps" subtitle="apps.subtitle" />
            {move || proxy_err.get().map(|e| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(tf("apps.proxy_err", &[("err", &e)]))))}
            <Show when=move || no_docker.get()>
                {ui::note_class(Tone::Info, icons::INFO, "apps-first-note", ui::text(t("apps.docker_first").to_string()))}
                <div class="grid apps-first">
                    {super::docker::install_card()}
                </div>
            </Show>
            <div class="card hw-tabs">{tabs("apt", TABS, tab, "nav.apps")}</div>
            {panel(0, catalog::featured)}
            {panel(1, catalog::browse)}
            {panel(2, plugins)}
            {panel(3, installed::panel)}
            {panel(4, updates_tab)}
            {move || a.detail.get().map(catalog::detail_drawer)}
            {move || a.wizard.get().map(wizard::wizard)}
            {move || a.manage.get().map(installed::drawer)}
        </div>
    }
}

/// Updates: the installed apps with updates, then where the catalogue comes from.
fn updates_tab() -> AnyView {
    view! {
        {installed::updates()}
        {catalog::source_panel()}
    }
    .into_any()
}

/// Plugins: the Docker module (install, then its page) and the system plugins to come.
fn plugins() -> AnyView {
    let d = docker();
    // None: unknown yet; Some(false): missing (its card is at the top of the page)
    let present = Memo::new(move |_| d.status.with(|s| s.as_ref().map(|s| s.installed || s.task_id.is_some())));
    view! {
        <div class="grid">
            {move || match present.get() {
                Some(true) => super::docker::install_card(),
                Some(false) => view! { <div class="card w-12">{ui::note(Tone::Info, icons::CONTAINER, ui::text(t("apps.plg.docker_above").to_string()))}</div> }.into_any(),
                None => view! { <div class="card w-12">{ui::loading(d.status_err.into(), Callback::new(move |_| d.refresh()), 4)}</div> }.into_any(),
            }}
            <div class="card soon w-12">
                <EmptyState icon=icons::PUZZLE title=Signal::derive(move || t("soon").to_string()) text=Signal::derive(move || t("soon.plugins").to_string())>
                    <span class="chip accent2"><Icon icon=icons::SPARKLES size=14 />{tr("soon")}</span>
                </EmptyState>
            </div>
        </div>
    }
    .into_any()
}

// ------------------------------------------------------------------ texts of closed sets

pub(super) fn category_key(c: AppCategory) -> &'static str {
    use AppCategory as C;
    match c {
        C::Management => "apps.cat.management",
        C::Media => "apps.cat.media",
        C::Photos => "apps.cat.photos",
        C::Files => "apps.cat.files",
        C::Documents => "apps.cat.documents",
        C::Productivity => "apps.cat.productivity",
        C::Security => "apps.cat.security",
        C::HomeAutomation => "apps.cat.home_automation",
        C::Surveillance => "apps.cat.surveillance",
        C::Monitoring => "apps.cat.monitoring",
        C::Network => "apps.cat.network",
        C::Sync => "apps.cat.sync",
        C::Development => "apps.cat.development",
        C::Backup => "apps.cat.backup",
        C::Ai => "apps.cat.ai",
    }
}

pub(super) const CATEGORIES: [AppCategory; 15] = [
    AppCategory::Management,
    AppCategory::Media,
    AppCategory::Photos,
    AppCategory::Files,
    AppCategory::Documents,
    AppCategory::Productivity,
    AppCategory::Security,
    AppCategory::HomeAutomation,
    AppCategory::Surveillance,
    AppCategory::Monitoring,
    AppCategory::Network,
    AppCategory::Sync,
    AppCategory::Development,
    AppCategory::Backup,
    AppCategory::Ai,
];

/// Name and explanation of an antifeature.
pub(super) fn antifeature_keys(a: AppAntifeature) -> (&'static str, &'static str) {
    use AppAntifeature as A;
    match a {
        A::NonFree => ("apps.af.non_free", "apps.af.non_free_text"),
        A::HighResources => ("apps.af.high_resources", "apps.af.high_resources_text"),
        A::NeedsGpu => ("apps.af.needs_gpu", "apps.af.needs_gpu_text"),
        A::Telemetry => ("apps.af.telemetry", "apps.af.telemetry_text"),
        A::Unmaintained => ("apps.af.unmaintained", "apps.af.unmaintained_text"),
        A::RequiresHttps => ("apps.af.requires_https", "apps.af.requires_https_text"),
        A::RootAccess => ("apps.af.root_access", "apps.af.root_access_text"),
    }
}

fn mb(n: u64) -> String {
    i18n::bytes(n * 1024 * 1024)
}

/// The RAM gate as a status pill.
pub(super) fn ram_pill(g: RamGate) -> AnyView {
    let (tone, text) = match g {
        RamGate::Ok => (Tone::Ok, t("apps.ram.ok").to_string()),
        RamGate::BelowRecommended { installed_mb, recommended_mb } => (Tone::Warn, tf("apps.ram.warn", &[("need", &mb(recommended_mb.into())), ("have", &mb(installed_mb))])),
        RamGate::BelowMinimum { installed_mb, minimum_mb } => (Tone::Err, tf("apps.ram.blocked", &[("need", &mb(minimum_mb.into())), ("have", &mb(installed_mb))])),
        RamGate::Unknown => (Tone::Unknown, t("apps.ram.unknown").to_string()),
    };
    ui::status(tone, text)
}

/// Antifeature chips: the name, the explanation as tooltip and accessible description.
pub(super) fn antifeature_chips(list: &[AppAntifeature]) -> AnyView {
    list.iter()
        .map(|a| {
            let (k, e) = antifeature_keys(*a);
            view! {
                <span class="chip warn-chip" title=tr(e) aria-label=move || format!("{}: {}", t(k), t(e))>
                    <Icon icon=icons::TRIANGLE_ALERT size=12 />{tr(k)}
                </span>
            }
        })
        .collect_view()
        .into_any()
}

/// Why this entry cannot be installed now (an i18n key), `None` when it can.
pub(super) fn install_block(c: &AppCard, cat: &AppCatalog) -> Option<&'static str> {
    if !cat.docker_installed {
        Some("apps.block.docker")
    } else if cat.stacks_dir.is_none() {
        Some("apps.block.volume")
    } else if !c.arch_ok {
        Some("apps.block.arch")
    } else if !c.rynas_ok {
        Some("apps.block.rynas")
    } else if matches!(c.ram, RamGate::BelowMinimum { .. }) {
        Some("apps.block.ram")
    } else if c.reverse_proxy && !apps().external.get() {
        Some("apps.block.reverse_proxy")
    } else {
        None
    }
}

/// The entry's icon (served by the API, SVG).
pub(super) fn app_icon(src: String, size: u32) -> AnyView {
    view! { <img class="app-icon" src=src alt="" width=size height=size loading="lazy" decoding="async" /> }.into_any()
}

/// `http(s)://<this host>:<port><path>`: the scheme is the app's own (`https` from its
/// entry's `web.scheme`), never the one of rynas' page — an app that speaks plain HTTP does
/// not answer `https://`.
pub(super) fn host_url(https: bool, port: u16, path: &str) -> String {
    let host = window().location().hostname().unwrap_or_default();
    let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host };
    let path = if path.starts_with('/') { path.to_string() } else { format!("/{path}") };
    format!("{}://{host}:{port}{path}", if https { "https" } else { "http" })
}

/// The address "Move behind the proxy" gives a stack (`https://<label>-<host>.local/`, the
/// label rule of §5.3; the server adds `-2`… when a label is taken).
pub(super) fn proxy_url(stack: &str, host: &str) -> Option<String> {
    if host.is_empty() {
        return None;
    }
    let label: String = stack.to_lowercase().chars().map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' { c } else { '-' }).take(40).collect();
    let name = format!("{}-{host}", label.trim_matches('-'));
    (name.len() <= 63).then(|| format!("https://{name}.local/"))
}

/// Where an installed app's web page answers: its web access route, else the published port
/// when it is on the LAN.
pub(super) fn open_href(a: &InstalledApp) -> Option<String> {
    let lan = a.bind.as_deref() != Some("127.0.0.1");
    match (&a.web_url, a.web_port) {
        (Some(u), _) if !lan => Some(u.clone()),
        (_, Some(p)) if lan => Some(host_url(a.web_https, p, a.web_path.as_deref().unwrap_or("/"))),
        (Some(u), _) => Some(u.clone()),
        _ => None,
    }
}
