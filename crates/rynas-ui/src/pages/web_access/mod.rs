//! Network › Web access (wave W1 of `docs/phase2/docker-apps.md` §5.3, D-PROXY / D-EXPOSE):
//! the built-in reverse proxy in front of the interface and the apps, its routes with a
//! diagnostics checklist, and the HTTPS certificate with "Trust this NAS" ([`cert`]).
//!
//! - Overview: the engine (installed, running, in sync, last load), the mode with its
//!   consequences, the interface's own names and who may reach them, local names, and the
//!   emergency address that never goes through the proxy.
//! - Routes: one row per app address (names, access badge, problems); a drawer per route with
//!   its access level and the checklist of `POST /web-access/routes/{id}/test`.
//! - A change of the interface's own route waits for a confirmation from the new address
//!   (`confirm` in the state): a banner with the countdown, the address to open and, when the
//!   page is already on it, the Confirm button. Without it the previous value comes back.
//!
//! The configuration travels as JSON (`serde_json::Value`): the page edits the few fields it
//! shows and sends the rest back untouched, with no typed copy of the whole key in the wasm.

pub mod cert;

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::web_access::{AccessLevel, CheckState, PendingConfirm, ProxyEngine, RouteDiagnostics, RouteView};
use serde_json::Value;

use crate::api;
use crate::config::{apply_now, keys};
use crate::form::{form_error, spinner, switch, tabs};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::toast::saved_applied;
use crate::ui::{self, Confirm, EmptyState, PageHeader, Remote, Tone, WidgetHead};

const URL: &str = "/api/v1/web-access";
const TABS: &[&str] = &["wa.tab.overview", "wa.tab.routes", "tls.title"];

/// `GET /api/v1/web-access` (`WebAccessState`) with the configuration kept as JSON.
#[derive(Clone, PartialEq, serde::Deserialize)]
pub struct WaState {
    pub applied: Value,
    #[serde(default)]
    pub pending: Option<Value>,
    pub engine: ProxyEngine,
    pub host: String,
    pub ui_names: Vec<String>,
    pub ui_urls: Vec<String>,
    pub routes: Vec<RouteView>,
    pub lan: Vec<String>,
    pub vpn: Vec<String>,
    #[serde(default)]
    pub confirm: Option<PendingConfirm>,
    pub emergency_port: u16,
}

impl WaState {
    /// The value the next apply would write: the staged one, else the applied one.
    fn current(&self) -> Value {
        self.pending.clone().unwrap_or_else(|| self.applied.clone())
    }
}

/// A string at a JSON pointer, `def` when absent.
fn s_at(v: &Value, ptr: &str, def: &str) -> String {
    v.pointer(ptr).and_then(Value::as_str).unwrap_or(def).to_string()
}

fn b_at(v: &Value, ptr: &str, def: bool) -> bool {
    v.pointer(ptr).and_then(Value::as_bool).unwrap_or(def)
}

/// Sets `obj[key] = val`, making `obj` an object when it is not one.
fn put(obj: &mut Value, key: &str, val: Value) {
    if !obj.is_object() {
        *obj = Value::Object(Default::default());
    }
    if let Some(m) = obj.as_object_mut() {
        m.insert(key.to_string(), val);
    }
}

pub fn access_key(a: AccessLevel) -> &'static str {
    match a {
        AccessLevel::Lan => "wa.access.lan",
        AccessLevel::LanVpn => "wa.access.lan_vpn",
        AccessLevel::Public => "wa.access.public",
    }
}

fn access_str(a: AccessLevel) -> &'static str {
    match a {
        AccessLevel::Lan => "lan",
        AccessLevel::LanVpn => "lan_vpn",
        AccessLevel::Public => "public",
    }
}

fn access_of(s: &str) -> AccessLevel {
    match s {
        "lan_vpn" => AccessLevel::LanVpn,
        "public" => AccessLevel::Public,
        _ => AccessLevel::Lan,
    }
}

fn problem_key(p: &str) -> Option<&'static str> {
    Some(match p {
        "name_too_long" => "wa.prob.name_too_long",
        "local_names_off" => "wa.prob.local_names_off",
        "disabled" => "wa.prob.disabled",
        "name_collision" => "wa.prob.name_collision",
        "name_unpublished" => "wa.prob.name_unpublished",
        _ => return None,
    })
}

fn problem_text(p: &str) -> String {
    problem_key(p).map(|k| t(k).to_string()).unwrap_or_else(|| p.to_string())
}

fn kind_key(k: &str) -> &'static str {
    match k {
        "stack" => "wa.kind.stack",
        "custom" => "wa.kind.custom",
        _ => "wa.kind.app",
    }
}

/// What the page's parts share.
#[derive(Clone, Copy)]
struct Wa {
    state: Remote<WaState>,
    /// Route id of the open drawer, and whether its test runs at once.
    route: RwSignal<Option<(String, bool)>>,
    /// Server time minus local time (the confirmation countdown).
    skew: RwSignal<f64>,
    now: RwSignal<f64>,
}

fn wa() -> Wa {
    expect_context::<Wa>()
}

#[component]
pub fn WebAccess() -> impl IntoView {
    let state = Remote::<WaState>::with_config(URL);
    let w = Wa { state, route: RwSignal::new(None), skew: RwSignal::new(0.0), now: RwSignal::new(js_sys::Date::now()) };
    provide_context(w);
    // `?tab=certificate` (the System page's link) opens the certificate
    let deep = leptos_router::hooks::use_query_map().with_untracked(|q| q.get("tab").map(|v| v == "certificate").unwrap_or(false));
    let tab = RwSignal::new(if deep { 2usize } else { 0 });
    let seen = RwSignal::new([!deep, false, deep]);
    Effect::new(move || {
        let i = tab.get();
        if !seen.with_untracked(|s| s[i]) {
            seen.update(|s| s[i] = true);
        }
    });
    Effect::new(move || {
        if state.data.with(Option::is_some)
            && let Some(d) = api::last_server_date()
        {
            w.skew.set(d - js_sys::Date::now());
        }
    });
    // the countdown ticks only while a confirmation waits
    if let Ok(h) = set_interval_with_handle(
        move || {
            if state.data.with_untracked(|s| s.as_ref().is_some_and(|s| s.confirm.is_some())) {
                w.now.set(js_sys::Date::now());
            }
        },
        Duration::from_secs(1),
    ) {
        on_cleanup(move || h.clear());
    }
    let panel = move |i: usize, body: fn() -> AnyView| {
        let mounted = Memo::new(move |_| seen.with(|s| s[i]));
        view! {
            <div class="hw-panel" role="tabpanel" id=format!("wat-p{i}") aria-labelledby=format!("wat-t{i}") hidden=move || tab.get() != i>
                {move || mounted.get().then(body)}
            </div>
        }
    };
    view! {
        <div class="page">
            <PageHeader icon=icons::SHIELD_CHECK title="wa.title" subtitle="wa.subtitle">
                <a class="btn ghost" href="/network"><Icon icon=icons::ARROW_LEFT size=16 /><span class="btn-label">{tr("nav.network")}</span></a>
            </PageHeader>
            {confirm_banner}
            <div class="card hw-tabs">{tabs("wat", TABS, tab, "wa.title")}</div>
            {panel(0, overview)}
            {panel(1, routes_panel)}
            {panel(2, cert::panel)}
            {move || w.route.get().map(|(id, run)| route_drawer(id, run))}
        </div>
    }
}

// ------------------------------------------------------------------ confirmation

/// The timed confirmation of a change of the interface's own route.
fn confirm_banner() -> AnyView {
    let w = wa();
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let pending = Memo::new(move |_| w.state.data.with(|s| s.as_ref().and_then(|s| s.confirm.clone())));
    view! {
        {move || pending.get().map(|c| {
            let deadline = c.deadline_ms as f64;
            let left = Signal::derive(move || ((deadline - w.now.get() - w.skew.get()) / 1000.0).ceil().max(0.0) as u64);
            let first = c.names.first().cloned().unwrap_or_default();
            let href = format!("https://{first}/network/web-access");
            let here = window().location().hostname().unwrap_or_default();
            let on_new = c.names.iter().any(|n| n.eq_ignore_ascii_case(&here));
            let go = move |_| {
                busy.set(true);
                error.set(None);
                spawn_local(async move {
                    let r = api::post_empty::<()>("/api/v1/web-access/confirm", None).await;
                    busy.set(false);
                    match r {
                        Ok(()) => {
                            crate::toast::toast(Tone::Ok, t("wa.confirm.done").to_string());
                            w.state.reload();
                        }
                        Err(e) => error.set(Some(e.text())),
                    }
                });
            };
            // the deadline passed: rynasd restores the previous value, read it again
            Effect::new(move |done: Option<bool>| {
                if left.get() == 0 && done != Some(true) {
                    set_timeout(move || w.state.reload(), Duration::from_secs(3));
                    return true;
                }
                done.unwrap_or(false)
            });
            view! {
                <section class="card widget w-12 wa-confirm" role="alert">
                    <WidgetHead icon=icons::TIMER title="wa.confirm.title" />
                    <p>{move || tf("wa.confirm.text", &[("s", &left.get().to_string())])}</p>
                    <p class="small muted">{move || tf("wa.confirm.rollback", &[("port", &w.state.data.with(|s| s.as_ref().map(|s| s.emergency_port).unwrap_or(8443)).to_string())])}</p>
                    {form_error(error)}
                    <div class="sec-actions">
                        {if on_new {
                            view! { <button class="btn primary" disabled=move || busy.get() || left.get() == 0 on:click=go>{spinner(busy)}<Icon icon=icons::CHECK size=16 />{tr("wa.confirm.go")}</button> }.into_any()
                        } else {
                            view! { <a class="btn primary" href=href target="_blank" rel="noopener"><Icon icon=icons::EXTERNAL_LINK size=16 />{tf("wa.confirm.open", &[("name", &first)])}</a> }.into_any()
                        }}
                    </div>
                </section>
            }
        })}
    }
    .into_any()
}

// ------------------------------------------------------------------ overview

fn engine_status(e: &ProxyEngine) -> AnyView {
    let (tone, key) = if !e.installed {
        (Tone::Unknown, "wa.eng.not_installed")
    } else if !e.running {
        (Tone::Err, "dk.eng.down")
    } else if e.in_sync == Some(false) {
        (Tone::Warn, "wa.eng.out_of_sync")
    } else {
        (Tone::Ok, "wa.eng.running")
    };
    ui::status(tone, t(key).to_string())
}

fn url_links(urls: &[String]) -> AnyView {
    if urls.is_empty() {
        return view! { <span class="muted">"—"</span> }.into_any();
    }
    view! {
        <span class="wa-urls">
            {urls.iter().map(|u| view! { <a class="mono" href=u.clone() target="_blank" rel="noopener noreferrer">{u.clone()}</a> }).collect_view()}
        </span>
    }
    .into_any()
}

fn kv(label: &'static str, value: AnyView) -> AnyView {
    view! { <div class="kv"><dt>{tr(label)}</dt><dd>{value}</dd></div> }.into_any()
}

/// The emergency address on the name the browser uses now.
fn emergency_url(port: u16) -> String {
    format!("https://{}:{port}/", window().location().hostname().unwrap_or_default())
}

fn overview() -> AnyView {
    let w = wa();
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let sync = move |_| {
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = api::post::<(), ProxyEngine>("/api/v1/web-access/sync", None).await;
            busy.set(false);
            match r {
                Ok(_) => w.state.reload(),
                Err(e) => error.set(Some(e.text())),
            }
        });
    };
    let engine = move || {
        w.state.view(5, move || {
            let Some(s) = w.state.data.get() else { return ().into_any() };
            let e = s.engine.clone();
            let last = e.last_load_ms.map(|ms| i18n::datetime(ms.max(0) as u64)).unwrap_or_else(|| t("wa.eng.never").to_string());
            let sync_key = match e.in_sync {
                Some(true) => "wa.eng.in_sync",
                Some(false) => "wa.eng.out_of_sync",
                None => "not_set",
            };
            view! {
                {(!e.installed).then(|| ui::note(Tone::Info, icons::INFO, ui::text(t("wa.eng.missing_text").to_string())))}
                {e.last_error.clone().map(|m| ui::note(Tone::Err, icons::CIRCLE_X, view! { {tr("wa.eng.last_error")}" "{ui::tech_detail(m)} }.into_any()))}
                <dl class="kvs">
                    {kv("wa.eng.state", engine_status(&e))}
                    {e.installed.then(|| kv("wa.eng.sync", ui::text(t(sync_key).to_string())))}
                    {e.installed.then(|| kv("wa.eng.last_load", ui::text(last.clone())))}
                </dl>
                {form_error(error)}
                <div class="sec-actions">
                    <button class="btn ghost sm" disabled=move || busy.get() || !e.installed on:click=sync>{spinner(busy)}<Icon icon=icons::REFRESH_CW size=14 />{tr("wa.eng.sync_now")}</button>
                </div>
            }
            .into_any()
        })
    };
    let ui_card = move || {
        w.state.view(5, move || {
            let Some(s) = w.state.data.get() else { return ().into_any() };
            let cur = s.current();
            let external = s_at(&cur, "/settings/mode", "builtin") == "external";
            let access = access_of(&s_at(&cur, "/settings/ui/access", "lan"));
            let https = cur.pointer("/settings/https_port").and_then(Value::as_u64).unwrap_or(443);
            view! {
                <dl class="kvs">
                    {kv("wa.ui.urls", if external { ui::text(t("wa.ui.external").to_string()) } else { url_links(&s.ui_urls) })}
                    {(!external).then(|| kv("wa.ui.access", view! { <span class="chip">{tr(access_key(access))}</span> }.into_any()))}
                    {(!external).then(|| kv("wa.ui.ports", ui::text(tf("wa.ui.ports_val", &[("https", &https.to_string()), ("http", &cur.pointer("/settings/http_port").and_then(Value::as_u64).unwrap_or(80).to_string())]))))}
                    {kv("wa.ui.lan", ui::breakable(&s.lan.join(", "), "mono small"))}
                    {(!s.vpn.is_empty()).then(|| kv("wa.ui.vpn", ui::breakable(&s.vpn.join(", "), "mono small")))}
                </dl>
                {ui::note(Tone::Info, icons::LIFE_BUOY, view! {
                    <span>{tr("wa.ui.emergency")}" "<a class="mono" href=emergency_url(s.emergency_port) target="_blank" rel="noopener">{emergency_url(s.emergency_port)}</a></span>
                }.into_any())}
            }
            .into_any()
        })
    };
    view! {
        <div class="grid">
            <section class="card widget w-6">
                <WidgetHead icon=icons::SERVER title="wa.eng.title" />
                {engine}
            </section>
            <section class="card widget w-6">
                <WidgetHead icon=icons::MONITOR title="wa.ui.title" />
                {ui_card}
            </section>
            <section class="card widget w-12">
                <WidgetHead icon=icons::SETTINGS_2 title="wa.set.title" />
                {move || w.state.gate(6).unwrap_or_else(settings_form)}
            </section>
        </div>
    }
    .into_any()
}

/// Radio choices of an access level (`public` is offered disabled until its own flow).
fn access_radios(name: &'static str, value: RwSignal<String>) -> AnyView {
    let opt = move |a: AccessLevel, help: &'static str| {
        let v = access_str(a);
        let soon = a == AccessLevel::Public;
        view! {
            <label class="net-radio" class:wa-off=soon>
                <input type="radio" name=name disabled=soon prop:checked=move || value.with(|x| x == v) on:change=move |_| value.set(v.to_string()) />
                <span class="switch-text">
                    <span class="switch-label">{tr(access_key(a))}{soon.then(|| view! { " "<span class="chip">{tr("wa.soon")}</span> })}</span>
                    <span class="field-help">{tr(help)}</span>
                </span>
            </label>
        }
    };
    view! {
        <div class="wa-radios">
            {opt(AccessLevel::Lan, "wa.access.lan_help")}
            {opt(AccessLevel::LanVpn, "wa.access.lan_vpn_help")}
            {opt(AccessLevel::Public, "wa.access.public_help")}
        </div>
    }
    .into_any()
}

/// PUT the configuration, then apply `web_access` only (A19b).
async fn save_config(cfg: Value, error: RwSignal<Option<String>>) -> bool {
    match api::put::<_, Value>(URL, &cfg).await {
        Ok(_) => {
            if apply_now(&[keys::WEB_ACCESS], error).await {
                saved_applied();
                return true;
            }
            false
        }
        Err(e) => {
            error.set(Some(e.text()));
            false
        }
    }
}

fn settings_form() -> AnyView {
    let w = wa();
    let Some(s) = w.state.data.get_untracked() else { return ().into_any() };
    let cur = s.current();
    let mode = RwSignal::new(s_at(&cur, "/settings/mode", "builtin"));
    let local = RwSignal::new(b_at(&cur, "/settings/local_names", true));
    let access = RwSignal::new(s_at(&cur, "/settings/ui/access", "lan"));
    let h3 = RwSignal::new(b_at(&cur, "/settings/h3", true));
    let init = StoredValue::new((mode.get_untracked(), local.get_untracked(), access.get_untracked(), h3.get_untracked()));
    let dirty = Memo::new(move |_| init.get_value() != (mode.get(), local.get(), access.get(), h3.get()));
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let ask = RwSignal::new(false);
    let reset = move || {
        let (m, l, a, h) = init.get_value();
        mode.set(m);
        local.set(l);
        access.set(a);
        h3.set(h);
        error.set(None);
    };
    let save = Callback::new(move |_: ()| {
        let Some(mut cfg) = w.state.data.with_untracked(|s| s.as_ref().map(WaState::current)) else { return };
        let mut settings = cfg.get("settings").cloned().unwrap_or(Value::Null);
        put(&mut settings, "mode", Value::String(mode.get_untracked()));
        put(&mut settings, "local_names", Value::Bool(local.get_untracked()));
        put(&mut settings, "h3", Value::Bool(h3.get_untracked()));
        let mut uiv = settings.get("ui").cloned().unwrap_or(Value::Null);
        put(&mut uiv, "access", Value::String(access.get_untracked()));
        put(&mut settings, "ui", uiv);
        put(&mut cfg, "settings", settings);
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            if save_config(cfg, error).await {
                init.set_value((mode.get_untracked(), local.get_untracked(), access.get_untracked(), h3.get_untracked()));
                ask.set(false);
            }
            busy.set(false);
            w.state.reload();
        });
    });
    // a change of how the interface itself is reached is explained first
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let (m, _, a, h) = init.get_value();
        if m != mode.get_untracked() || a != access.get_untracked() || h != h3.get_untracked() {
            ask.set(true);
        } else {
            save.run(());
        }
    };
    let mode_help = move || t(if mode.get() == "external" { "wa.mode.external_help" } else { "wa.mode.builtin_help" });
    view! {
        <form class="form" on:submit=submit>
            {crate::form::segmented("wa.mode", &[("builtin", "wa.mode.builtin"), ("external", "wa.mode.external")], mode.into(), Callback::new(move |v| mode.set(v)))}
            <p class="small muted wa-help">{mode_help}</p>
            <Show when=move || mode.get() != "external">
                <fieldset class="fieldset">
                    <legend class="field-label">{tr("wa.ui.access")}</legend>
                    {access_radios("wa-ui-access", access)}
                </fieldset>
                {switch("wa.set.local_names", Some("wa.set.local_names_help"), local.into(), Callback::new(move |v| local.set(v)))}
                {switch("wa.set.h3", Some("wa.set.h3_help"), h3.into(), Callback::new(move |v| h3.set(v)))}
            </Show>
            {form_error(error)}
            <div class="sec-actions">
                <button type="submit" class="btn primary" disabled=move || busy.get() || !dirty.get()>{spinner(busy)}{tr("save")}</button>
                <button type="button" class="btn ghost" disabled=move || !dirty.get() on:click=move |_| reset()>{tr("reset")}</button>
            </div>
        </form>
        {move || ask.get().then(|| {
            let external = mode.get_untracked() == "external";
            let port = w.state.data.with_untracked(|s| s.as_ref().map(|s| s.emergency_port).unwrap_or(8443));
            let text = if external { tf("wa.change.external", &[("url", &emergency_url(port))]) } else { tf("wa.change.timed", &[("url", &emergency_url(port))]) };
            ui::confirm(Confirm { busy, error, ..Confirm::new(t("wa.change.title").to_string(), text, "save", false, save, Callback::new(move |_| ask.set(false))) })
        })}
    }
    .into_any()
}

// ------------------------------------------------------------------ routes

fn route_name(r: &RouteView) -> String {
    r.app.clone().or_else(|| r.stack.clone()).unwrap_or_else(|| r.id.clone())
}

fn route_state(r: &RouteView) -> AnyView {
    if let Some(p) = r.problems.iter().find(|p| p.as_str() != "disabled") {
        ui::status(if p == "local_names_off" { Tone::Unknown } else { Tone::Warn }, problem_text(p))
    } else if !r.enabled {
        ui::status(Tone::Unknown, t("wa.route.off").to_string())
    } else {
        ui::status(Tone::Ok, t("wa.route.on").to_string())
    }
}

fn routes_panel() -> AnyView {
    let w = wa();
    let content = move || {
        let Some(s) = w.state.data.get() else { return ().into_any() };
        let external = s_at(&s.applied, "/settings/mode", "builtin") == "external";
        let rows = s
            .routes
            .iter()
            .map(|r| {
                let id = r.id.clone();
                let id2 = r.id.clone();
                let name = route_name(r);
                let label = tf("wa.route.test_named", &[("name", &name)]);
                ui::row(vec![
                    ui::cell("wa.col.app", "wrap", ui::two_lines(view! { <strong>{name.clone()}</strong> }.into_any(), Some(t(kind_key(match r.kind {
                        rynas_api_types::web_access::RouteKind::App => "app",
                        rynas_api_types::web_access::RouteKind::Stack => "stack",
                        rynas_api_types::web_access::RouteKind::Custom => "custom",
                    })).to_string()), "")),
                    ui::cell("wa.col.address", "wrap", url_links(&r.urls)),
                    ui::cell("wa.col.access", "", view! { <span class="chip">{tr(access_key(r.access))}</span> }.into_any()),
                    ui::cell("col.status", "wrap", route_state(r)),
                    ui::actions(view! {
                        <button class="btn ghost sm" aria-label=label on:click=move |_| w.route.set(Some((id.clone(), true)))><Icon icon=icons::LIST_CHECKS size=14 />{tr("wa.route.test")}</button>
                        <button class="btn ghost sm" on:click=move |_| w.route.set(Some((id2.clone(), false)))><Icon icon=icons::PENCIL size=14 />{tr("edit")}</button>
                    }.into_any()),
                ])
            })
            .collect::<Vec<_>>();
        view! {
            {external.then(|| ui::note(Tone::Info, icons::INFO, ui::text(t("wa.routes.external").to_string())))}
            {mdns_note()}
            {if rows.is_empty() {
                view! {
                    <EmptyState icon=icons::LINK title=Signal::derive(|| t("wa.routes.empty").to_string()) text=Signal::derive(|| t("wa.routes.empty_text").to_string())>
                        <a class="btn primary" href="/apps">{tr("nav.apps")}</a>
                    </EmptyState>
                }
                .into_any()
            } else {
                ui::table("wa-routes", &["wa.col.app", "wa.col.address", "wa.col.access", "col.status", ""], rows)
            }}
        }
        .into_any()
    };
    view! { <section class="card widget w-12">{move || w.state.view(6, content)}</section> }.into_any()
}

/// `.local` names need mDNS on the client too.
fn mdns_note() -> AnyView {
    ui::note(Tone::Info, icons::INFO, ui::text(t("wa.mdns").to_string()))
}

fn check_key(id: &str) -> Option<(&'static str, &'static str)> {
    Some(match id {
        "local_name" => ("wa.check.local_name", "wa.fix.local_name"),
        "dns" => ("wa.check.dns", "wa.fix.dns"),
        "proxy_running" => ("wa.check.proxy_running", "wa.fix.proxy_running"),
        "route_loaded" => ("wa.check.route_loaded", "wa.fix.route_loaded"),
        "https_listening" => ("wa.check.https_listening", "wa.fix.https_listening"),
        "certificate" => ("wa.check.certificate", "wa.fix.certificate"),
        "upstream" => ("wa.check.upstream", "wa.fix.upstream"),
        _ => return None,
    })
}

fn check_state(s: CheckState) -> (Tone, &'static str) {
    match s {
        CheckState::Ok => (Tone::Ok, "wa.cs.ok"),
        CheckState::Warn => (Tone::Warn, "wa.cs.warn"),
        CheckState::Fail => (Tone::Err, "wa.cs.fail"),
        CheckState::Skipped => (Tone::Unknown, "wa.cs.skipped"),
    }
}

fn checklist(d: &RouteDiagnostics) -> AnyView {
    let items = d
        .checks
        .iter()
        .map(|c| {
            let (tone, sk) = check_state(c.state);
            let keys = check_key(&c.id);
            let title = keys.map(|k| t(k.0).to_string()).unwrap_or_else(|| c.id.clone());
            let fix = keys.filter(|_| matches!(c.state, CheckState::Warn | CheckState::Fail)).map(|k| t(k.1));
            view! {
                <li class="wa-check">
                    <span class=format!("wa-check-icon {}", tone.class()) title=tr(sk)><Icon icon=tone.icon() size=16 /></span>
                    <div class="cell-main">
                        <span><strong>{title}</strong>" · "<span class="muted small">{tr(sk)}</span></span>
                        {fix.map(|f| view! { <span class="small">{f}</span> })}
                        {(!c.detail.is_empty()).then(|| view! { <span class="cell-sub small" lang="en">{c.detail.clone()}</span> })}
                    </div>
                </li>
            }
        })
        .collect_view();
    view! {
        <ol class="wa-checks">{items}</ol>
        <p class="small muted">{tf("wa.test.at", &[("when", &i18n::datetime(d.at_ms.max(0) as u64))])}</p>
    }
    .into_any()
}

fn route_drawer(id: String, run_now: bool) -> AnyView {
    let w = wa();
    let close = Callback::new(move |_| w.route.set(None));
    let rid = StoredValue::new(id.clone());
    let view_of = Memo::new(move |_| w.state.data.with(|s| s.as_ref().and_then(|s| s.routes.iter().find(|r| r.id == rid.get_value()).cloned())));
    let cfg_of = move || w.state.data.with_untracked(|s| s.as_ref().map(WaState::current)).and_then(|c| c.get("routes").and_then(Value::as_array).and_then(|a| a.iter().find(|r| r.get("id").and_then(Value::as_str) == Some(rid.get_value().as_str())).cloned()));
    let start = cfg_of().unwrap_or(Value::Null);
    let access = RwSignal::new(s_at(&start, "/access", "lan"));
    let enabled = RwSignal::new(b_at(&start, "/enabled", true));
    let init = StoredValue::new((access.get_untracked(), enabled.get_untracked()));
    let dirty = Memo::new(move |_| init.get_value() != (access.get(), enabled.get()));
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let diag = RwSignal::new(None::<RouteDiagnostics>);
    let testing = RwSignal::new(false);
    let test_err = RwSignal::new(None::<String>);
    let test = move || {
        testing.set(true);
        test_err.set(None);
        let url = format!("/api/v1/web-access/routes/{}/test", api::encode(&rid.get_value()));
        spawn_local(async move {
            let r = api::post::<(), RouteDiagnostics>(&url, None).await;
            let _ = testing.try_set(false);
            match r {
                Ok(d) => {
                    let _ = diag.try_set(Some(d));
                }
                Err(e) => {
                    let _ = test_err.try_set(Some(e.text()));
                }
            }
        });
    };
    if run_now {
        test();
    }
    let save = move |_| {
        let Some(mut cfg) = w.state.data.with_untracked(|s| s.as_ref().map(WaState::current)) else { return };
        if let Some(r) = cfg.get_mut("routes").and_then(Value::as_array_mut).and_then(|a| a.iter_mut().find(|r| r.get("id").and_then(Value::as_str) == Some(rid.get_value().as_str()))) {
            put(r, "access", Value::String(access.get_untracked()));
            put(r, "enabled", Value::Bool(enabled.get_untracked()));
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            if save_config(cfg, error).await {
                init.set_value((access.get_untracked(), enabled.get_untracked()));
            }
            busy.set(false);
            w.state.reload();
        });
    };
    let body = move || {
        let Some(r) = view_of.get() else { return ui::note(Tone::Info, icons::INFO, ui::text(t("wa.route.gone").to_string())) };
        let cfg = cfg_of().unwrap_or(Value::Null);
        let auth = s_at(&cfg, "/auth", "app");
        let headers = s_at(&cfg, "/headers", "default");
        let problems = r.problems.iter().filter(|p| p.as_str() != "disabled").map(|p| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(problem_text(p)))).collect_view();
        view! {
            {problems}
            <dl class="kvs">
                {kv("wa.col.address", url_links(&r.urls))}
                {kv("wa.route.names", ui::breakable(&if r.names.is_empty() { "—".to_string() } else { r.names.join(", ") }, "mono"))}
                {kv("wa.route.upstream", ui::breakable(&r.upstream, "mono"))}
                {kv("wa.route.kind", ui::text(t(kind_key(&s_at(&cfg, "/kind", "app"))).to_string()))}
                {r.stack.clone().map(|st| kv("dk.ed.name", view! { <a class="mono" href=format!("/docker?stack={}", api::encode(&st))>{st.clone()}</a> }.into_any()))}
                {kv("wa.route.login", ui::text(t(if auth == "none" { "wa.auth.none" } else { "wa.auth.app" }).to_string()))}
                {kv("wa.route.headers", ui::text(t(if headers == "relaxed" { "wa.headers.relaxed" } else { "wa.headers.default" }).to_string()))}
            </dl>
            <h3 class="mini-title">{tr("wa.col.access")}</h3>
            {access_radios("wa-route-access", access)}
            {(auth == "none").then(|| ui::note(Tone::Info, icons::LOCK, ui::text(t("wa.route.auth_none").to_string())))}
            {switch("wa.route.enabled", Some("wa.route.enabled_help"), enabled.into(), Callback::new(move |v| enabled.set(v)))}
            {form_error(error)}
            <div class="sec-actions">
                <button class="btn primary sm" disabled=move || busy.get() || !dirty.get() on:click=save>{spinner(busy)}{tr("save")}</button>
            </div>
            <h3 class="mini-title">{tr("wa.test.title")}</h3>
            <p class="small muted">{tr("wa.test.text")}</p>
            {mdns_note()}
            <div class="sec-actions">
                <button class="btn ghost sm" disabled=move || testing.get() on:click=move |_| test()>{spinner(testing)}<Icon icon=icons::LIST_CHECKS size=14 />{tr("wa.route.test")}</button>
            </div>
            {form_error(test_err)}
            {move || diag.with(|d| d.as_ref().map(checklist))}
        }
        .into_any()
    };
    let title = Signal::derive(move || view_of.with(|r| r.as_ref().map(route_name)).map(|n| tf("wa.route.title", &[("name", &n)])).unwrap_or_default());
    let foot = view! { <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("close")}</button> }.into_any();
    ui::drawer(title, close, view! { {body} }.into_any(), foot)
}
