//! The HTTPS certificate of the interface (W0, `docs/phase1/plan.md` "API v1: web TLS"):
//! the active certificate with its expiry, "Trust this NAS" (the local CA and short steps per
//! system), the import of an own certificate and the way back to the local one.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::tls::{TlsCertificate, TlsImportRequest, TlsSource, TlsStatus, TlsWarning};
use wasm_bindgen::JsCast;

use crate::api;
use crate::form::{SaveBar, form_error, save_bar_with};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::toast::toast;
use crate::ui::{self, Confirm, DlgKind, Remote, Tone, WidgetHead};

const URL: &str = "/api/v1/system/tls";
const DAY_MS: i64 = 86_400_000;

pub(super) fn warning_key(w: TlsWarning) -> &'static str {
    match w {
        TlsWarning::ExpiresSoon => "tls.warn.expires_soon",
        TlsWarning::Expired => "tls.warn.expired",
        TlsWarning::NamesNotCovered => "tls.warn.names_not_covered",
        TlsWarning::ChainIncomplete => "tls.warn.chain_incomplete",
    }
}


fn mono(s: &str) -> AnyView {
    ui::breakable(s, "mono")
}

pub(super) fn cert_list(c: &TlsCertificate) -> AnyView {
    let days = (c.not_after_ms - js_sys::Date::now() as i64) / DAY_MS;
    let tone = if days < 0 { Tone::Err } else if days < 30 { Tone::Warn } else { Tone::Ok };
    view! {
        <dl class="kvs">
            {ui::kv("tls.subject", mono(&c.subject))}
            {(!c.dns_names.is_empty()).then(|| ui::kv("tls.names", mono(&c.dns_names.join(", "))))}
            {(!c.ip_addresses.is_empty()).then(|| ui::kv("tls.addresses", mono(&c.ip_addresses.join(", "))))}
            {ui::kv("tls.issuer", mono(&c.issuer))}
            {ui::kv("tls.valid_from", ui::text(i18n::datetime(c.not_before_ms.max(0) as u64)))}
            {ui::kv("tls.valid_until", view! {
                <span>{i18n::datetime(c.not_after_ms.max(0) as u64)}" "</span>
                {ui::status(tone, tf("tls.days_left", &[("n", &days.max(0).to_string())]))}
            }.into_any())}
            {ui::kv("tls.key_type", ui::text(c.key_type.clone()))}
            {ui::kv("tls.fingerprint", mono(&c.fingerprint_sha256))}
            {ui::kv("sto.serial", mono(&c.serial))}
        </dl>
    }
    .into_any()
}

pub fn panel() -> AnyView {
    let st = Remote::<TlsStatus>::new(URL);
    let importing = RwSignal::new(false);
    let ask_local = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let to_local = Callback::new(move |_| {
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = api::post::<(), TlsStatus>(&format!("{URL}/local"), None).await;
            busy.set(false);
            match r {
                Ok(s) => {
                    st.data.set(Some(s));
                    ask_local.set(false);
                    toast(Tone::Ok, t("tls.reload_note").to_string());
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    });
    let active = move || {
        st.view(9, move || {
            let Some(s) = st.data.get() else { return ().into_any() };
            let imported = s.source == TlsSource::Imported;
            let warnings = s.warnings.iter().map(|w| ui::note(if *w == TlsWarning::Expired { Tone::Err } else { Tone::Warn }, icons::TRIANGLE_ALERT, ui::text(t(warning_key(*w)).to_string()))).collect_view();
            let chain = s.chain.iter().map(|c| view! { <li class="mono small">{c.subject.clone()}</li> }).collect_view();
            view! {
                <div class="chips">
                    <span class="chip accent"><Icon icon=icons::LOCK size=12 />{tr(if imported { "tls.source.imported" } else { "tls.source.local" })}</span>
                    <span class="chip">{move || tf("tls.https_port", &[("port", &super::emergency_port().to_string())])}</span>
                </div>
                {warnings}
                {(!s.missing_names.is_empty()).then(|| ui::note(Tone::Warn, icons::INFO, view! { {tr("tls.missing_names")}" "{mono(&s.missing_names.join(", "))} }.into_any()))}
                {cert_list(&s.certificate)}
                {(!s.chain.is_empty()).then(|| view! { <h3 class="mini-title">{tr("tls.chain")}</h3><ul class="app-list">{chain}</ul> })}
                <div class="sec-actions">
                    <button class="btn ghost sm" on:click=move |_| importing.set(true)><Icon icon=icons::IMPORT size=14 />{tr("tls.import")}</button>
                    {imported.then(|| view! { <button class="btn ghost sm" on:click=move |_| { error.set(None); ask_local.set(true); }><Icon icon=icons::ROTATE_CCW size=14 />{tr("tls.use_local")}</button> })}
                </div>
            }
            .into_any()
        })
    };
    let trust = move || {
        st.view(4, move || {
            let Some(ca) = st.data.with(|s| s.as_ref().and_then(|s| s.local_ca.clone())) else {
                return ui::empty_line("tls.no_ca");
            };
            view! {
                <p class="small">{tr("tls.trust_help")}</p>
                <dl class="kvs">
                    {ui::kv("tls.subject", mono(&ca.subject))}
                    {ui::kv("tls.fingerprint", mono(&ca.fingerprint_sha256))}
                    {ui::kv("tls.valid_until", ui::text(i18n::datetime(ca.not_after_ms.max(0) as u64)))}
                </dl>
                <div class="sec-actions">
                    <a class="btn primary sm" href=format!("{URL}/ca") download="rynas-local-ca.crt"><Icon icon=icons::DOWNLOAD size=14 />{tr("tls.download_ca")}</a>
                </div>
                <div class="tls-os">
                    {os_steps("tls.os.windows", "tls.trust.windows")}
                    {os_steps("tls.os.macos", "tls.trust.macos")}
                    {os_steps("tls.os.ios", "tls.trust.ios")}
                    {os_steps("tls.os.android", "tls.trust.android")}
                    {os_steps("tls.os.linux", "tls.trust.linux")}
                    {os_steps("tls.os.firefox", "tls.trust.firefox")}
                </div>
                <p class="small muted">{tr("tls.trust_check")}</p>
            }
            .into_any()
        })
    };
    view! {
        <div class="grid">
            <section class="card widget w-6">
                <WidgetHead icon=icons::LOCK title="tls.active" />
                {active}
            </section>
            <section class="card widget w-6">
                <WidgetHead icon=icons::SHIELD_CHECK title="tls.local_ca" />
                {trust}
            </section>
        </div>
        {move || importing.get().then(|| import_dialog(st, Callback::new(move |_| importing.set(false))))}
        {move || ask_local.get().then(|| ui::confirm(Confirm { busy, error, ..Confirm::new(t("tls.use_local_title").to_string(), t("tls.use_local.confirm").to_string(), "tls.use_local", false, to_local, Callback::new(move |_| ask_local.set(false))) }))}
    }
    .into_any()
}

/// One system's steps, folded.
fn os_steps(title: &'static str, steps: &'static str) -> AnyView {
    view! {
        {ui::fold(None, title, false, None, view! { 
            <ol class="small app-list">{move || t(steps).split(" | ").map(|s| view! { <li>{s.to_string()}</li> }).collect_view()}</ol>
         }.into_any())}
    }
    .into_any()
}

/// A PEM text area with "Load from a file".
pub(super) fn pem_field(label: &'static str, value: RwSignal<String>, err: RwSignal<Option<String>>, accept: &'static str) -> AnyView {
    let id = crate::form::fid();
    let fid = format!("{id}f");
    let load = move |ev: leptos::ev::Event| {
        let Some(input) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()) else { return };
        let Some(file) = input.files().and_then(|f| f.get(0)) else { return };
        let p = file.text();
        spawn_local(async move {
            if let Ok(v) = wasm_bindgen_futures::JsFuture::from(p).await
                && let Some(s) = v.as_string()
            {
                value.set(s);
                err.set(None);
            }
        });
    };
    view! {
        <div class="field">
            <label class="field-label" for=id.clone()>{tr(label)}</label>
            <textarea id=id class="input area mono tls-pem" rows="6" spellcheck="false" placeholder="-----BEGIN …-----"
                aria-invalid=move || err.with(Option::is_some).then_some("true")
                prop:value=move || value.get() on:input=move |ev| { value.set(event_target_value(&ev)); err.set(None); }></textarea>
            <label class="btn ghost sm tls-file" for=fid.clone()><Icon icon=icons::FILE_INPUT size=14 />{tr("load_file")}</label>
            <input id=fid class="sr-only" type="file" accept=accept on:change=load />
            <span class="field-error" aria-live="polite">{move || err.get().map(|e| view! { <Icon icon=icons::CIRCLE_X size=12 />{ui::err_view(e)} })}</span>
        </div>
    }
    .into_any()
}

fn import_dialog(st: Remote<TlsStatus>, close: Callback<()>) -> AnyView {
    let cert = RwSignal::new(String::new());
    let key = RwSignal::new(String::new());
    let cert_err = RwSignal::new(None::<String>);
    let key_err = RwSignal::new(None::<String>);
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let go = move |_| {
        let body = TlsImportRequest { certificate: cert.get_untracked(), private_key: key.get_untracked() };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = api::post::<_, TlsStatus>(&format!("{URL}/import"), Some(&body)).await;
            busy.set(false);
            match r {
                Ok(s) => {
                    st.data.set(Some(s));
                    toast(Tone::Ok, t("tls.reload_note").to_string());
                    close.run(());
                }
                Err(e) => match e.field() {
                    Some("certificate") => cert_err.set(Some(e.text())),
                    Some("private_key") => key_err.set(Some(e.text())),
                    _ => error.set(Some(e.text())),
                },
            }
        });
    };
    let body = view! {
        <p class="small">{tr("tls.import.hint")}</p>
        {pem_field("tls.import.certificate", cert, cert_err, ".crt,.pem,.cer")}
        {pem_field("tls.import.private_key", key, key_err, ".key,.pem")}
        {ui::note(Tone::Info, icons::INFO, ui::text(t("tls.reload_note").to_string()))}
        {form_error(error)}
    }
    .into_any();
    let foot = save_bar_with(SaveBar {
        can_save: Signal::derive(move || !cert.with(|c| c.trim().is_empty()) && !key.with(|k| k.trim().is_empty())),
        icon: Some(icons::IMPORT),
        ..SaveBar::new(busy, "tls.import", close, Callback::new(go))
    });
    ui::dialog(DlgKind::Normal, Signal::derive(|| t("tls.import").to_string()), close, body, Some(foot))
}
