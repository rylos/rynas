//! Featured and Apps: the catalogue cards (search, category filter, badges) and the detail
//! drawer of one entry (description, screenshots, ports, folders, shares, web, resources).

use leptos::prelude::*;
use rynas_api_types::apps::{AppCard, AppCatalog, FolderIntent, FolderSsd, GpuNeed, NoteLevel, RamGate, ShareAccess, WebAuth, WebPublic};

use super::{AppDetail, CATEGORIES, antifeature_chips, antifeature_keys, app_icon, apps, category_key, install_block, ram_pill};
use crate::form::search_box;
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::ui::{self, EmptyState, Remote, Tone};

/// The catalogue's featured picks (`featured`) that are not installed yet, those that fit
/// this NAS first.
fn recommended(c: &AppCatalog) -> Vec<AppCard> {
    let mut v: Vec<AppCard> = c.apps.iter().filter(|a| a.featured && a.installed.is_empty() && !a.manager).cloned().collect();
    let fits = |a: &AppCard| a.arch_ok && a.rynas_ok && matches!(a.ram, RamGate::Ok | RamGate::Unknown);
    v.sort_by(|a, b| fits(b).cmp(&fits(a)).then_with(|| a.name.cmp(&b.name)));
    v
}

fn grid(cards: Vec<AppCard>) -> AnyView {
    view! { <div class="app-grid">{cards.into_iter().map(card).collect_view()}</div> }.into_any()
}

/// Featured: updates waiting, apps that fit this NAS, the Docker managers.
pub fn featured() -> AnyView {
    let a = apps();
    let content = move || {
        let Some(c) = a.cat.data.get() else { return ().into_any() };
        let updates = c.apps.iter().filter(|x| x.update_available).count();
        let rec = recommended(&c);
        let managers: Vec<AppCard> = c.apps.iter().filter(|x| x.manager).cloned().collect();
        if c.apps.is_empty() {
            return empty_catalog(&c);
        }
        view! {
            {(updates > 0).then(|| ui::note(Tone::Info, icons::DOWNLOAD, view! {
                {tf("apps.updates_waiting", &[("n", &updates.to_string())])}" "
                <button type="button" class="link-btn" on:click=move |_| a.tab.set(4)>{tr("apps.see_updates")}</button>
            }.into_any()))}
            <section class="apps-sec">
                <h3 class="apps-sec-title">{tr("apps.feat.fit")}</h3>
                <p class="muted small">{ram_line(&c)}</p>
                {if rec.is_empty() { view! { <p class="muted small">{tr("apps.feat.none")}</p> }.into_any() } else { grid(rec) }}
            </section>
            {(!managers.is_empty()).then(|| view! {
                <section class="apps-sec">
                    <h3 class="apps-sec-title">{tr("apps.feat.managers")}</h3>
                    <p class="muted small">{tr("apps.feat.managers_text")}</p>
                    {grid(managers)}
                </section>
            })}
            <div class="sec-actions">
                <button type="button" class="btn ghost" on:click=move |_| a.tab.set(1)>{tr("apps.browse_all")}<Icon icon=icons::CHEVRON_RIGHT size=16 /></button>
            </div>
        }
        .into_any()
    };
    view! { <section class="card widget w-12">{move || a.cat.view(8, content)}</section> }.into_any()
}

fn ram_line(c: &AppCatalog) -> String {
    match c.installed_ram_mb {
        Some(m) => tf("apps.feat.ram", &[("ram", &i18n::bytes(m * 1024 * 1024)), ("arch", &c.arch)]),
        None => tf("apps.feat.arch", &[("arch", &c.arch)]),
    }
}

fn empty_catalog(c: &AppCatalog) -> AnyView {
    view! {
        <EmptyState icon=icons::BOX title=Signal::derive(|| t("apps.empty").to_string()) text=Signal::derive(|| t("apps.empty_text").to_string()) />
        {issues(c)}
    }
    .into_any()
}

/// Entries the catalogue refused to load (never installable).
fn issues(c: &AppCatalog) -> Option<AnyView> {
    (!c.issues.is_empty()).then(|| {
        let rows = c.issues.iter().map(|i| view! { <li><strong>{i.app.clone()}</strong>" · "<span class="mono">{i.file.clone()}</span>" — "<span lang="en">{i.message.clone()}</span></li> }).collect_view();
        view! {
            <details class="apps-issues">
                <summary>{tf("apps.issues", &[("n", &c.issues.len().to_string())])}</summary>
                <ul class="small">{rows}</ul>
            </details>
        }
        .into_any()
    })
}

/// Apps: every card, searchable and filtered by category.
pub fn browse() -> AnyView {
    let a = apps();
    let q = RwSignal::new(String::new());
    let cat = RwSignal::new(String::new());
    let content = move || {
        let Some(c) = a.cat.data.get() else { return ().into_any() };
        if c.apps.is_empty() {
            return empty_catalog(&c);
        }
        let qv = q.get().trim().to_lowercase();
        let cv = cat.get();
        let shown: Vec<AppCard> = c
            .apps
            .iter()
            .filter(|x| qv.is_empty() || x.name.to_lowercase().contains(&qv) || x.summary.to_lowercase().contains(&qv) || x.id.contains(&qv))
            .filter(|x| cv.is_empty() || x.categories.iter().any(|k| category_key(*k) == cv))
            .cloned()
            .collect();
        let n = shown.len();
        view! {
            <p class="small muted" aria-live="polite">{tf("apps.count", &[("n", &n.to_string())])}</p>
            {if n == 0 {
                view! { <EmptyState icon=icons::SEARCH title=Signal::derive(|| t("apps.no_match").to_string()) text=Signal::derive(|| t("apps.no_match_text").to_string()) /> }.into_any()
            } else {
                grid(shown)
            }}
            {issues(&c)}
        }
        .into_any()
    };
    // categories that occur in the catalogue
    let cats = move || {
        let used: Vec<_> = a.cat.data.with(|c| c.as_ref().map(|c| CATEGORIES.iter().copied().filter(|k| c.apps.iter().any(|x| x.categories.contains(k))).collect()).unwrap_or_default());
        used.into_iter().map(|k| { let key = category_key(k); view! { <option value=key selected=move || cat.with(|c| c == key)>{tr(key)}</option> } }).collect_view()
    };
    view! {
        <section class="card widget w-12">
            <div class="toolbar">
                {search_box("apps.search", q)}
                <select class="input dk-filter" aria-label=tr("apps.category") on:change=move |ev| cat.set(event_target_value(&ev))>
                    <option value="" selected=move || cat.with(String::is_empty)>{tr("apps.cat.all")}</option>
                    {cats}
                </select>
            </div>
            {move || a.cat.view(8, content)}
        </section>
    }
    .into_any()
}

/// One catalogue card.
fn card(c: AppCard) -> AnyView {
    let a = apps();
    let (id, id2) = (c.id.clone(), c.id.clone());
    let installed = c.installed.first().cloned();
    let block = a.cat.data.with_untracked(|cat| cat.as_ref().and_then(|cat| install_block(&c, cat)));
    let details_label = format!("{}: {}", t("apps.details"), c.name);
    let primary = match installed {
        Some(stack) => view! {
            <button type="button" class="btn ghost sm" on:click=move |_| a.manage.set(Some(stack.clone()))><Icon icon=icons::SETTINGS_2 size=14 />{tr("apps.manage")}</button>
        }
        .into_any(),
        None => view! {
            <button type="button" class="btn primary sm" disabled=block.is_some() title=block.map(tr) on:click=move |_| a.wizard.set(Some(id2.clone()))>
                <Icon icon=icons::DOWNLOAD size=14 />{tr("apps.install")}
            </button>
        }
        .into_any(),
    };
    view! {
        <article class="card app-card">
            <header class="app-card-head">
                {app_icon(c.icon.clone(), 40)}
                <div class="app-card-title">
                    <h3 lang="en">{c.name.clone()}</h3>
                    <span class="cell-sub">{tf("apps.version", &[("v", &c.app_version)])}" · "<span class="mono">{c.license.clone()}</span></span>
                </div>
            </header>
            <p class="app-sum" lang="en">{c.summary.clone()}</p>
            <div class="chips">
                {(!c.installed.is_empty()).then(|| ui::status(Tone::Ok, t("apps.installed").to_string()))}
                {c.update_available.then(|| view! { <span class="chip accent"><Icon icon=icons::DOWNLOAD size=12 />{tr("dk.upd.available")}</span> })}
                {ram_pill(c.ram)}
                {(!c.arch_ok).then(|| ui::status(Tone::Err, t("apps.block.arch").to_string()))}
                {(!c.rynas_ok).then(|| ui::status(Tone::Err, t("apps.block.rynas").to_string()))}
                {antifeature_chips(&c.antifeatures)}
            </div>
            <div class="app-card-foot">
                <button type="button" class="btn ghost sm" aria-label=details_label on:click=move |_| a.detail.set(Some(id.clone()))>{tr("apps.details")}</button>
                {primary}
            </div>
        </article>
    }
    .into_any()
}

// ------------------------------------------------------------------ detail

fn intent_key(i: FolderIntent) -> &'static str {
    match i {
        FolderIntent::Config => "apps.intent.config",
        FolderIntent::Data => "apps.intent.data",
        FolderIntent::Db => "apps.intent.db",
        FolderIntent::Media => "apps.intent.media",
        FolderIntent::Cache => "apps.intent.cache",
    }
}

pub(super) fn ssd_key(s: FolderSsd) -> Option<&'static str> {
    match s {
        FolderSsd::No => None,
        FolderSsd::Ok => Some("apps.ssd.ok"),
        FolderSsd::Preferred => Some("apps.ssd.preferred"),
    }
}

pub(super) fn intent_text(i: FolderIntent) -> &'static str {
    t(intent_key(i))
}

fn auth_key(w: WebAuth) -> &'static str {
    match w {
        WebAuth::App => "apps.web.auth_app",
        WebAuth::None => "apps.web.auth_none",
        WebAuth::Proxy => "apps.web.auth_proxy",
    }
}

fn public_key(p: WebPublic) -> &'static str {
    match p {
        WebPublic::Ok => "apps.web.public_ok",
        WebPublic::Warn => "apps.web.public_warn",
        WebPublic::Never => "apps.web.public_never",
    }
}

fn yes_no(b: bool) -> String {
    t(if b { "yes" } else { "no" }).to_string()
}

pub(super) fn notes_view(notes: &[rynas_api_types::apps::AppNote]) -> AnyView {
    notes
        .iter()
        .map(|n| {
            let (tone, icon) = if n.level == NoteLevel::Warning { (Tone::Warn, icons::TRIANGLE_ALERT) } else { (Tone::Info, icons::INFO) };
            ui::note(tone, icon, view! { <span lang="en">{n.text.clone()}</span> }.into_any())
        })
        .collect_view()
        .into_any()
}

fn kv(label: &'static str, value: String, class: &'static str) -> AnyView {
    ui::kv_row(label, Signal::stored(value), class)
}

fn ports_table(c: &AppCard) -> Option<AnyView> {
    let rows = c
        .ports
        .iter()
        .map(|p| {
            ui::row(vec![
                ui::cell("apps.col.port", "", ui::two_lines(
                    view! { <span class="tnum">{format!("{}/{}", p.default, if p.protocol == rynas_api_types::apps::PortProtocol::Udp { "udp" } else { "tcp" })}</span> }.into_any(),
                    Some(p.env.clone()),
                    "cell-sub mono",
                )),
                ui::cell("apps.col.purpose", "wrap", view! { <span lang="en">{p.purpose.clone()}</span> }.into_any()),
                ui::cell("apps.col.reach", "", view! {
                    <div class="chips">
                        {p.proxied.then(|| view! { <span class="chip">{tr("apps.port.proxied")}</span> })}
                        {(!p.relocatable).then(|| view! { <span class="chip">{tr("apps.port.fixed")}</span> })}
                    </div>
                }.into_any()),
            ])
        })
        .collect::<Vec<_>>();
    (!rows.is_empty()).then(|| view! { <h4 class="mini-title">{tr("apps.d.ports")}</h4>{ui::table("", &["apps.col.port", "apps.col.purpose", "apps.col.reach"], rows)} }.into_any())
}

fn folders_table(d: &AppDetail) -> Option<AnyView> {
    let rows = d
        .folders
        .iter()
        .map(|f| {
            ui::row(vec![
                ui::cell("apps.col.folder", "", view! { <div class="cell-main"><span class="mono">{format!("./{}", f.path)}</span><span class="cell-sub" lang="en">{f.description.clone()}</span></div> }.into_any()),
                ui::cell("apps.col.kind", "", view! {
                    <div class="chips">
                        <span class="chip">{intent_text(f.intent)}</span>
                        {ssd_key(f.ssd).map(|k| view! { <span class="chip accent">{tr(k)}</span> })}
                        {f.nocow.then(|| view! { <span class="chip">{tr("shares.nocow")}</span> })}
                    </div>
                }.into_any()),
            ])
        })
        .collect::<Vec<_>>();
    (!rows.is_empty()).then(|| {
        view! {
            <h4 class="mini-title">{tr("apps.d.folders")}</h4>
            <p class="small muted">{tr("apps.d.folders_text")}</p>
            {ui::table("", &["apps.col.folder", "apps.col.kind"], rows)}
        }
        .into_any()
    })
}

fn shares_table(d: &AppDetail) -> Option<AnyView> {
    let rows = d
        .shares
        .iter()
        .map(|s| {
            ui::row(vec![
                ui::cell("shares.folder", "", view! { <div class="cell-main"><span lang="en">{s.label.clone()}</span><span class="cell-sub" lang="en">{s.description.clone()}</span></div> }.into_any()),
                ui::cell("apps.col.access", "", view! {
                    <div class="chips">
                        <span class="chip">{tr(if s.access == ShareAccess::Rw { "rm.set.rw" } else { "perm.ro" })}</span>
                        {s.required.then(|| view! { <span class="chip">{tr("apps.share.required")}</span> })}
                    </div>
                }.into_any()),
            ])
        })
        .collect::<Vec<_>>();
    (!rows.is_empty()).then(|| view! { <h4 class="mini-title">{tr("apps.d.shares")}</h4>{ui::table("", &["shares.folder", "apps.col.access"], rows)} }.into_any())
}

fn web_view(d: &AppDetail) -> Option<AnyView> {
    let w = d.web.as_ref()?;
    let port = d.card.ports.iter().find(|p| p.name == w.port).map(|p| p.default.to_string()).unwrap_or_else(|| w.port.clone());
    let mut feats: Vec<&'static str> = Vec::new();
    if w.websocket {
        feats.push("apps.web.websocket");
    }
    if w.subpath {
        feats.push("apps.web.subpath");
    }
    if !w.buffering {
        feats.push("apps.web.streaming");
    }
    Some(
        view! {
            <h4 class="mini-title">{tr("nt.kind.ui")}</h4>
            <dl class="kvs">
                {kv("apps.web.port", format!("{port} · {}", w.path), "mono")}
                {kv("apps.web.auth", t(auth_key(w.auth)).to_string(), "")}
                {kv("apps.web.public", t(public_key(w.public)).to_string(), "")}
                {w.body.clone().map(|b| kv("apps.web.body", b, "mono"))}
                {w.timeout.clone().map(|b| kv("apps.web.timeout", b, "mono"))}
            </dl>
            {(!feats.is_empty()).then(|| view! { <div class="chips">{feats.into_iter().map(|k| view! { <span class="chip">{tr(k)}</span> }).collect_view()}</div> })}
        }
        .into_any(),
    )
}

/// A titled list of `name — description` lines (`lang="en"` descriptions from the entry).
fn name_list(title: &'static str, items: Vec<(String, String, bool)>) -> Option<AnyView> {
    (!items.is_empty()).then(|| {
        view! {
            <h4 class="mini-title">{tr(title)}</h4>
            <ul class="small app-list">{items.into_iter().map(|(n, d, shown)| view! { <li><span class="mono">{n}</span>" — "<span lang="en">{d}</span>{shown.then(|| view! { " "<span class="chip">{tr("apps.secret.shown")}</span> })}</li> }).collect_view()}</ul>
        }
        .into_any()
    })
}

fn resources(d: &AppDetail) -> AnyView {
    let c = &d.card;
    let gpu = match d.hardware.gpu {
        GpuNeed::None => None,
        GpuNeed::Optional => Some("apps.gpu.optional"),
        GpuNeed::Required => Some("apps.gpu.required"),
    };
    view! {
        <h4 class="mini-title">{tr("apps.d.resources")}</h4>
        <dl class="kvs">
            {kv("apps.ram.min", i18n::bytes(u64::from(c.ram_min_mb) * 1024 * 1024), "tnum")}
            {kv("apps.ram.recommended", i18n::bytes(u64::from(c.ram_recommended_mb) * 1024 * 1024), "tnum")}
            {gpu.map(|k| kv("apps.gpu", t(k).to_string(), ""))}
            {(!d.hardware.cpu_features.is_empty()).then(|| kv("apps.cpu", d.hardware.cpu_features.join(", "), "mono"))}
            {d.host_network.then(|| kv("apps.network", t("apps.host_network").to_string(), ""))}
        </dl>
    }
    .into_any()
}

fn detail_head(d: &AppDetail) -> AnyView {
    let c = &d.card;
    let link = |href: String, key: &'static str| view! { <a class="chip" href=href target="_blank" rel="noopener noreferrer"><Icon icon=icons::EXTERNAL_LINK size=12 />{tr(key)}</a> }.into_any();
    view! {
        <header class="app-card-head">
            {app_icon(c.icon.clone(), 56)}
            <div class="app-card-title">
                <h3 lang="en">{c.name.clone()}</h3>
                <span class="cell-sub">{tf("apps.version_pkg", &[("v", &c.app_version), ("pkg", &c.version)])}</span>
            </div>
        </header>
        <div class="chips">
            <span class="chip mono" title=tr("apps.license")>{c.license.clone()}</span>
            {c.categories.iter().map(|k| view! { <span class="chip">{tr(category_key(*k))}</span> }).collect_view()}
            {ram_pill(c.ram)}
            {(!c.installed.is_empty()).then(|| ui::status(Tone::Ok, t("apps.installed").to_string()))}
            {antifeature_chips(&c.antifeatures)}
        </div>
        <div class="chips">
            {link(d.homepage.clone(), "apps.homepage")}
            {d.source.clone().map(|s| link(s, "apps.source"))}
            {link(d.changelog_url.clone(), "apps.changelog")}
        </div>
    }
    .into_any()
}

fn detail_body(d: AppDetail) -> AnyView {
    let c = &d.card;
    let afs = c
        .antifeatures
        .iter()
        .map(|x| {
            let (k, e) = antifeature_keys(*x);
            view! { <li><strong>{tr(k)}</strong>" — "{tr(e)}</li> }
        })
        .collect_view();
    let has_af = !c.antifeatures.is_empty();
    // only the catalogue's own files (served by the API): nothing loads from another site
    let shots: Vec<String> = d.screenshots.iter().filter(|s| s.starts_with("/api/")).cloned().collect();
    let secrets = d.secrets.iter().map(|s| (s.env.clone(), s.description.clone(), s.show)).collect();
    let volumes = d.named_volumes.iter().map(|v| (v.name.clone(), v.description.clone(), false)).collect();
    view! {
        <div class="app-detail">
            {detail_head(&d)}
            {crate::markdown::render(&d.description, "en")}
            {(!shots.is_empty()).then(|| view! {
                <div class="app-shots">{shots.into_iter().map(|s| view! { <a href=s.clone() target="_blank" rel="noopener"><img src=s.clone() alt=tr("apps.screenshot") loading="lazy" /></a> }).collect_view()}</div>
            })}
            {notes_view(&d.notes)}
            {has_af.then(|| view! { <h4 class="mini-title">{tr("apps.d.antifeatures")}</h4><ul class="small app-list">{afs}</ul> })}
            {resources(&d)}
            {ports_table(c)}
            {web_view(&d)}
            {folders_table(&d)}
            {shares_table(&d)}
            {name_list("apps.d.secrets", secrets)}
            {name_list("apps.d.volumes", volumes)}
            {(!d.maintainers.is_empty()).then(|| view! { <dl class="kvs">{kv("apps.maintainers", d.maintainers.join(", "), "")}{kv("apps.d.manager", yes_no(c.manager), "")}</dl> })}
        </div>
    }
    .into_any()
}

/// The detail drawer of one entry.
pub fn detail_drawer(id: String) -> AnyView {
    let a = apps();
    let d = Remote::<AppDetail>::new(format!("/api/v1/apps/catalog/{}", crate::api::encode(&id)));
    let close = Callback::new(move |_| a.detail.set(None));
    let title = Signal::derive(move || d.data.with(|d| d.as_ref().map(|d| d.card.name.clone())).unwrap_or_else(|| t("apps.details").to_string()));
    let body = view! { {move || d.view(10, move || d.data.get().map(detail_body).unwrap_or_else(|| ().into_any()))} }.into_any();
    let id2 = StoredValue::new(id);
    let block = Memo::new(move |_| d.data.with(|d| a.cat.data.with(|c| match (d, c) {
        (Some(d), Some(c)) => install_block(&d.card, c),
        _ => Some("loading"),
    })));
    let foot = view! {
        {move || block.get().filter(|k| *k != "loading").map(|k| view! {
            <span class="small muted foot-note">{tr(k)}{(k == "apps.block.reverse_proxy").then(|| view! { " "<a href="/network/web-access">{tr("apps.block.to_mode")}</a> })}</span>
        })}
        <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("close")}</button>
        <button type="button" class="btn primary" disabled=move || block.get().is_some()
            on:click=move |_| { a.detail.set(None); a.wizard.set(Some(id2.get_value())); }>
            <Icon icon=icons::DOWNLOAD size=16 />{tr("apps.install")}
        </button>
    }
    .into_any();
    ui::drawer(title, close, body, foot)
}
