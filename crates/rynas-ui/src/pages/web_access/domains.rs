//! Own domains (wave W2 of `docs/phase2/docker-apps.md` §5.4): a domain with a Let's Encrypt
//! certificate through ACME DNS-01, the LAN records rynas writes at the DNS provider, the
//! provider credentials (write-only) and the certificate dashboard.
//!
//! - Domains tab: one row per domain (records, certificate, status), the ACME account settings
//!   and the credentials; a drawer per domain with its problems and their fixes, the records of
//!   the last sync ("Write records now", taking over a name rynas did not write), its
//!   certificates, the domain checklist and the settings.
//! - Adding a domain is a guided form: provider → token → domain → preview of the DNS changes
//!   (`POST /web-access/plan` with the candidate configuration: the provider is only read),
//!   where every `conflict` needs an explicit "Replace" (`take_over`).
//! - Secret values are typed, sent once and never shown again: answers carry only the names of
//!   the stored values (`fields`).
//! - Certificate tab: [`inventory`], every certificate rynas serves.
//!
//! Like the rest of the page the configuration travels as JSON: only the fields shown here are
//! changed, the rest goes back untouched.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::web_access::{
    CertEntry, CertKind, CertStatus, CertificateInventory, DnsAction, DnsChange, DnsProviderInfo, DnsSyncReport, DomainView, RouteDiagnostics,
};
use serde_json::{Map, Value, json};

use super::{WaState, b_at, kv, put, s_at, save_config, url_links, wa};
use crate::api;
use crate::form::{In, form_error, spinner, switch, text_field};
use crate::i18n::{self, t, tf, tr, try_t};
use crate::icons::{self, Icon};
use crate::pages::security::copy_button;
use crate::ui::{self, Confirm, EmptyState, Remote, Tone, WidgetHead};

const PROVIDERS: &str = "/api/v1/web-access/providers";
const CERTS: &str = "/api/v1/web-access/certificates";

/// What the tab's parts share.
#[derive(Clone, Copy)]
struct Dom {
    providers: Remote<Vec<DnsProviderInfo>>,
    certs: Remote<CertificateInventory>,
    /// Domain id of the open drawer (`""`: a new domain), and whether its checks run at once.
    open: RwSignal<Option<(String, bool)>>,
    /// Credential id of the open credential dialog (`""`: a new one).
    cred: RwSignal<Option<String>>,
}

fn dom() -> Dom {
    expect_context::<Dom>()
}

/// Provider names are brands: the same in every language.
fn brand(p: &str) -> String {
    match p {
        "cloudflare" => "Cloudflare",
        "desec" => "deSEC",
        "hetzner" => "Hetzner",
        "porkbun" => "Porkbun",
        "ovh" => "OVHcloud",
        "duckdns" => "DuckDNS",
        "gandi" => "Gandi",
        "route53" => "Amazon Route 53",
        "digitalocean" => "DigitalOcean",
        "rfc2136" => "RFC 2136",
        "acmedns" => "acme-dns",
        other => return other.to_string(),
    }
    .to_string()
}

fn challenge_key(c: &str) -> &'static str {
    match c {
        "http01" => "dom.ch.http01",
        "cname_delegation" => "dom.ch.cname_delegation",
        _ => "dom.ch.dns01",
    }
}

/// The label of a credential value (`dns.f.<name>`), else the name itself.
fn field_label(name: &str) -> String {
    try_t(&format!("dns.f.{name}")).map(str::to_string).unwrap_or_else(|| name.to_string())
}

fn problem(p: &str) -> (Tone, String) {
    let tone = match p {
        "credential_missing" | "records_failed" => Tone::Err,
        "records_manual" => Tone::Info,
        _ => Tone::Warn,
    };
    (tone, try_t(&format!("dom.prob.{p}")).map(str::to_string).unwrap_or_else(|| p.to_string()))
}

fn action_of(a: DnsAction) -> (Tone, &'static str) {
    match a {
        DnsAction::Keep => (Tone::Ok, "dns.act.keep"),
        DnsAction::Create => (Tone::Info, "dns.act.create"),
        DnsAction::Update => (Tone::Info, "dns.act.update"),
        DnsAction::Delete => (Tone::Info, "dns.act.delete"),
        DnsAction::Manual => (Tone::Warn, "dns.act.manual"),
        DnsAction::Unknown => (Tone::Unknown, "dns.act.unknown"),
        DnsAction::Conflict => (Tone::Warn, "dns.act.conflict"),
    }
}

pub fn cert_status(s: CertStatus) -> (Tone, &'static str) {
    match s {
        CertStatus::Ok => (Tone::Ok, "cert.st.ok"),
        CertStatus::ExpiresSoon => (Tone::Warn, "cert.st.expires_soon"),
        CertStatus::Expired => (Tone::Err, "cert.st.expired"),
        CertStatus::RenewalOverdue => (Tone::Err, "cert.st.renewal_overdue"),
        CertStatus::Pending => (Tone::Info, "cert.st.pending"),
        CertStatus::Failed => (Tone::Err, "cert.st.failed"),
    }
}

fn cert_rank(s: CertStatus) -> u8 {
    match s {
        CertStatus::Ok => 0,
        CertStatus::Pending => 1,
        CertStatus::ExpiresSoon => 2,
        CertStatus::RenewalOverdue => 3,
        CertStatus::Failed => 4,
        CertStatus::Expired => 5,
    }
}

fn cert_kind(k: CertKind) -> &'static str {
    match k {
        CertKind::LocalCa => "cert.kind.local_ca",
        CertKind::Interface => "cert.kind.interface",
        CertKind::ProxyLocal => "cert.kind.proxy_local",
        CertKind::Acme => "cert.kind.acme",
        CertKind::Imported => "cert.kind.imported",
    }
}

/// The domain checklist (`POST /web-access/domains/{id}/test`): title and fix of each step.
fn check_key(id: &str) -> Option<(&'static str, &'static str)> {
    Some(match id {
        "dns_record" => ("dom.check.dns_record", "dom.fix.dns_record"),
        "dns_nas" => ("dom.check.dns_nas", "dom.fix.dns_nas"),
        "dns_public" => ("dom.check.dns_public", "dom.fix.dns_public"),
        "rebind" => ("dom.check.rebind", "dom.fix.rebind"),
        "certificate" => ("dom.check.certificate", "dom.fix.certificate"),
        "renewal" => ("dom.check.renewal", "dom.fix.renewal"),
        "hairpin" => ("dom.check.hairpin", "dom.fix.hairpin"),
        _ => return None,
    })
}

// ------------------------------------------------------------------ configuration helpers

fn current() -> Option<Value> {
    wa().state.data.with_untracked(|s| s.as_ref().map(WaState::current))
}

fn list<'a>(cfg: &'a Value, key: &str) -> &'a [Value] {
    cfg.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

fn list_mut<'a>(cfg: &'a mut Value, key: &str) -> &'a mut Vec<Value> {
    if !cfg.get(key).is_some_and(Value::is_array) {
        put(cfg, key, Value::Array(Vec::new()));
    }
    cfg.get_mut(key).and_then(Value::as_array_mut).expect("array set above")
}

fn by_id<'a>(items: &'a [Value], id: &str) -> Option<&'a Value> {
    items.iter().find(|v| v.get("id").and_then(Value::as_str) == Some(id))
}

fn ids(items: &[Value]) -> Vec<String> {
    items.iter().filter_map(|v| v.get("id").and_then(Value::as_str)).map(str::to_string).collect()
}

/// A free id from `base`: `[a-z0-9-]`, at most 63 characters, `-2`, `-3`… when taken.
fn free_id(base: &str, taken: &[String]) -> String {
    let mut s: String = base.to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    s = s.trim_matches('-').chars().take(56).collect::<String>().trim_end_matches('-').to_string();
    if s.is_empty() {
        s = "domain".into();
    }
    let mut id = s.clone();
    let mut n = 2;
    while taken.contains(&id) {
        id = format!("{s}-{n}");
        n += 1;
    }
    id
}

fn str_list(v: &Value, key: &str) -> Vec<String> {
    v.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
}

/// Remove a key from a JSON object.
fn drop_key(v: &mut Value, key: &str) {
    if let Some(m) = v.as_object_mut() {
        m.remove(key);
    }
}

// ------------------------------------------------------------------ the tab

pub fn panel() -> AnyView {
    let d = Dom { providers: Remote::new(PROVIDERS), certs: Remote::new(CERTS), open: RwSignal::new(None), cred: RwSignal::new(None) };
    provide_context(d);
    let w = wa();
    // the certificates follow every apply (a new domain, a renewal)
    Effect::new(move |prev: Option<()>| {
        w.state.data.track();
        if prev.is_some() {
            d.certs.reload();
        }
    });
    view! {
        <div class="grid">
            <section class="card widget w-12">
                <WidgetHead icon=icons::LINK title="dom.title">
                    <button class="btn primary sm" on:click=move |_| d.open.set(Some((String::new(), false)))><Icon icon=icons::PLUS size=14 />{tr("dom.add")}</button>
                </WidgetHead>
                {move || w.state.view(5, domain_list)}
            </section>
            <section class="card widget w-6">
                <WidgetHead icon=icons::SHIELD_CHECK title="dom.acme.title" />
                {acme_card}
            </section>
            <section class="card widget w-6">
                <WidgetHead icon=icons::KEY title="dom.cred.title">
                    <button class="btn ghost sm" aria-label=tr("dom.cred.add") on:click=move |_| d.cred.set(Some(String::new()))><Icon icon=icons::PLUS size=14 />{tr("create")}</button>
                </WidgetHead>
                {move || w.state.view(4, cred_list)}
            </section>
        </div>
        {move || d.open.get().map(|(id, run)| domain_drawer(id, run))}
        {move || d.cred.get().map(cred_dialog)}
    }
    .into_any()
}

/// The worst certificate status of a domain, `None` before there is any.
fn domain_cert(d: Dom, name: &str) -> Option<CertStatus> {
    d.certs.data.with(|c| c.as_ref().and_then(|c| c.certificates.iter().filter(|e| e.domain.as_deref() == Some(name)).map(|e| e.status).max_by_key(|s| cert_rank(*s))))
}

fn records_summary(r: Option<&DnsSyncReport>, lan_record: bool) -> AnyView {
    let Some(r) = r else {
        return ui::status(Tone::Unknown, t(if lan_record { "dom.rec.not_yet" } else { "dom.rec.off" }).to_string());
    };
    let failed = r.changes.iter().filter(|c| c.detail.starts_with("failed: ")).count();
    let conflicts = r.changes.iter().filter(|c| c.action == DnsAction::Conflict && !c.wanted.is_empty()).count();
    let manual = r.changes.iter().filter(|c| c.action == DnsAction::Manual).count();
    if failed > 0 {
        ui::status(Tone::Err, tf("dom.rec.failed", &[("n", &failed.to_string())]))
    } else if conflicts > 0 {
        ui::status(Tone::Warn, tf("dom.rec.conflicts", &[("n", &conflicts.to_string())]))
    } else if manual > 0 {
        ui::status(Tone::Warn, t("dom.rec.manual").to_string())
    } else if r.changes.iter().any(|c| c.action == DnsAction::Unknown) {
        ui::status(Tone::Unknown, t("dns.act.unknown").to_string())
    } else {
        ui::status(Tone::Ok, t("dom.rec.ok").to_string())
    }
}

fn domain_state(v: &DomainView) -> AnyView {
    if !v.enabled {
        return ui::status(Tone::Unknown, t("wa.route.off").to_string());
    }
    match v.problems.first() {
        Some(p) => {
            let (tone, _) = problem(p);
            ui::status(tone, try_t(&format!("dom.ps.{p}")).map(str::to_string).unwrap_or_else(|| p.clone()))
        }
        None => ui::status(Tone::Ok, t("dom.state.ok").to_string()),
    }
}

fn domain_list() -> AnyView {
    let w = wa();
    let d = dom();
    let Some(s) = w.state.data.get() else { return ().into_any() };
    let cfg = s.current();
    let external = s_at(&cfg, "/settings/mode", "builtin") == "external";
    let rows = s
        .domains
        .iter()
        .map(|v| {
            let c = by_id(list(&cfg, "domains"), &v.id).cloned().unwrap_or(Value::Null);
            let sub = format!("{} · {}", brand(&s_at(&c, "/provider", "")), t(challenge_key(&s_at(&c, "/challenge", "dns01"))));
            let cert = domain_cert(d, &v.name).map(|st| {
                let (tone, k) = cert_status(st);
                ui::status(tone, t(k).to_string())
            });
            let (id, id2) = (v.id.clone(), v.id.clone());
            let label = tf("dom.check_named", &[("name", &v.name)]);
            ui::row(vec![
                ui::cell("dom.col.domain", "wrap", ui::two_lines(view! { <strong>{dotted(&v.name)}</strong> }.into_any(), Some(sub), "cell-sub small")),
                ui::cell("dom.col.records", "wrap", records_summary(v.records.as_ref(), b_at(&c, "/lan_record", true))),
                ui::cell("dom.col.cert", "wrap", cert.unwrap_or_else(|| view! { <span class="muted">"—"</span> }.into_any())),
                ui::cell("col.status", "wrap", domain_state(v)),
                ui::actions(view! {
                    <button class="btn ghost sm" aria-label=label on:click=move |_| d.open.set(Some((id.clone(), true)))><Icon icon=icons::LIST_CHECKS size=14 />{tr("wa.route.test")}</button>
                    <button class="btn ghost sm" on:click=move |_| d.open.set(Some((id2.clone(), false)))><Icon icon=icons::PENCIL size=14 />{tr("dom.open")}</button>
                }.into_any()),
            ])
        })
        .collect::<Vec<_>>();
    view! {
        {external.then(|| ui::note(Tone::Info, icons::INFO, ui::text(t("dom.external").to_string())))}
        {if rows.is_empty() {
            view! {
                <EmptyState icon=icons::LINK title=Signal::derive(|| t("dom.empty").to_string()) text=Signal::derive(|| t("dom.empty_text").to_string())>
                    <button class="btn primary" on:click=move |_| d.open.set(Some((String::new(), false)))><Icon icon=icons::PLUS size=16 />{tr("dom.add")}</button>
                </EmptyState>
            }
            .into_any()
        } else {
            view! {
                <p class="small muted wa-help">{tr("dom.intro")}</p>
                {ui::table("wa-routes", &["dom.col.domain", "dom.col.records", "dom.col.cert", "col.status", ""], rows)}
            }
            .into_any()
        }}
    }
    .into_any()
}

// ------------------------------------------------------------------ ACME account

fn acme_card() -> AnyView {
    let w = wa();
    let key = Memo::new(move |_| w.state.data.with(|s| s.as_ref().map(|s| s.current().get("acme").cloned().unwrap_or(Value::Null))));
    (move || {
        key.track();
        w.state.gate(4).unwrap_or_else(acme_form)
    })
    .into_any()
}

fn acme_form() -> AnyView {
    let w = wa();
    let cfg = current().unwrap_or(Value::Null);
    let first = (s_at(&cfg, "/acme/email", ""), b_at(&cfg, "/acme/staging", false), s_at(&cfg, "/acme/profile", ""));
    let email = RwSignal::new(first.0.clone());
    let staging = RwSignal::new(first.1);
    let profile = RwSignal::new(first.2.clone());
    let init = StoredValue::new(first);
    let dirty = Memo::new(move |_| init.get_value() != (email.get(), staging.get(), profile.get()));
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let email_err = RwSignal::new(None::<String>);
    let save = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let Some(mut cfg) = current() else { return };
        let mut acme = cfg.get("acme").cloned().unwrap_or(Value::Null);
        put(&mut acme, "staging", Value::Bool(staging.get_untracked()));
        for (k, v) in [("email", email.get_untracked()), ("profile", profile.get_untracked())] {
            if v.trim().is_empty() {
                drop_key(&mut acme, k);
            } else {
                put(&mut acme, k, Value::String(v.trim().to_string()));
            }
        }
        put(&mut cfg, "acme", acme);
        busy.set(true);
        error.set(None);
        email_err.set(None);
        spawn_local(async move {
            if save_config(cfg, error).await {
                init.set_value((email.get_untracked(), staging.get_untracked(), profile.get_untracked()));
            }
            busy.set(false);
            w.state.reload();
        });
    };
    let pid = crate::form::fid();
    view! {
        <form class="form" on:submit=save>
            <p class="small muted wa-help">{tr("dom.acme.text")}</p>
            {text_field("dom.acme.email", email.into(), Callback::new(move |v| email.set(v)), email_err.into(), In { kind: "email", help: Some("dom.acme.email_help"), autocomplete: "email", ..In::default() })}
            {switch("dom.acme.staging", Some("dom.acme.staging_help"), staging.into(), Callback::new(move |v| staging.set(v)))}
            <div class="field">
                <label class="field-label" for=pid.clone()>{tr("dom.acme.profile")}</label>
                <select id=pid class="input" on:change=move |ev| profile.set(event_target_value(&ev))>
                    {[("", "dom.acme.profile_default"), ("tlsserver", "dom.acme.profile_tlsserver"), ("classic", "dom.acme.profile_classic")]
                        .into_iter()
                        .map(|(v, k)| view! { <option value=v selected=move || profile.with(|p| p == v)>{tr(k)}</option> })
                        .collect_view()}
                </select>
            </div>
            {form_error(error)}
            <div class="sec-actions">
                <button type="submit" class="btn primary" disabled=move || busy.get() || !dirty.get()>{spinner(busy)}{tr("save")}</button>
            </div>
        </form>
    }
    .into_any()
}

// ------------------------------------------------------------------ credentials

fn cred_list() -> AnyView {
    let w = wa();
    let d = dom();
    let Some(cfg) = w.state.data.with(|s| s.as_ref().map(WaState::current)) else { return ().into_any() };
    let creds = list(&cfg, "credentials").to_vec();
    if creds.is_empty() {
        return view! { <p class="small muted">{tr("dom.cred.empty")}</p> }.into_any();
    }
    let ask = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let remove = Callback::new(move |_| {
        let Some(id) = ask.get_untracked() else { return };
        let Some(mut cfg) = current() else { return };
        list_mut(&mut cfg, "credentials").retain(|c| c.get("id").and_then(Value::as_str) != Some(id.as_str()));
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            if save_config(cfg, error).await {
                ask.set(None);
            }
            busy.set(false);
            w.state.reload();
        });
    });
    let items = creds
        .iter()
        .map(|c| {
            let id = s_at(c, "/id", "");
            let used = list(&cfg, "domains").iter().filter(|dm| dm.get("credential").and_then(Value::as_str) == Some(id.as_str())).count();
            let label = s_at(c, "/label", "");
            let fields = str_list(c, "fields");
            let (id_e, id_r) = (id.clone(), id.clone());
            view! {
                <li class="wa-check">
                    <span class="wa-check-icon tone-ok"><Icon icon=icons::KEY size=16 /></span>
                    <div class="cell-main">
                        <span><strong>{brand(&s_at(c, "/provider", ""))}</strong>{(!label.is_empty()).then(|| view! { " · "<span class="wa-name">{label.clone()}</span> })}</span>
                        <span class="small muted">{if fields.is_empty() { t("dom.cred.no_values").to_string() } else { tf("dom.cred.stored", &[("fields", &fields.iter().map(|f| field_label(f)).collect::<Vec<_>>().join(", "))]) }}</span>
                        <span class="small muted">{tf("dom.cred.used", &[("n", &used.to_string())])}</span>
                        <div class="sec-actions">
                            <button class="btn ghost sm" on:click=move |_| d.cred.set(Some(id_e.clone()))><Icon icon=icons::PENCIL size=14 />{tr("edit")}</button>
                            <button class="btn ghost sm" disabled={used > 0} title={(used > 0).then(|| t("dom.cred.in_use"))} on:click=move |_| { error.set(None); ask.set(Some(id_r.clone())); }><Icon icon=icons::TRASH_2 size=14 />{tr("dom.remove_go")}</button>
                        </div>
                    </div>
                </li>
            }
        })
        .collect_view();
    view! {
        <ul class="wa-checks">{items}</ul>
        {move || ask.get().map(|_| ui::confirm(Confirm { busy, error, ..Confirm::new(t("dom.cred.remove").to_string(), t("dom.cred.remove_text").to_string(), "dom.remove_go", true, remove, Callback::new(move |_| ask.set(None))) }))}
    }
    .into_any()
}

/// The inputs of a provider's credential values. Secret values are password fields that start
/// empty (nothing is ever read back); `stored` says the values exist and may stay.
fn secret_inputs(info: DnsProviderInfo, values: RwSignal<Vec<(String, String)>>, err: RwSignal<Option<String>>) -> AnyView {
    let fields = info
        .fields
        .iter()
        .map(|f| {
            let id = crate::form::fid();
            let name = f.name.clone();
            let n2 = f.name.clone();
            let label = if f.optional { tf("dom.f.optional", &[("name", &field_label(&f.name))]) } else { field_label(&f.name) };
            view! {
                <div class="field">
                    <label class="field-label" for=id.clone()>{label}</label>
                    <input id=id class="input mono" type=if f.secret { "password" } else { "text" }
                        autocomplete=if f.secret { "new-password" } else { "off" } spellcheck="false" maxlength="4096"
                        prop:value=move || values.with(|v| v.iter().find(|(k, _)| *k == name).map(|(_, x)| x.clone()).unwrap_or_default())
                        on:input=move |ev| {
                            let x = event_target_value(&ev);
                            err.set(None);
                            values.update(|v| match v.iter_mut().find(|(k, _)| *k == n2) {
                                Some(e) => e.1 = x,
                                None => v.push((n2.clone(), x)),
                            });
                        } />
                </div>
            }
        })
        .collect_view();
    view! {
        {fields}
        <p class="small muted">{tr("dom.f.never_shown")}</p>
        {(!info.token_hint.is_empty()).then(|| ui::note(Tone::Info, icons::KEY, view! { <span lang="en">{info.token_hint.clone()}</span> }.into_any()))}
        <span class="field-error" aria-live="polite">{move || err.get().map(|e| view! { <Icon icon=icons::CIRCLE_X size=12 />{ui::err_view(e)} })}</span>
    }
    .into_any()
}

fn filled(values: &[(String, String)]) -> Map<String, Value> {
    values.iter().filter(|(_, v)| !v.trim().is_empty()).map(|(k, v)| (k.clone(), Value::String(v.trim().to_string()))).collect()
}

/// A provider `<select>` over the providers rynas knows.
fn provider_select(label: &'static str, list: Vec<DnsProviderInfo>, value: RwSignal<String>, disabled: bool) -> AnyView {
    let id = crate::form::fid();
    view! {
        <div class="field">
            <label class="field-label" for=id.clone()>{tr(label)}</label>
            <select id=id class="input" disabled=disabled on:change=move |ev| value.set(event_target_value(&ev))>
                {list.into_iter().map(|p| {
                    let v = p.provider.as_str();
                    view! { <option value=v selected=move || value.with(|x| x == v)>{brand(v)}</option> }
                }).collect_view()}
            </select>
        </div>
    }
    .into_any()
}

fn provider_info(d: Dom, p: &str) -> Option<DnsProviderInfo> {
    d.providers.data.with(|l| l.as_ref().and_then(|l| l.iter().find(|i| i.provider.as_str() == p).cloned()))
}

/// Field errors of a credential (`credentials[i].secrets...`) go under its values.
fn split_error(e: &api::ApiErr, secrets: RwSignal<Option<String>>, other: RwSignal<Option<String>>) {
    if e.field().is_some_and(|f| f.starts_with("credentials")) {
        secrets.set(Some(e.text()));
    } else {
        other.set(Some(e.text()));
    }
}

/// PUT the configuration then apply it, splitting a credential's field error from the rest.
async fn save_split(cfg: Value, secrets: RwSignal<Option<String>>, error: RwSignal<Option<String>>) -> bool {
    match api::put::<_, Value>(super::URL, &cfg).await {
        Ok(_) => {
            if crate::config::apply_now(&[crate::config::keys::WEB_ACCESS], error).await {
                crate::toast::saved_applied();
                return true;
            }
            false
        }
        Err(e) => {
            split_error(&e, secrets, error);
            false
        }
    }
}

fn cred_dialog(id: String) -> AnyView {
    let w = wa();
    let d = dom();
    let close = Callback::new(move |_| d.cred.set(None));
    let is_new = id.is_empty();
    let start = current().and_then(|c| by_id(list(&c, "credentials"), &id).cloned()).unwrap_or(Value::Null);
    let provider = RwSignal::new(s_at(&start, "/provider", "cloudflare"));
    let label = RwSignal::new(s_at(&start, "/label", ""));
    let values = RwSignal::new(Vec::<(String, String)>::new());
    let stored = str_list(&start, "fields");
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let sec_err = RwSignal::new(None::<String>);
    let cid = StoredValue::new(id);
    Effect::new(move |prev: Option<String>| {
        let p = provider.get();
        if prev.is_some_and(|x| x != p) {
            values.set(Vec::new());
        }
        p
    });
    let go = move |_| {
        let Some(mut cfg) = current() else { return };
        let secrets = filled(&values.get_untracked());
        let creds = list_mut(&mut cfg, "credentials");
        if is_new {
            let nid = free_id(&provider.get_untracked(), &ids(creds));
            creds.push(json!({"id": nid, "provider": provider.get_untracked(), "label": label.get_untracked().trim(), "secrets": secrets}));
        } else if let Some(c) = creds.iter_mut().find(|c| c.get("id").and_then(Value::as_str) == Some(cid.get_value().as_str())) {
            put(c, "label", Value::String(label.get_untracked().trim().to_string()));
            if !secrets.is_empty() {
                put(c, "secrets", Value::Object(secrets));
            }
        }
        busy.set(true);
        error.set(None);
        sec_err.set(None);
        spawn_local(async move {
            let ok = save_split(cfg, sec_err, error).await;
            busy.set(false);
            w.state.reload();
            if ok {
                close.run(());
            }
        });
    };
    let body = move || {
        let Some(list) = d.providers.data.get() else { return d.providers.view(4, || ().into_any()) };
        view! {
            {provider_select("dom.provider", list, provider, !is_new)}
            {text_field("dom.cred.label", label.into(), Callback::new(move |v| label.set(v)), Signal::stored(None), In { help: Some("dom.cred.label_help"), ..In::default() })}
            {(!stored.is_empty()).then(|| ui::note(Tone::Info, icons::LOCK, ui::text(t("dom.cred.keep").to_string())))}
            {move || provider_info(d, &provider.get()).map(|i| secret_inputs(i, values, sec_err))}
            {form_error(error)}
        }
        .into_any()
    };
    let foot = view! {
        <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("cancel")}</button>
        <button type="button" class="btn primary" disabled=move || busy.get() on:click=go>{spinner(busy)}{tr("save")}</button>
    }
    .into_any();
    let title = if is_new { "dom.cred.add" } else { "dom.cred.edit" };
    ui::dialog(ui::DlgKind::Normal, Signal::derive(move || t(title).to_string()), close, view! { {body} }.into_any(), Some(foot))
}

// ------------------------------------------------------------------ records

/// A DNS name that wraps only after a dot (`nas.example.` / `com`), never inside a label.
fn dotted(s: &str) -> AnyView {
    let n = s.split('.').count();
    let parts = s
        .split('.')
        .enumerate()
        .map(|(i, l)| if i + 1 < n { view! { {format!("{l}.")}<wbr /> }.into_any() } else { l.to_string().into_any() })
        .collect::<Vec<_>>();
    view! { <span class="mono">{parts}</span> }.into_any()
}

/// Zone-file lines of a record set (what to add by hand).
fn zone_lines(c: &DnsChange) -> String {
    c.wanted.iter().map(|v| format!("{}. {} IN {} {}", c.name.trim_end_matches('.'), c.ttl.max(60), c.rtype, v)).collect::<Vec<_>>().join("\n")
}

/// How a conflict row is taken over: a check box in the form (`take_over` of the draft), or
/// a button asking first (the domain's drawer).
#[derive(Clone, Copy)]
enum Take {
    Toggle(RwSignal<Vec<String>>),
    Ask(RwSignal<Option<DnsChange>>),
}

fn records_table(changes: Vec<DnsChange>, take: Take) -> AnyView {
    if changes.is_empty() {
        return view! { <p class="small muted">{tr("dom.rec.none")}</p> }.into_any();
    }
    let manual: Vec<&DnsChange> = changes.iter().filter(|c| c.action == DnsAction::Manual).collect();
    let all_manual = manual.iter().map(|c| zone_lines(c)).collect::<Vec<_>>().join("\n");
    // the actions column only when a row has one (more room for names and values)
    let acts = changes.iter().any(|c| c.action == DnsAction::Manual || (c.action == DnsAction::Conflict && !c.wanted.is_empty()));
    let rows = changes
        .iter()
        .map(|c| {
            let failed = c.detail.starts_with("failed: ");
            let (tone, k) = if failed { (Tone::Err, "dns.act.failed") } else { action_of(c.action) };
            let value = if c.wanted.is_empty() { "—".to_string() } else { c.wanted.join(", ") };
            let now = (!c.current.is_empty() && c.current != c.wanted).then(|| tf("dns.now", &[("v", &c.current.join(", "))]));
            let detail = (!c.detail.is_empty()).then(|| view! { <span class="cell-sub small" lang="en">{c.detail.clone()}</span> });
            let action = match (c.action, take) {
                (DnsAction::Manual, _) => copy_button("dns.copy", zone_lines(c)).into_any(),
                (DnsAction::Conflict, Take::Toggle(list)) if !c.wanted.is_empty() => {
                    let n = c.name.clone();
                    let n2 = c.name.clone();
                    view! {
                        <label class="switch-row">
                            <input type="checkbox" prop:checked=move || list.with(|l| l.contains(&n)) on:change=move |ev| {
                                let on = event_target_checked(&ev);
                                list.update(|l| { l.retain(|x| *x != n2); if on { l.push(n2.clone()); } });
                            } />
                            <span class="switch-label">{tr("dns.take_over")}</span>
                        </label>
                    }
                    .into_any()
                }
                (DnsAction::Conflict, Take::Ask(ask)) if !c.wanted.is_empty() => {
                    let c2 = c.clone();
                    view! { <button class="btn ghost sm" on:click=move |_| ask.set(Some(c2.clone()))>{tr("dns.take_over_btn")}</button> }.into_any()
                }
                _ => ().into_any(),
            };
            let mut cells = vec![
                ui::cell("dns.col.name", "wrap", dotted(&c.name)),
                ui::text_cell("dns.col.type", "mono", c.rtype.clone()),
                ui::cell("dns.col.action", "wrap", view! { <div class="cell-main">{ui::status(tone, t(k).to_string())}{detail}</div> }.into_any()),
                ui::cell("dns.col.value", "wrap", view! { <div class="cell-main"><span class="mono">{value}</span>{now.map(|n| view! { <span class="cell-sub small mono">{n}</span> })}</div> }.into_any()),
            ];
            if acts {
                cells.push(ui::actions(action));
            }
            ui::row(cells)
        })
        .collect::<Vec<_>>();
    view! {
        {(!manual.is_empty()).then(|| view! {
            {ui::note(Tone::Warn, icons::INFO, ui::text(t("dom.rec.manual_text").to_string()))}
            <div class="sec-actions">{copy_button("dns.copy_all", all_manual)}</div>
        })}
        {ui::table("wa-routes", if acts { &["dns.col.name", "dns.col.type", "dns.col.action", "dns.col.value", ""] } else { &["dns.col.name", "dns.col.type", "dns.col.action", "dns.col.value"] }, rows)}
    }
    .into_any()
}

// ------------------------------------------------------------------ certificates

fn cert_rows(list: &[CertEntry]) -> Vec<AnyView> {
    list.iter()
        .map(|e| {
            let (tone, k) = cert_status(e.status);
            let until = e.certificate.as_ref().map(|c| i18n::datetime(c.not_after_ms.max(0) as u64)).unwrap_or_else(|| "—".into());
            let renew = e.renew_after_ms.map(|ms| i18n::datetime(ms.max(0) as u64)).unwrap_or_else(|| "—".into());
            let err = e.last_error.clone().map(|m| view! { <span class="cell-sub small" lang="en">{m}</span> });
            ui::row(vec![
                ui::cell("cert.col.cert", "wrap", view! {
                    <div class="cell-main">
                        <span><strong>{tr(cert_kind(e.kind))}</strong>{e.staging.then(|| view! { " "<span class="chip">{tr("cert.staging")}</span> })}</span>
                        {ui::breakable(&e.names.join(", "), "mono small")}
                    </div>
                }.into_any()),
                ui::cell("col.status", "wrap", view! { <div class="cell-main">{ui::status(tone, t(k).to_string())}{err}</div> }.into_any()),
                ui::text_cell("tls.valid_until", "", until),
                ui::text_cell("cert.col.renew", "", renew),
            ])
        })
        .collect()
}

const CERT_HEADS: &[&str] = &["cert.col.cert", "col.status", "tls.valid_until", "cert.col.renew"];

/// Every certificate rynas serves (`GET /web-access/certificates`), for the Certificate tab.
pub fn inventory(inv: Remote<CertificateInventory>) -> AnyView {
    let content = move || {
        let Some(i) = inv.data.get() else { return ().into_any() };
        view! {
            <p class="small muted wa-help">{tr("cert.inv.text")}</p>
            {ui::table("wa-routes", CERT_HEADS, cert_rows(&i.certificates))}
            <p class="small muted">{tf("cert.inv.at", &[("when", &i18n::datetime(i.at_ms.max(0) as u64))])}</p>
        }
        .into_any()
    };
    view! {
        <div class="grid">
            <section class="card widget w-12">
                <WidgetHead icon=icons::LIST_CHECKS title="cert.inv.title">
                    <button class="btn ghost sm" on:click=move |_| inv.reload()><Icon icon=icons::REFRESH_CW size=14 />{tr("cfg.refresh")}</button>
                </WidgetHead>
                {move || inv.view(5, content)}
            </section>
        </div>
    }
    .into_any()
}

// ------------------------------------------------------------------ the domain drawer

/// Saves the configuration after `edit`, then reloads.
fn save_with(edit: impl FnOnce(&mut Value) + 'static, busy: RwSignal<bool>, error: RwSignal<Option<String>>, done: Callback<()>) {
    let Some(mut cfg) = current() else { return };
    edit(&mut cfg);
    busy.set(true);
    error.set(None);
    spawn_local(async move {
        let ok = save_config(cfg, error).await;
        let _ = busy.try_set(false);
        wa().state.reload();
        if ok {
            done.run(());
        }
    });
}

fn domain_drawer(id: String, run_now: bool) -> AnyView {
    let d = dom();
    let close = Callback::new(move |_| d.open.set(None));
    if id.is_empty() {
        let title = Signal::derive(|| t("dom.add").to_string());
        let foot = view! { <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("cancel")}</button> }.into_any();
        let saved = Callback::new(move |nid: String| d.open.set(Some((nid, false))));
        return ui::drawer(title, close, domain_form(String::new(), saved), foot);
    }
    let w = wa();
    let did = StoredValue::new(id.clone());
    let view_of = Memo::new(move |_| w.state.data.with(|s| s.as_ref().and_then(|s| s.domains.iter().find(|v| v.id == did.get_value()).cloned())));
    let report = RwSignal::new(None::<DnsSyncReport>);
    let writing = RwSignal::new(false);
    let write_err = RwSignal::new(None::<String>);
    let write = move || {
        writing.set(true);
        write_err.set(None);
        let url = format!("/api/v1/web-access/domains/{}/records", api::encode(&did.get_value()));
        spawn_local(async move {
            let r = api::post::<(), DnsSyncReport>(&url, None).await;
            let _ = writing.try_set(false);
            match r {
                Ok(rep) => {
                    let _ = report.try_set(Some(rep));
                    w.state.reload();
                }
                Err(e) => {
                    let _ = write_err.try_set(Some(e.text()));
                }
            }
        });
    };
    let diag = RwSignal::new(None::<RouteDiagnostics>);
    let testing = RwSignal::new(false);
    let test_err = RwSignal::new(None::<String>);
    let test = move || {
        testing.set(true);
        test_err.set(None);
        let url = format!("/api/v1/web-access/domains/{}/test", api::encode(&did.get_value()));
        spawn_local(async move {
            let r = api::post::<(), RouteDiagnostics>(&url, None).await;
            let _ = testing.try_set(false);
            match r {
                Ok(x) => {
                    let _ = diag.try_set(Some(x));
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
    // taking over a name, switching staging off, removing the domain: each asks first
    let ask_take = RwSignal::new(None::<DnsChange>);
    let ask_remove = RwSignal::new(false);
    let ask_busy = RwSignal::new(false);
    let ask_err = RwSignal::new(None::<String>);
    let take = Callback::new(move |_| {
        let Some(c) = ask_take.get_untracked() else { return };
        let id = did.get_value();
        save_with(
            move |cfg| {
                if let Some(dm) = list_mut(cfg, "domains").iter_mut().find(|x| x.get("id").and_then(Value::as_str) == Some(id.as_str())) {
                    let mut names = str_list(dm, "take_over");
                    if !names.contains(&c.name) {
                        names.push(c.name.clone());
                    }
                    put(dm, "take_over", json!(names));
                }
            },
            ask_busy,
            ask_err,
            Callback::new(move |_| ask_take.set(None)),
        );
    });
    let remove = Callback::new(move |_| {
        let id = did.get_value();
        save_with(move |cfg| list_mut(cfg, "domains").retain(|x| x.get("id").and_then(Value::as_str) != Some(id.as_str())), ask_busy, ask_err, Callback::new(move |_| d.open.set(None)));
    });
    let fix_busy = RwSignal::new(false);
    let fix_err = RwSignal::new(None::<String>);
    let no_staging = move |_| {
        save_with(
            |cfg| {
                let mut acme = cfg.get("acme").cloned().unwrap_or(Value::Null);
                put(&mut acme, "staging", Value::Bool(false));
                put(cfg, "acme", acme);
            },
            fix_busy,
            fix_err,
            Callback::new(|_| ()),
        )
    };
    let status = move || {
        let Some(v) = view_of.get() else { return ui::note(Tone::Info, icons::INFO, ui::text(t("dom.gone").to_string())) };
        let cfg = current().and_then(|c| by_id(list(&c, "domains"), &v.id).cloned()).unwrap_or(Value::Null);
        let cred = s_at(&cfg, "/credential", "");
        let problems = v
            .problems
            .iter()
            .map(|p| {
                let (tone, text) = problem(p);
                let cred = cred.clone();
                let act = match p.as_str() {
                    "credential_missing" if !cred.is_empty() => Some(view! { <button class="btn ghost sm" on:click=move |_| d.cred.set(Some(cred.clone()))><Icon icon=icons::KEY size=14 />{tr("dom.fix.enter_values")}</button> }.into_any()),
                    "staging" => Some(view! { <button class="btn ghost sm" disabled=move || fix_busy.get() on:click=no_staging>{spinner(fix_busy)}<Icon icon=icons::SHIELD_CHECK size=14 />{tr("dom.fix.no_staging")}</button> }.into_any()),
                    _ => None,
                };
                view! {
                    {ui::note(tone, if tone == Tone::Err { icons::CIRCLE_X } else { icons::TRIANGLE_ALERT }, ui::text(text))}
                    {act.map(|a| view! { <div class="sec-actions">{a}</div> })}
                }
            })
            .collect_view();
        let certs = d.certs.data.with(|c| c.as_ref().map(|c| c.certificates.iter().filter(|e| e.domain.as_deref() == Some(v.name.as_str())).cloned().collect::<Vec<_>>())).unwrap_or_default();
        let lan_record = b_at(&cfg, "/lan_record", true);
        let records = report.get().or_else(|| v.records.clone());
        view! {
            {problems}
            {form_error(fix_err)}
            <dl class="kvs">
                {kv("dom.ui_urls", url_links(&v.ui_urls))}
                {kv("dom.cert_names", ui::breakable(&v.certificate_names.join(", "), "mono"))}
                {kv("dom.provider", ui::text(brand(&s_at(&cfg, "/provider", ""))))}
                {kv("dom.challenge", ui::text(t(challenge_key(&s_at(&cfg, "/challenge", "dns01"))).to_string()))}
            </dl>
            <h3 class="mini-title">{tr("dom.col.records")}</h3>
            {if lan_record {
                view! {
                    <p class="small muted">{tr("dom.rec.text")}</p>
                    {match records {
                        Some(r) => view! {
                            {records_table(r.changes.clone(), Take::Ask(ask_take))}
                            <p class="small muted">{tf("dom.rec.at", &[("when", &i18n::datetime(r.at_ms.max(0) as u64))])}</p>
                        }.into_any(),
                        None => view! { <p class="small muted">{tr("dom.rec.not_yet_text")}</p> }.into_any(),
                    }}
                    {form_error(write_err)}
                    <div class="sec-actions">
                        <button class="btn ghost sm" disabled=move || writing.get() on:click=move |_| write()>{spinner(writing)}<Icon icon=icons::REFRESH_CW size=14 />{tr("dom.rec.write")}</button>
                    </div>
                }.into_any()
            } else {
                view! { <p class="small muted">{tr("dom.rec.off_text")}</p> }.into_any()
            }}
            <h3 class="mini-title">{tr("dom.cert.title")}</h3>
            {if certs.is_empty() {
                view! { <p class="small muted">{tr("dom.cert.none")}</p> }.into_any()
            } else {
                ui::table("wa-routes", CERT_HEADS, cert_rows(&certs))
            }}
            <h3 class="mini-title">{tr("wa.test.title")}</h3>
            <p class="small muted">{tr("dom.test.text")}</p>
            <div class="sec-actions">
                <button class="btn ghost sm" disabled=move || testing.get() on:click=move |_| test()>{spinner(testing)}<Icon icon=icons::LIST_CHECKS size=14 />{tr("wa.route.test")}</button>
            </div>
            {form_error(test_err)}
            {move || diag.with(|x| x.as_ref().map(|x| super::checklist(x, check_key)))}
        }
        .into_any()
    };
    let saved = Callback::new(|_: String| ());
    let body = view! {
        {status}
        <details class="tls-step">
            <summary>{tr("dom.settings")}</summary>
            {domain_form(id, saved)}
        </details>
        <div class="sec-actions">
            <button class="btn danger sm" on:click=move |_| { ask_err.set(None); ask_remove.set(true); }><Icon icon=icons::TRASH_2 size=14 />{tr("dom.remove")}</button>
        </div>
        {move || ask_take.get().map(|c| {
            let text = tf("dom.take.text", &[("name", &c.name), ("now", &c.current.join(", ")), ("wanted", &c.wanted.join(", "))]);
            ui::confirm(Confirm { busy: ask_busy, error: ask_err, ..Confirm::new(tf("dom.take.title", &[("name", &c.name)]), text, "dns.take_over_btn", false, take, Callback::new(move |_| ask_take.set(None))) })
        })}
        {move || ask_remove.get().then(|| ui::confirm(Confirm { busy: ask_busy, error: ask_err, ..Confirm::new(t("dom.remove").to_string(), t("dom.remove_text").to_string(), "dom.remove_go", true, remove, Callback::new(move |_| ask_remove.set(false))) }))}
    }
    .into_any();
    let title = Signal::derive(move || view_of.with(|v| v.as_ref().map(|v| v.name.clone())).unwrap_or_default());
    let foot = view! { <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("close")}</button> }.into_any();
    ui::drawer(title, close, body, foot)
}

// ------------------------------------------------------------------ the domain form

#[derive(Clone, PartialEq)]
struct Draft {
    name: String,
    provider: String,
    /// Credential id; empty: a new one.
    cred: String,
    label: String,
    /// New values for the chosen existing credential.
    replace: bool,
    challenge: String,
    delegate_to: String,
    wildcard: bool,
    lan_record: bool,
    lan_ipv6: bool,
    ui: bool,
    enabled: bool,
}

fn draft_of(cfg: &Value, id: &str) -> (Draft, Vec<String>) {
    let dm = by_id(list(cfg, "domains"), id).cloned().unwrap_or(Value::Null);
    let provider = s_at(&dm, "/provider", "cloudflare");
    // a new domain reuses the first credential of its provider
    let cred = dm.get("credential").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| {
        list(cfg, "credentials").iter().find(|c| c.get("provider").and_then(Value::as_str) == Some(provider.as_str())).map(|c| s_at(c, "/id", "")).unwrap_or_default()
    });
    let draft = Draft {
        name: s_at(&dm, "/name", ""),
        provider,
        cred,
        label: String::new(),
        replace: false,
        challenge: s_at(&dm, "/challenge", "dns01"),
        delegate_to: s_at(&dm, "/delegate_to", ""),
        wildcard: b_at(&dm, "/wildcard", true),
        lan_record: b_at(&dm, "/lan_record", true),
        lan_ipv6: b_at(&dm, "/lan_ipv6", false),
        ui: b_at(&dm, "/ui", true),
        enabled: b_at(&dm, "/enabled", true),
    };
    (draft, str_list(&dm, "take_over"))
}

/// The configuration with the draft in it, and the domain's id.
fn with_draft(mut cfg: Value, id: &str, d: &Draft, values: &[(String, String)], take_over: &[String]) -> (Value, String) {
    let secrets = filled(values);
    let creds = list_mut(&mut cfg, "credentials");
    let mut cred = d.cred.clone();
    if cred.is_empty() {
        cred = free_id(&d.provider, &ids(creds));
        let label = if d.label.trim().is_empty() { d.name.trim().to_string() } else { d.label.trim().to_string() };
        creds.push(json!({"id": cred, "provider": d.provider, "label": label, "secrets": secrets}));
    } else if d.replace
        && !secrets.is_empty()
        && let Some(c) = creds.iter_mut().find(|c| c.get("id").and_then(Value::as_str) == Some(cred.as_str()))
    {
        put(c, "secrets", Value::Object(secrets));
    }
    let domains = list_mut(&mut cfg, "domains");
    let id = if id.is_empty() { free_id(d.name.trim().split('.').next().unwrap_or(""), &ids(domains)) } else { id.to_string() };
    let http = d.challenge == "http01";
    let mut obj = by_id(domains, &id).cloned().unwrap_or_else(|| json!({}));
    put(&mut obj, "id", Value::String(id.clone()));
    put(&mut obj, "name", Value::String(d.name.trim().trim_end_matches('.').to_ascii_lowercase()));
    put(&mut obj, "provider", Value::String(d.provider.clone()));
    put(&mut obj, "credential", Value::String(cred));
    put(&mut obj, "challenge", Value::String(d.challenge.clone()));
    if d.challenge == "cname_delegation" {
        put(&mut obj, "delegate_to", Value::String(d.delegate_to.trim().to_string()));
    } else {
        drop_key(&mut obj, "delegate_to");
    }
    put(&mut obj, "wildcard", Value::Bool(d.wildcard && !http));
    put(&mut obj, "lan_record", Value::Bool(d.lan_record));
    put(&mut obj, "lan_ipv6", Value::Bool(d.lan_record && d.lan_ipv6));
    put(&mut obj, "ui", Value::Bool(d.ui));
    put(&mut obj, "enabled", Value::Bool(d.enabled));
    put(&mut obj, "take_over", json!(take_over));
    match domains.iter_mut().find(|x| x.get("id").and_then(Value::as_str) == Some(id.as_str())) {
        Some(x) => *x = obj,
        None => domains.push(obj),
    }
    (cfg, id)
}

/// The response of `POST /web-access/plan`, only the records.
#[derive(serde::Deserialize)]
struct PlanDns {
    #[serde(default)]
    dns_changes: Vec<DnsChange>,
}

fn step(n: &'static str, title: &'static str) -> AnyView {
    view! { <h3 class="mini-title"><span class="chip accent">{n}</span>" "{tr(title)}</h3> }.into_any()
}

fn domain_form(id: String, saved: Callback<String>) -> AnyView {
    let d = dom();
    let w = wa();
    let cfg0 = current().unwrap_or(Value::Null);
    let (first, take0) = draft_of(&cfg0, &id);
    let is_new = id.is_empty();
    let did = StoredValue::new(id);
    let draft = RwSignal::new(first);
    let take_over = RwSignal::new(take0);
    let values = RwSignal::new(Vec::<(String, String)>::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let name_err = RwSignal::new(None::<String>);
    let deleg_err = RwSignal::new(None::<String>);
    let sec_err = RwSignal::new(None::<String>);
    let plan = RwSignal::new(None::<Vec<DnsChange>>);
    let planning = RwSignal::new(false);
    let plan_err = RwSignal::new(None::<String>);
    let provider = RwSignal::new(draft.get_untracked().provider);
    let cred = RwSignal::new(draft.get_untracked().cred);
    // a provider change picks its first credential (or a new one) and forgets typed values
    Effect::new(move |prev: Option<String>| {
        let p = provider.get();
        if prev.is_some_and(|x| x != p) {
            let c = current().map(|c| list(&c, "credentials").iter().find(|x| x.get("provider").and_then(Value::as_str) == Some(p.as_str())).map(|x| s_at(x, "/id", "")).unwrap_or_default()).unwrap_or_default();
            cred.set(c);
            values.set(Vec::new());
            plan.set(None);
        }
        draft.update(|x| x.provider = p.clone());
        p
    });
    Effect::new(move || {
        let c = cred.get();
        draft.update(|x| x.cred = c);
    });
    let candidate = move || {
        let cfg = current()?;
        Some(with_draft(cfg, &did.get_value(), &draft.get_untracked(), &values.get_untracked(), &take_over.get_untracked()))
    };
    let route_err = move |e: api::ApiErr| {
        match e.field() {
            Some(f) if f.starts_with("domains") && f.ends_with(".name") => name_err.set(Some(e.text())),
            Some(f) if f.ends_with(".delegate_to") => deleg_err.set(Some(e.text())),
            Some(f) if f.starts_with("credentials") || f.ends_with(".credential") => sec_err.set(Some(e.text())),
            _ => return Some(e.text()),
        }
        None
    };
    let preview = move |_| {
        let Some((cfg, _)) = candidate() else { return };
        planning.set(true);
        plan_err.set(None);
        spawn_local(async move {
            let r = api::post::<_, PlanDns>("/api/v1/web-access/plan", Some(&cfg)).await;
            let _ = planning.try_set(false);
            match r {
                Ok(p) => {
                    let name = draft.with_untracked(|x| x.name.trim().trim_end_matches('.').to_ascii_lowercase());
                    let _ = plan.try_set(Some(p.dns_changes.into_iter().filter(|c| c.domain == name).collect()));
                }
                Err(e) => {
                    if let Some(m) = route_err(e) {
                        let _ = plan_err.try_set(Some(m));
                    }
                }
            }
        });
    };
    let save = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let Some((cfg, nid)) = candidate() else { return };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let ok = match api::put::<_, Value>(super::URL, &cfg).await {
                Ok(_) => crate::config::apply_now(&[crate::config::keys::WEB_ACCESS], error).await,
                Err(e) => {
                    if let Some(m) = route_err(e) {
                        error.set(Some(m));
                    }
                    false
                }
            };
            let _ = busy.try_set(false);
            w.state.reload();
            d.certs.reload();
            if ok {
                crate::toast::saved_applied();
                let _ = values.try_set(Vec::new());
                saved.run(nid);
            }
        });
    };
    let (name, set_name) = crate::form::lens(draft, |x| x.name.clone(), |x, v| x.name = v);
    let (deleg, set_deleg) = crate::form::lens(draft, |x| x.delegate_to.clone(), |x, v| x.delegate_to = v);
    let (label, set_label) = crate::form::lens(draft, |x| x.label.clone(), |x, v| x.label = v);
    let challenge = Signal::derive(move || draft.with(|x| x.challenge.clone()));
    let bool_switch = move |key: &'static str, help: &'static str, get: fn(&Draft) -> bool, set: fn(&mut Draft, bool)| {
        let (v, s) = crate::form::lens_bool(draft, get, set);
        switch(key, Some(help), v, s)
    };
    let provider_part = move || {
        let Some(list) = d.providers.data.get() else { return d.providers.view(3, || ().into_any()) };
        let list: Vec<DnsProviderInfo> = list.into_iter().filter(|p| p.provider.as_str() != "acmedns" || challenge.get() == "cname_delegation").collect();
        view! {
            {provider_select("dom.provider", list, provider, false)}
            {move || provider_info(d, &provider.get()).map(|i| view! {
                <div class="chips">
                    <span class="chip">{tr(if i.records { "dom.p.records" } else { "dom.p.manual" })}</span>
                    {(!i.wildcard_records).then(|| view! { <span class="chip">{tr("dom.p.per_name")}</span> })}
                </div>
                {(!i.scoped_tokens).then(|| ui::note(Tone::Warn, icons::SHIELD_ALERT, ui::text(t("dom.p.unscoped").to_string())))}
                {(i.provider.as_str() == "duckdns").then(|| ui::note(Tone::Warn, icons::INFO, ui::text(t("dom.p.duckdns").to_string())))}
            })}
        }
        .into_any()
    };
    let cred_part = move || {
        let p = provider.get();
        let Some(info) = provider_info(d, &p) else { return ().into_any() };
        let creds: Vec<(String, String, Vec<String>)> = current()
            .map(|c| list(&c, "credentials").iter().filter(|x| x.get("provider").and_then(Value::as_str) == Some(p.as_str())).map(|x| (s_at(x, "/id", ""), s_at(x, "/label", ""), str_list(x, "fields"))).collect())
            .unwrap_or_default();
        let sid = crate::form::fid();
        let stored = creds.iter().find(|c| c.0 == cred.get()).map(|c| c.2.clone());
        let options = creds
            .iter()
            .map(|(cid, cl, _)| {
                let v = cid.clone();
                let text = if cl.is_empty() { cid.clone() } else { cl.clone() };
                view! { <option value=v.clone() selected=move || cred.with(|c| *c == v)>{text}</option> }
            })
            .collect_view();
        let replace = Signal::derive(move || draft.with(|x| x.replace));
        view! {
            {(!creds.is_empty()).then(|| view! {
                <div class="field">
                    <label class="field-label" for=sid.clone()>{tr("dom.cred.pick")}</label>
                    <select id=sid class="input" on:change=move |ev| { cred.set(event_target_value(&ev)); values.set(Vec::new()); }>
                        {options}
                        <option value="" selected=move || cred.with(String::is_empty)>{tr("dom.cred.new")}</option>
                    </select>
                </div>
            })}
            {match stored {
                Some(fields) => view! {
                    {ui::note(Tone::Info, icons::LOCK, ui::text(if fields.is_empty() { t("dom.cred.no_values").to_string() } else { tf("dom.cred.stored", &[("fields", &fields.iter().map(|f| field_label(f)).collect::<Vec<_>>().join(", "))]) }))}
                    {switch("dom.cred.replace", None, replace, Callback::new(move |v| draft.update(|x| x.replace = v)))}
                    <Show when=move || replace.get()>{secret_inputs(info.clone(), values, sec_err)}</Show>
                }.into_any(),
                None => view! {
                    {text_field("dom.cred.label", label, set_label, Signal::stored(None), In { help: Some("dom.cred.label_help"), ..In::default() })}
                    {secret_inputs(info, values, sec_err)}
                }.into_any(),
            }}
        }
        .into_any()
    };
    let ch_help = move || t(match challenge.get().as_str() {
        "http01" => "dom.ch.http01_help",
        "cname_delegation" => "dom.ch.cname_delegation_help",
        _ => "dom.ch.dns01_help",
    });
    let plan_view = move || {
        plan.get().map(|changes| {
            let conflicts = changes.iter().any(|c| c.action == DnsAction::Conflict && !c.wanted.is_empty());
            view! {
                {conflicts.then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(t("dom.plan.conflict").to_string())))}
                {records_table(changes, Take::Toggle(take_over))}
            }
        })
    };
    view! {
        <form class="form" on:submit=save>
            {is_new.then(|| view! { <p class="small muted wa-help">{tr("dom.form.intro")}</p> })}
            {step("1", "dom.provider")}
            {provider_part}
            {step("2", "dom.step.token")}
            {cred_part}
            {step("3", "dom.step.domain")}
            {text_field("dom.name", name, Callback::new(move |v| { name_err.set(None); plan.set(None); set_name.run(v); }), name_err.into(), In { placeholder: "nas.example.com", help: Some("dom.name_help"), mono: true, maxlength: 212, ..In::default() })}
            {crate::form::segmented("dom.challenge", &[("dns01", "dom.ch.dns01"), ("cname_delegation", "dom.ch.cname_delegation"), ("http01", "dom.ch.http01")], challenge, Callback::new(move |v| { plan.set(None); draft.update(|x| x.challenge = v); }))}
            <p class="small muted wa-help">{ch_help}</p>
            <Show when=move || challenge.get() == "cname_delegation">
                {text_field("dom.delegate_to", deleg, Callback::new(move |v| { deleg_err.set(None); set_deleg.run(v); }), deleg_err.into(), In { placeholder: "nas-example.acme.example.net", help: Some("dom.delegate_to_help"), mono: true, ..In::default() })}
            </Show>
            <Show when=move || challenge.get() != "http01">
                {bool_switch("dom.wildcard", "dom.wildcard_help", |x| x.wildcard, |x, v| x.wildcard = v)}
            </Show>
            {bool_switch("dom.lan_record", "dom.lan_record_help", |x| x.lan_record, |x, v| x.lan_record = v)}
            <Show when=move || draft.with(|x| x.lan_record)>
                {bool_switch("dom.lan_ipv6", "dom.lan_ipv6_help", |x| x.lan_ipv6, |x, v| x.lan_ipv6 = v)}
            </Show>
            {bool_switch("dom.ui", "dom.ui_help", |x| x.ui, |x, v| x.ui = v)}
            {bool_switch("dom.enabled", "dom.enabled_help", |x| x.enabled, |x, v| x.enabled = v)}
            {step("4", "dom.step.records")}
            <p class="small muted wa-help">{tr("dom.plan.text")}</p>
            <div class="sec-actions">
                <button type="button" class="btn ghost sm" disabled=move || planning.get() || draft.with(|x| x.name.trim().is_empty()) on:click=preview>{spinner(planning)}<Icon icon=icons::FILE_DIFF size=14 />{tr("dom.plan.go")}</button>
            </div>
            {form_error(plan_err)}
            {plan_view}
            {form_error(error)}
            <div class="sec-actions">
                <button type="submit" class="btn primary" disabled=move || busy.get() || draft.with(|x| x.name.trim().is_empty())>{spinner(busy)}{tr(if is_new { "dom.add_go" } else { "save" })}</button>
            </div>
        </form>
    }
    .into_any()
}
