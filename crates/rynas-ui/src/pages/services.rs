//! File services (DSM Control Panel › File Services, TrueNAS Services › SMB/NFS, OMV Services):
//! global SMB settings with the recycle bin cleanup, global NFS settings and the list of NFS
//! exports (edited per shared folder). Saving stages a pending change.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::{NfsSettings, NfsSquash, NfsState, ShareView, SmbSettings, SmbState};

use crate::api;
use crate::form::{Draft, In, area_field, lens, lens_bool, select_field, stale_notice, switch, text_field};
use crate::i18n::{t, tr};
use crate::icons::{self, Icon};
use crate::pages::shares::share_path;
use crate::toast::{saved_pending, toast};
use crate::ui::{self, PageHeader, Remote, Tone, WidgetHead};

const MIN_PROTOCOLS: &[(&str, &str)] = &[("SMB2_10", "smb.proto_smb2"), ("SMB3_00", "smb.proto_smb3"), ("SMB3_11", "smb.proto_smb311")];

/// NetBIOS-style workgroup: 1–15 characters, no spaces or `\/:*?"<>|`.
fn valid_workgroup(w: &str) -> bool {
    !w.is_empty() && w.len() <= 15 && !w.contains(|c: char| c.is_whitespace() || "\\/:*?\"<>|,".contains(c))
}

fn running_pill(enabled: bool, running: bool) -> AnyView {
    match (enabled, running) {
        (_, true) => ui::status(Tone::Ok, t("svc.running").to_string()),
        (true, false) => ui::status(Tone::Warn, t("svc.stopped").to_string()),
        (false, false) => ui::status(Tone::Unknown, t("svc.off").to_string()),
    }
}

#[component]
pub fn FileServices() -> impl IntoView {
    let smb = Remote::<SmbState>::with_config("/api/v1/services/smb");
    let nfs = Remote::<NfsState>::with_config("/api/v1/services/nfs");
    let shares = Remote::<Vec<ShareView>>::with_config("/api/v1/shares");
    view! {
        <div class="page">
            <PageHeader icon=icons::SERVER title="svc.title" subtitle="svc.subtitle" />
            <div class="grid">
                <section class="card widget w-6">
                    <WidgetHead icon=icons::SHARE_2 title="svc.smb">
                        {move || smb.data.with(|s| s.as_ref().map(|s| view! { {running_pill(s.settings.enabled, s.running)}{s.pending.then(|| ui::status(Tone::Info, t("pending.change").to_string()))} }))}
                    </WidgetHead>
                    {move || smb.gate(6).unwrap_or_else(|| {
                        // built once: the draft follows the server's settings (form::Draft)
                        let s = smb.data.get_untracked().unwrap_or_default();
                        smb_form(s.settings, Signal::derive(move || smb.data.with(|s| s.as_ref().map(|s| s.settings.clone()))), shares)
                    })}
                </section>
                <section class="card widget w-6">
                    <WidgetHead icon=icons::FOLDER_SYNC title="svc.nfs">
                        {move || nfs.data.with(|s| s.as_ref().map(|s| view! { {running_pill(s.settings.enabled, s.running)}{s.pending.then(|| ui::status(Tone::Info, t("pending.change").to_string()))} }))}
                    </WidgetHead>
                    {move || nfs.gate(5).unwrap_or_else(|| {
                        let s = nfs.data.get_untracked().unwrap_or_default();
                        nfs_form(s.settings, Signal::derive(move || nfs.data.with(|s| s.as_ref().map(|s| s.settings.clone()))))
                    })}
                </section>
                <section class="card widget w-12">
                    <WidgetHead icon=icons::LIST_CHECKS title="svc.exports" />
                    {move || shares.gate(3).unwrap_or_else(|| exports(shares))}
                </section>
            </div>
        </div>
    }
}

/// Save button of a settings card: enabled when the draft differs from the server's value;
/// above it the "changed elsewhere" notice of a stale draft.
fn save_row(busy: RwSignal<bool>, dirty: Signal<bool>, stale: RwSignal<bool>, error: RwSignal<Option<String>>, reset: Callback<()>) -> AnyView {
    view! {
        {stale_notice(stale, reset)}
        {crate::form::form_error(error)}
        <div class="sec-actions">
            <button type="submit" class="btn primary" disabled=move || busy.get() || !dirty.get()>{crate::form::spinner(busy)}{tr("save")}</button>
            <button type="button" class="btn ghost" disabled=move || !dirty.get() on:click=move |_| reset.run(())>{tr("reset")}</button>
        </div>
    }
    .into_any()
}

fn smb_form(init: SmbSettings, server: Signal<Option<SmbSettings>>, shares: Remote<Vec<ShareView>>) -> AnyView {
    let draft = Draft::new(init, move || server.get());
    let d = draft.d;
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let (enabled, set_enabled) = lens_bool(d, |s| s.enabled, |s, v| s.enabled = v);
    let (wg, set_wg) = lens(d, |s| s.workgroup.clone(), |s, v| s.workgroup = v.trim().to_uppercase());
    let (ss, set_ss) = lens(d, |s| s.server_string.clone(), |s, v| s.server_string = v);
    let (proto, set_proto) = lens(d, |s| s.min_protocol.clone(), |s, v| s.min_protocol = v);
    let (nb, set_nb) = lens_bool(d, |s| s.netbios, |s, v| s.netbios = v);
    let (wsdd, set_wsdd) = lens_bool(d, |s| s.wsdd, |s, v| s.wsdd = v);
    let (mdns, set_mdns) = lens_bool(d, |s| s.mdns, |s, v| s.mdns = v);
    let (enc_, set_enc) = lens_bool(d, |s| s.encryption_required, |s, v| s.encryption_required = v);
    let (ifs, set_ifs) = lens(d, |s| s.interfaces.join(", "), |s, v| {
        s.interfaces = v.split([',', ' ']).map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect()
    });
    let (extra, set_extra) = lens(d, |s| s.extra.clone(), |s, v| s.extra = v);
    let wg_err = Signal::derive(move || (!valid_workgroup(&wg.get())).then(|| t("v.workgroup").to_string()));
    let none = Signal::derive(|| None::<String>);
    let dirty = draft.dirty();
    let reset = Callback::new(move |_| {
        draft.reset();
        error.set(None);
    });
    let save = move || {
        if !valid_workgroup(&wg.get_untracked()) {
            error.set(Some(t("form.check").to_string()));
            return;
        }
        busy.set(true);
        error.set(None);
        let body = d.get_untracked();
        spawn_local(async move {
            let r = api::put::<_, SmbState>("/api/v1/services/smb", &body).await;
            busy.set(false);
            match r {
                Ok(_) => {
                    saved_pending();
                    crate::config::changed();
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    };
    view! {
        <form class="form" on:submit=move |ev| { ev.prevent_default(); save(); }>
            {switch("smb.enabled", Some("smb.enabled_help"), enabled, set_enabled)}
            <div class="form" hidden=move || !enabled.get()>
                <div class="form-row">
                    {text_field("smb.workgroup", wg, set_wg, wg_err, In { help: Some("smb.workgroup_help"), mono: true, maxlength: 15, ..In::default() })}
                    {select_field("smb.min_protocol", Some("smb.min_protocol_help"), MIN_PROTOCOLS, proto, set_proto)}
                </div>
                {text_field("smb.server_string", ss, set_ss, none, In { help: Some("smb.server_string_help"), placeholder: "rynas %h", ..In::default() })}
                {switch("smb.wsdd", Some("smb.wsdd_help"), wsdd, set_wsdd)}
                {switch("smb.mdns", Some("smb.mdns_help"), mdns, set_mdns)}
                {switch("smb.encrypt", Some("smb.encrypt_help"), enc_, set_enc)}
                <details class="advanced">
                    <summary>{tr("advanced")}</summary>
                    <div class="form">
                        {switch("smb.netbios", Some("smb.netbios_help"), nb, set_nb)}
                        {text_field("smb.interfaces", ifs, set_ifs, none, In { help: Some("smb.interfaces_help"), placeholder: "eth0, 192.0.2.10", mono: true, ..In::default() })}
                        {area_field("smb.extra", extra, set_extra, Some("smb.extra_help"), "")}
                    </div>
                </details>
            </div>
            {save_row(busy, dirty, draft.stale, error, reset)}
        </form>
        {recycle_cleanup(shares)}
    }
    .into_any()
}

/// Empty the `#recycle` bins now (task `smb.recycle-cleanup`), for every share or one, with
/// their retention or a number of days.
fn recycle_cleanup(shares: Remote<Vec<ShareView>>) -> AnyView {
    let share = RwSignal::new(String::new());
    let days = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let sid = crate::form::fid();
    let run = move || {
        busy.set(true);
        let mut q = Vec::new();
        let s = share.get_untracked();
        if !s.is_empty() {
            q.push(format!("share={}", api::encode(&s)));
        }
        if let Ok(n) = days.get_untracked().trim().parse::<u32>() {
            q.push(format!("days={n}"));
        }
        let url = format!("/api/v1/services/smb/recycle-cleanup{}{}", if q.is_empty() { "" } else { "?" }, q.join("&"));
        spawn_local(async move {
            let r = crate::tasks::run_task(&url, None).await;
            let _ = busy.try_set(false);
            match r {
                Ok(task) if task.status == rynas_api_types::TaskStatus::Ok => toast(Tone::Ok, t("recycle.done").to_string()),
                Ok(task) => toast(Tone::Err, crate::tasks::task_error(&task)),
                Err(e) if e.status() == 401 => {}
                Err(e) => toast(Tone::Err, e.text()),
            }
        });
    };
    let names = move || {
        shares.data.with(|d| d.iter().flatten().filter(|s| s.share.smb.enabled && s.share.smb.recycle.enabled).map(|s| s.share.name.clone()).collect::<Vec<_>>())
    };
    view! {
        <div class="subsection">
            <h3 class="form-section"><Icon icon=icons::TRASH_2 size=16 />{tr("recycle.title")}</h3>
            <p class="field-help">{tr("recycle.help")}</p>
            <div class="form-row">
                <div class="field">
                    <label class="field-label" for=sid.clone()>{tr("shares.folder")}</label>
                    <select id=sid class="input" on:change=move |ev| share.set(event_target_value(&ev))>
                        <option value="">{tr("recycle.all")}</option>
                        {move || names().into_iter().map(|n| view! { <option value=n.clone()>{n.clone()}</option> }).collect_view()}
                    </select>
                </div>
                {text_field("recycle.days", Signal::derive(move || days.get()), Callback::new(move |v| days.set(v)), Signal::derive(|| None), In { kind: "number", help: Some("recycle.days_help"), maxlength: 5, ..In::default() })}
            </div>
            <div class="sec-actions">
                <button type="button" class="btn ghost" disabled=move || busy.get() on:click=move |_| run()>
                    {crate::form::spinner(busy)}<Icon icon=icons::TRASH_2 size=16 />{tr("recycle.run")}
                </button>
            </div>
        </div>
    }
    .into_any()
}

fn nfs_form(init: NfsSettings, server: Signal<Option<NfsSettings>>) -> AnyView {
    let draft = Draft::new(init, move || server.get());
    let d = draft.d;
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let (enabled, set_enabled) = lens_bool(d, |s| s.enabled, |s, v| s.enabled = v);
    let (v3, set_v3) = lens_bool(d, |s| s.v3, |s, v| s.v3 = v);
    let (threads, set_threads) = lens(d, |s| s.threads.to_string(), |s, v| s.threads = v.trim().parse().unwrap_or(0));
    let port = |get: fn(&NfsSettings) -> u16, set: fn(&mut NfsSettings, u16)| {
        (Signal::derive(move || d.with(|s| get(s).to_string())), Callback::new(move |v: String| d.update(|s| set(s, v.trim().parse().unwrap_or(0)))))
    };
    let (mountd, set_mountd) = port(|s| s.mountd_port, |s, v| s.mountd_port = v);
    let (statd, set_statd) = port(|s| s.statd_port, |s, v| s.statd_port = v);
    let (lockd, set_lockd) = port(|s| s.lockd_port, |s, v| s.lockd_port = v);
    let threads_err = Signal::derive(move || d.with(|s| !(1..=256).contains(&s.threads)).then(|| t("v.threads").to_string()));
    let port_err = |p: Signal<String>| Signal::derive(move || (p.get().parse::<u16>().unwrap_or(0) < 1).then(|| t("v.port").to_string()));
    let ok = move || d.with(|s| (1..=256).contains(&s.threads) && s.mountd_port > 0 && s.statd_port > 0 && s.lockd_port > 0);
    let dirty = draft.dirty();
    let reset = Callback::new(move |_| {
        draft.reset();
        error.set(None);
    });
    let save = move || {
        if !ok() {
            error.set(Some(t("form.check").to_string()));
            return;
        }
        busy.set(true);
        error.set(None);
        let body = d.get_untracked();
        spawn_local(async move {
            let r = api::put::<_, NfsState>("/api/v1/services/nfs", &body).await;
            busy.set(false);
            match r {
                Ok(_) => {
                    saved_pending();
                    crate::config::changed();
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    };
    let num = In { kind: "number", mono: true, maxlength: 5, ..In::default() };
    view! {
        <form class="form" on:submit=move |ev| { ev.prevent_default(); save(); }>
            {switch("nfs.enabled", Some("nfs.enabled_help"), enabled, set_enabled)}
            <div class="form" hidden=move || !enabled.get()>
                {switch("nfs.v3", Some("nfs.v3_help"), v3, set_v3)}
                {text_field("nfs.threads", threads, set_threads, threads_err, In { help: Some("nfs.threads_help"), ..num })}
                <div class="form-row three" hidden=move || !v3.get()>
                    {text_field("nfs.mountd", mountd, set_mountd, port_err(mountd), num)}
                    {text_field("nfs.statd", statd, set_statd, port_err(statd), num)}
                    {text_field("nfs.lockd", lockd, set_lockd, port_err(lockd), num)}
                </div>
                <p class="field-help" hidden=move || !v3.get()>{tr("nfs.ports_help")}</p>
            </div>
            {save_row(busy, dirty, draft.stale, error, reset)}
        </form>
    }
    .into_any()
}

/// Every NFS export: share, path and client rules (edited in the shared folder).
fn exports(shares: Remote<Vec<ShareView>>) -> AnyView {
    let rows: Vec<ShareView> = shares.data.with(|d| d.iter().flatten().filter(|s| s.share.nfs.enabled).cloned().collect());
    if rows.is_empty() {
        return view! { <p class="muted table-none">{tr("svc.no_exports")}</p> }.into_any();
    }
    view! {
        {ui::table("", &["shares.name", "shares.path", "nfs.rules"], rows.into_iter().map(|v| {
                        let s = v.share;
                        let path = share_path(&s);
                        ui::row(vec![
                            ui::cell("shares.name", "", view! { <a class="link-strong" href="/shares">{s.name.clone()}</a> }.into_any()),
                            ui::cell("shares.path", "mono", view! { {path} }.into_any()),
                            ui::cell("nfs.rules", "", view! {
                                <div class="chips">
                                    {if s.nfs.rules.is_empty() {
                                        view! { <span class="muted small">{tr("nfs.no_rules_short")}</span> }.into_any()
                                    } else {
                                        s.nfs.rules.iter().map(|r| {
                                            let squash = match r.squash { NfsSquash::Root => "root_squash", NfsSquash::All => "all_squash", NfsSquash::None => "no_root_squash" };
                                            let text = format!("{}({},{},{})", r.client, if r.read_only { "ro" } else { "rw" }, if r.sync { "sync" } else { "async" }, squash);
                                            view! { <span class="chip mono">{text}</span> }
                                        }).collect_view().into_any()
                                    }}
                                </div>
                            }
                            .into_any()),
                        ])
                    }).collect::<Vec<_>>())}
        <p class="field-help">{tr("svc.exports_help")}</p>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workgroup() {
        assert!(valid_workgroup("WORKGROUP"));
        assert!(valid_workgroup("HOME-LAN"));
        assert!(!valid_workgroup(""));
        assert!(!valid_workgroup("MY GROUP"));
        assert!(!valid_workgroup("ABCDEFGHIJKLMNOP"));
    }
}
