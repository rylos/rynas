//! Imported certificates (`docs/phase2/docker-apps.md` §5.5): a certificate made elsewhere (a
//! router's ACME client, a company CA, a bought wildcard) served by the proxy for the names it
//! covers. Uploaded here, or pushed by another tool into a watched folder.
//!
//! - The card lists each import with what it takes over, its status and expiry.
//! - "Import a certificate": id, description, source; for an upload the PEM chain and key, Check
//!   (`POST /imports/inspect`: subject, issuer, expiry, the addresses it takes over) then Save.
//! - The drawer of an import: details, the addresses it takes over, the last refused files, and
//!   for a watched folder its path with the sshd and acme.sh lines filled in and Check now;
//!   replacing the files; Remove (asks first).
//!
//! These are immediate actions, not part of the staged `web_access` key. The private key is sent
//! once and never comes back.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::tls::TlsWarning;
use rynas_api_types::web_access::{CertImportRequest, ImportSource, ImportedCert};

use super::cert::{cert_list, pem_field, warning_key};
use super::domains::cert_status;
use super::{kv, wa};
use crate::api;
use crate::form::{In, form_error, spinner, text_field};
use crate::i18n::{self, t, tr};
use crate::icons::{self, Icon};
use crate::pages::security::copy_button;
use crate::toast::toast;
use crate::ui::{self, Confirm, DlgKind, Remote, Tone, WidgetHead};

const URL: &str = "/api/v1/web-access/imports";

#[derive(Clone, Copy)]
struct Imp {
    list: Remote<Vec<ImportedCert>>,
    /// Id of the open drawer.
    open: RwSignal<Option<String>>,
    /// The import dialog: `Some("")` a new import, `Some(id)` new files for `id`.
    form: RwSignal<Option<String>>,
    /// After a change: the certificate inventory reads again.
    changed: Callback<()>,
}

fn source_key(s: ImportSource) -> &'static str {
    match s {
        ImportSource::Upload => "wa.import.source.upload",
        ImportSource::Folder => "wa.import.source.folder",
    }
}

fn names(list: &[String]) -> AnyView {
    ui::breakable(&list.join(", "), "mono")
}

fn status_of(c: &ImportedCert) -> AnyView {
    if c.certificate.is_none() && c.source == ImportSource::Folder {
        return ui::status(Tone::Info, t("wa.import.waiting").to_string());
    }
    let (tone, k) = cert_status(c.status);
    ui::status(tone, t(k).to_string())
}

/// The card on the Certificate tab.
pub fn card(changed: Callback<()>) -> AnyView {
    let imp = Imp { list: Remote::new(URL), open: RwSignal::new(None), form: RwSignal::new(None), changed };
    let content = move || {
        let Some(list) = imp.list.data.get() else { return ().into_any() };
        if list.is_empty() {
            return view! {
                <p class="small muted wa-help">{tr("wa.import.empty")}</p>
            }
            .into_any();
        }
        let rows = list
            .iter()
            .map(|c| {
                let id = c.id.clone();
                let title = if c.label.is_empty() { c.id.clone() } else { c.label.clone() };
                let until = c.certificate.as_ref().map(|x| i18n::datetime(x.not_after_ms.max(0) as u64)).unwrap_or_else(|| "—".into());
                let covers = if c.covers.is_empty() { view! { <span class="muted small">{tr("wa.import.covers_none")}</span> }.into_any() } else { names(&c.covers) };
                let err = c.last_error.as_ref().map(|_| view! { <span class="cell-sub small">{tr("wa.import.last_error")}</span> });
                ui::row(vec![
                    ui::cell("wa.import.label", "wrap", ui::two_lines(view! { <strong class="wa-name">{title}</strong> }.into_any(), Some(t(source_key(c.source)).to_string()), "cell-sub small")),
                    ui::cell("wa.import.covers", "wrap", covers),
                    ui::cell("col.status", "wrap", view! { <div class="cell-main">{status_of(c)}{err}</div> }.into_any()),
                    ui::text_cell("tls.valid_until", "", until),
                    ui::actions(view! { <button class="btn ghost sm" on:click=move |_| imp.open.set(Some(id.clone()))><Icon icon=icons::PENCIL size=14 />{tr("dom.open")}</button> }.into_any()),
                ])
            })
            .collect::<Vec<_>>();
        ui::table("wa-routes", &["wa.import.label", "wa.import.covers", "col.status", "tls.valid_until", ""], rows)
    };
    view! {
        <div class="grid">
            <section class="card widget w-12">
                <WidgetHead icon=icons::IMPORT title="wa.import.title">
                    <button class="btn ghost sm" on:click=move |_| imp.form.set(Some(String::new()))><Icon icon=icons::PLUS size=14 />{tr("wa.import.add")}</button>
                </WidgetHead>
                <p class="small muted wa-help">{tr("wa.import.intro")}" "{tr("wa.import.not_renewed")}</p>
                {move || imp.list.view(4, content)}
            </section>
        </div>
        {move || imp.open.get().map(|id| drawer(imp, id))}
        {move || imp.form.get().map(|id| import_dialog(imp, id))}
    }
    .into_any()
}

/// The sshd and acme.sh lines for a watched folder, with its id and this NAS's name.
fn ssh_lines(base: &str, id: &str, host: &str) -> (String, String) {
    let sshd = format!(
        "# adduser --system --ingroup rynas-cert-drop --shell /usr/sbin/nologin certpush\nMatch User certpush\n    ChrootDirectory {base}\n    ForceCommand internal-sftp -d /{id} -u 0007\n    AllowTcpForwarding no\n    X11Forwarding no"
    );
    let acme = format!(
        "DEPLOY_SSH_USER=certpush DEPLOY_SSH_SERVER={host}.local DEPLOY_SSH_USE_SCP=yes DEPLOY_SSH_BACKUP=no DEPLOY_SSH_KEYFILE=/{id}/privkey.pem DEPLOY_SSH_FULLCHAIN=/{id}/fullchain.pem acme.sh --deploy -d <name> --deploy-hook ssh"
    );
    (sshd, acme)
}

fn code_block(text: String) -> AnyView {
    view! {
        <pre class="app-code">{text.clone()}</pre>
        <div class="sec-actions">{copy_button("dns.copy", text)}</div>
    }
    .into_any()
}

fn drawer(imp: Imp, id: String) -> AnyView {
    let close = Callback::new(move |_| imp.open.set(None));
    let iid = StoredValue::new(id);
    let item = Memo::new(move |_| imp.list.data.with(|l| l.as_ref().and_then(|l| l.iter().find(|c| c.id == iid.get_value()).cloned())));
    let checking = RwSignal::new(false);
    let check_err = RwSignal::new(None::<String>);
    let check = move |_| {
        checking.set(true);
        check_err.set(None);
        let url = format!("{URL}/{}/check", api::encode(&iid.get_value()));
        spawn_local(async move {
            let r = api::post::<(), ImportedCert>(&url, None).await;
            let _ = checking.try_set(false);
            match r {
                Ok(_) => {
                    imp.list.reload();
                    imp.changed.run(());
                }
                Err(e) => {
                    let _ = check_err.try_set(Some(e.text()));
                }
            }
        });
    };
    let ask = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let remove = Callback::new(move |_| {
        busy.set(true);
        error.set(None);
        let url = format!("{URL}/{}", api::encode(&iid.get_value()));
        spawn_local(async move {
            let r = api::delete_empty(&url).await;
            let _ = busy.try_set(false);
            match r {
                Ok(()) => {
                    imp.list.reload();
                    imp.changed.run(());
                    imp.open.set(None);
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    });
    let body = move || {
        let Some(c) = item.get() else { return ui::note(Tone::Info, icons::INFO, ui::text(t("dom.gone").to_string())) };
        let warnings = c
            .warnings
            .iter()
            .map(|w| ui::note(if *w == TlsWarning::Expired { Tone::Err } else { Tone::Warn }, icons::TRIANGLE_ALERT, ui::text(t(warning_key(*w)).to_string())))
            .collect_view();
        let last_error = c.last_error.clone().map(|m| {
            let when = c.last_error_at_ms.map(|ms| format!(" ({})", i18n::datetime(ms.max(0) as u64))).unwrap_or_default();
            ui::note(Tone::Err, icons::CIRCLE_X, view! {
                <strong>{tr("wa.import.last_error")}{when}</strong>" "{ui::tech_detail(m)}
                {c.certificate.is_some().then(|| view! { " "<span>{tr("wa.import.kept")}</span> })}
            }.into_any())
        });
        let folder = c.folder.clone().filter(|_| c.source == ImportSource::Folder).map(|f| {
            let base = f.trim_end_matches('/').rsplit_once('/').map(|(b, _)| b.to_string()).unwrap_or_else(|| "/var/lib/rynas-cert-drop".into());
            let host = wa().state.data.with_untracked(|s| s.as_ref().map(|s| s.host.clone())).unwrap_or_default();
            let (sshd, acme) = ssh_lines(&base, &c.id, &host);
            view! {
                <h3 class="mini-title">{tr("wa.import.folder")}</h3>
                <div class="chips"><span class="chip mono wa-name">{f.clone()}</span>{copy_button("dns.copy", f.clone())}</div>
                <p class="small muted">{tr("wa.import.folder_help")}</p>
                {form_error(check_err)}
                <div class="sec-actions">
                    <button class="btn ghost sm" disabled=move || checking.get() on:click=check>{spinner(checking)}<Icon icon=icons::REFRESH_CW size=14 />{tr("wa.import.check_now")}</button>
                </div>
                <details class="tls-step">
                    <summary>{tr("wa.import.folder_ssh")}</summary>
                    <p class="small muted">{tr("wa.import.ssh_note")}</p>
                    {code_block(sshd)}
                    {code_block(acme)}
                </details>
            }
        });
        view! {
            {(c.certificate.is_none() && c.source == ImportSource::Folder).then(|| ui::note(Tone::Info, icons::CLOCK, ui::text(t("wa.import.waiting").to_string())))}
            {warnings}
            {last_error}
            <dl class="kvs">
                {kv("col.status", status_of(&c))}
                {kv("wa.import.label", ui::text(if c.label.is_empty() { "—".to_string() } else { c.label.clone() }))}
                {kv("wa.import.id", ui::breakable(&c.id, "mono"))}
                {c.imported_at_ms.map(|ms| kv("wa.import.imported_at", ui::text(i18n::datetime(ms.max(0) as u64))))}
                {kv("wa.import.covers", if c.covers.is_empty() { ui::text(t("wa.import.covers_none").to_string()) } else { names(&c.covers) })}
                {(!c.shadowed.is_empty()).then(|| kv("wa.import.shadowed", names(&c.shadowed)))}
                {(!c.replaces_acme.is_empty()).then(|| kv("wa.import.replaces_acme", names(&c.replaces_acme)))}
            </dl>
            <p class="small muted">{tr("wa.import.not_renewed")}</p>
            {c.certificate.as_ref().map(cert_list)}
            {folder}
            <div class="sec-actions">
                <button class="btn ghost sm" on:click=move |_| imp.form.set(Some(iid.get_value()))><Icon icon=icons::IMPORT size=14 />{tr("wa.import.replace")}</button>
                <button class="btn danger sm" on:click=move |_| { error.set(None); ask.set(true); }><Icon icon=icons::TRASH_2 size=14 />{tr("wa.import.delete")}</button>
            </div>
        }
        .into_any()
    };
    let title = Signal::derive(move || item.with(|c| c.as_ref().map(|c| if c.label.is_empty() { c.id.clone() } else { c.label.clone() })).unwrap_or_default());
    let foot = view! { <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("close")}</button> }.into_any();
    view! {
        {ui::drawer(title, close, view! { {body} }.into_any(), foot)}
        {move || ask.get().then(|| ui::confirm(Confirm { busy, error, ..Confirm::new(t("wa.import.delete").to_string(), t("wa.import.delete_confirm").to_string(), "wa.import.delete", true, remove, Callback::new(move |_| ask.set(false))) }))}
    }
    .into_any()
}

/// What `inspect` found, before saving.
fn inspect_view(c: &ImportedCert) -> AnyView {
    let warnings = c
        .warnings
        .iter()
        .map(|w| ui::note(if *w == TlsWarning::Expired { Tone::Err } else { Tone::Warn }, icons::TRIANGLE_ALERT, ui::text(t(warning_key(*w)).to_string())))
        .collect_view();
    let cert = c.certificate.clone();
    view! {
        {warnings}
        <dl class="kvs">
            {cert.as_ref().map(|x| kv("tls.subject", ui::breakable(&x.subject, "mono")))}
            {cert.as_ref().map(|x| kv("tls.issuer", ui::breakable(&x.issuer, "mono")))}
            {cert.as_ref().map(|x| kv("tls.names", names(&x.dns_names)))}
            {cert.as_ref().map(|x| kv("tls.valid_until", ui::text(i18n::datetime(x.not_after_ms.max(0) as u64))))}
            {kv("wa.import.covers", if c.covers.is_empty() { ui::text(t("wa.import.covers_none").to_string()) } else { names(&c.covers) })}
            {(!c.shadowed.is_empty()).then(|| kv("wa.import.shadowed", names(&c.shadowed)))}
            {(!c.replaces_acme.is_empty()).then(|| kv("wa.import.replaces_acme", names(&c.replaces_acme)))}
        </dl>
    }
    .into_any()
}

/// A free import id from the description: `[a-z0-9-]`, at most 32, not starting with `-`.
fn id_from(label: &str) -> String {
    let mut s = String::new();
    for c in label.to_ascii_lowercase().chars() {
        let c = if c.is_ascii_alphanumeric() { c } else { '-' };
        if !(c == '-' && s.ends_with('-')) {
            s.push(c);
        }
    }
    let s = s.trim_matches('-').chars().take(32).collect::<String>();
    s.trim_end_matches('-').to_string()
}

fn import_dialog(imp: Imp, id: String) -> AnyView {
    let close = Callback::new(move |_| imp.form.set(None));
    let replacing = !id.is_empty();
    let existing = imp.list.data.with_untracked(|l| l.as_ref().and_then(|l| l.iter().find(|c| c.id == id).cloned()));
    let iid = RwSignal::new(id.clone());
    let id_touched = RwSignal::new(replacing);
    let label = RwSignal::new(existing.as_ref().map(|c| c.label.clone()).unwrap_or_default());
    let source = RwSignal::new(if existing.as_ref().is_some_and(|c| c.source == ImportSource::Folder) { "folder" } else { "upload" }.to_string());
    let cert = RwSignal::new(String::new());
    let key = RwSignal::new(String::new());
    let id_err = RwSignal::new(None::<String>);
    let label_err = RwSignal::new(None::<String>);
    let cert_err = RwSignal::new(None::<String>);
    let key_err = RwSignal::new(None::<String>);
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let checking = RwSignal::new(false);
    let found = RwSignal::new(None::<ImportedCert>);
    let request = move || {
        let upload = source.get_untracked() == "upload";
        let text = |s: RwSignal<String>| Some(s.get_untracked()).filter(|v| !v.trim().is_empty());
        CertImportRequest {
            id: iid.get_untracked().trim().to_string(),
            label: label.get_untracked().trim().to_string(),
            source: if upload { ImportSource::Upload } else { ImportSource::Folder },
            certificate: if upload { text(cert) } else { None },
            private_key: if upload { text(key) } else { None },
        }
    };
    let route = move |e: api::ApiErr| match e.field() {
        Some("id") => id_err.set(Some(e.text())),
        Some("label") => label_err.set(Some(e.text())),
        Some("certificate") => cert_err.set(Some(e.text())),
        Some("private_key") => key_err.set(Some(e.text())),
        _ => error.set(Some(e.text())),
    };
    let inspect = move |_| {
        let body = request();
        checking.set(true);
        error.set(None);
        spawn_local(async move {
            let r = api::post::<_, ImportedCert>(&format!("{URL}/inspect"), Some(&body)).await;
            let _ = checking.try_set(false);
            match r {
                Ok(c) => {
                    let _ = found.try_set(Some(c));
                }
                Err(e) => route(e),
            }
        });
    };
    let save = move |_| {
        let body = request();
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = api::post::<_, ImportedCert>(URL, Some(&body)).await;
            let _ = busy.try_set(false);
            match r {
                Ok(c) => {
                    imp.list.reload();
                    imp.changed.run(());
                    toast(Tone::Ok, t("wa.import.saved").to_string());
                    imp.form.set(None);
                    imp.open.set(Some(c.id));
                }
                Err(e) => route(e),
            }
        });
    };
    let changed = move || {
        found.set(None);
    };
    Effect::new(move || {
        cert.track();
        key.track();
        source.track();
        changed();
    });
    let body = view! {
        {(!replacing).then(|| view! {
            {text_field("wa.import.label", label.into(), Callback::new(move |v: String| {
                label_err.set(None);
                if !id_touched.get_untracked() {
                    iid.set(id_from(&v));
                }
                label.set(v);
            }), label_err.into(), In { maxlength: 80, ..In::default() })}
            {text_field("wa.import.id", iid.into(), Callback::new(move |v: String| { id_err.set(None); id_touched.set(true); iid.set(v); }), id_err.into(), In { mono: true, maxlength: 32, help: Some("wa.import.id_help"), ..In::default() })}
        })}
        {crate::form::segmented("wa.import.source", &[("upload", "wa.import.source.upload"), ("folder", "wa.import.source.folder")], source.into(), Callback::new(move |v| source.set(v)))}
        <Show when=move || source.get() == "upload" fallback=|| view! { <p class="small muted">{tr("wa.import.folder_help")}</p> }>
            {pem_field("wa.import.certificate", cert, cert_err, ".crt,.pem,.cer")}
            {pem_field("wa.import.private_key", key, key_err, ".key,.pem")}
            <p class="small muted">{tr("dom.f.never_shown")}</p>
            <div class="sec-actions">
                <button type="button" class="btn ghost sm" disabled=move || checking.get() || cert.with(|c| c.trim().is_empty()) || key.with(|k| k.trim().is_empty()) on:click=inspect>
                    {spinner(checking)}<Icon icon=icons::SHIELD_CHECK size=14 />{tr("wa.import.inspect")}
                </button>
            </div>
            {move || found.with(|f| f.as_ref().map(inspect_view))}
        </Show>
        {form_error(error)}
    }
    .into_any();
    let ready = move || {
        !iid.with(|i| i.trim().is_empty()) && (source.with(|s| s == "folder") || (cert.with(|c| !c.trim().is_empty()) && key.with(|k| !k.trim().is_empty())))
    };
    let foot = view! {
        <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("cancel")}</button>
        <button type="button" class="btn primary" disabled=move || busy.get() || !ready() on:click=save>{spinner(busy)}<Icon icon=icons::IMPORT size=16 />{tr("save")}</button>
    }
    .into_any();
    let title = if replacing { "wa.import.replace" } else { "wa.import.add" };
    ui::dialog(DlgKind::Normal, Signal::derive(move || t(title).to_string()), close, body, Some(foot))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_ssh_lines() {
        assert_eq!(id_from("Router wildcard *.home.example"), "router-wildcard-home-example");
        assert_eq!(id_from("--x--"), "x");
        let (sshd, acme) = ssh_lines("/var/lib/rynas-cert-drop", "router", "nas");
        assert!(sshd.contains("ForceCommand internal-sftp -d /router -u 0007"));
        assert!(acme.contains("DEPLOY_SSH_SERVER=nas.local") && acme.contains("/router/fullchain.pem"));
    }
}
