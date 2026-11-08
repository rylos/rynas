//! Hardware › UPS › network UPS server (`/api/v1/ups/netserver`,
//! `/api/v1/ups/plan/netserver`, key `ups.netserver`): other machines read this UPS from
//! upsd (port 3493) and shut down on its low battery. On/off, listen addresses and allowed
//! hosts in a settings dialog; client accounts in a table (add, new password, remove), each a
//! dialog that saves and applies only this key (A19b). A password is shown once, right after
//! the apply, with the connection details for home automation and for another machine's
//! upsmon, each with a copy button.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::ups::*;

use crate::api;
use crate::form::{area_field, form_error, segmented, spinner, switch, text_field, In};
use crate::i18n::{t, tf, tr};
use crate::icons::{self, Icon};
use crate::modules::{self, UpsSlot};
use crate::pages::security::copy_button;
use crate::toast::saved_applied;
use crate::ui::{self, Confirm, Modal, Remote, Tone, WidgetHead, confirm, status};

const URL: &str = "/api/v1/ups/netserver";
const PLAN: &str = "/api/v1/ups/plan/netserver";
const KEY: &str = crate::config::keys::UPS_NETSERVER;

/// The value a form starts from: the staged one, else the applied one, else off.
fn current(st: &UpsNetServerState) -> UpsNetServerConfig {
    st.pending.clone().or(st.applied.clone()).unwrap_or_default()
}

/// i18n key of a role.
pub fn role_key(r: UpsClientRole) -> &'static str {
    match r {
        UpsClientRole::Secondary => "upsn.role_secondary",
        UpsClientRole::Monitor => "upsn.role_monitor",
    }
}

/// The `upsmon.conf` line of another machine (`PASSWORD` when the password is not known).
pub fn monitor_line(ups: &str, host: &str, user: &str, password: Option<&str>) -> String {
    let host = if host.contains(':') { format!("[{host}]") } else { host.to_string() };
    format!("MONITOR {ups}@{host} 1 {user} {} secondary", password.unwrap_or("PASSWORD"))
}

/// Allowed hosts typed one per line (spaces and commas also separate them).
pub fn parse_list(s: &str) -> Vec<String> {
    s.split(|c: char| c.is_whitespace() || c == ',').filter(|w| !w.is_empty()).map(str::to_string).collect()
}

/// `nets` added to the allowed hosts typed in `allow` (one per line, those already there
/// are not repeated; the text stays as typed when nothing is missing).
pub fn add_hosts(allow: &str, nets: &[String]) -> String {
    let have = parse_list(allow);
    let missing: Vec<&String> = nets.iter().filter(|n| !have.contains(n)).collect();
    if missing.is_empty() {
        return allow.to_string();
    }
    let mut out = allow.trim_end().to_string();
    for n in missing {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(n);
    }
    out
}

/// A password upsd accepts (rynasd checks the same): 12 to 64 characters from
/// `A-Z a-z 0-9 . _ ~ @ % + = : , -`.
pub fn password_ok(p: &str) -> bool {
    let (lo, hi) = NET_PASSWORD_LEN;
    (lo..=hi).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric() || b"._~@%+=:,-".contains(&b))
}

/// A user name rynasd accepts: `a-z 0-9 _ -`, 1 to 32 characters, not one of rynas's own.
pub fn user_ok(u: &str) -> bool {
    (1..=32).contains(&u.len())
        && u.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        && !["rynas", "rynas-upsmon"].contains(&u)
}

/// Saves `cfg` and applies `ups.netserver`; the passwords of this request on success. A
/// refused listen address (`listen.N`) goes to `listen_err`, next to the addresses.
async fn save(cfg: UpsNetServerConfig, error: RwSignal<Option<String>>, listen_err: Option<RwSignal<Option<String>>>) -> Option<Vec<UpsNetPassword>> {
    error.set(None);
    if let Some(l) = listen_err {
        l.set(None);
    }
    let listen = cfg.listen.clone();
    match api::put::<_, UpsNetServerSaved>(URL, &cfg).await {
        Ok(saved) => crate::config::apply_now(&[KEY], error).await.then_some(saved.passwords),
        Err(e) => {
            let bad = e.field().and_then(|f| f.strip_prefix("listen.")).and_then(|i| i.parse::<usize>().ok()).and_then(|i| listen.get(i));
            match (bad, listen_err) {
                (Some(a), Some(l)) => {
                    let _ = l.try_set(Some(tf("upsn.listen_bad", &[("addr", a)])));
                }
                _ => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
            None
        }
    }
}

/// A password shown once, for an account.
#[derive(Clone, PartialEq)]
struct Shown {
    name: String,
    role: UpsClientRole,
    password: Option<String>,
}

// ------------------------------------------------------------------ the card

pub fn card() -> AnyView {
    let state = Remote::<UpsNetServerState>::with_config(URL);
    #[cfg(feature = "demo")]
    if crate::auth::auth().phase.get_untracked() == crate::auth::Phase::Demo {
        state.data.set(Some(crate::demo::ups_netserver()));
    }
    let settings_open = RwSignal::new(false);
    let add_open = RwSignal::new(false);
    let shown = RwSignal::new(None::<Shown>);
    let remove = RwSignal::new(None::<String>);
    let regen = RwSignal::new(None::<String>);
    let body = move || {
        if let Some(v) = state.gate(3) {
            return v;
        }
        let st = state.data.get().unwrap_or_default();
        match st.blocked {
            Some(UpsNetBlock::NutMissing) => return view! { <p class="muted">{t("upsn.need_nut")}</p> }.into_any(),
            Some(UpsNetBlock::Module) => return ui::note(Tone::Info, icons::INFO, view! { {t("upsn.module")} }.into_any()),
            Some(UpsNetBlock::ModuleTooOld) => return ui::note(Tone::Info, icons::INFO, view! { {t(modules::ups_key(UpsSlot::NetTooOld))} }.into_any()),
            None => {}
        }
        let c = current(&st);
        // a module layer runs upsd and its firewall (st.layer): listen addresses are its own,
        // allowed hosts are required
        let layer = st.layer.clone();
        let module = layer.is_some();
        let listen = if module {
            t(modules::ups_key(UpsSlot::NetListen)).to_string()
        } else if c.listen.is_empty() {
            t("upsn.listen_all").to_string()
        } else {
            c.listen.join(", ")
        };
        let allow = match (c.allow.is_empty(), module) {
            (false, _) => c.allow.join(", "),
            (true, true) => t("upsn.allow_none").to_string(),
            (true, false) => t("upsn.allow_any").to_string(),
        };
        let filter_label = if module { modules::ups_key(UpsSlot::NetFilter) } else { "upsn.filter" };
        let layer_notes = layer.map(|l| {
            let mut v = Vec::new();
            if let Some(name) = &l.primary_account {
                v.push(ui::note(Tone::Info, icons::INFO, ui::text_code(&tf(modules::ups_key(UpsSlot::NetPrimary), &[("name", name)]))));
            }
            if l.source == UpsNetSource::Manual && st.applied.is_none() {
                v.push(ui::note(Tone::Info, icons::INFO, view! { {t(modules::ups_key(UpsSlot::NetManual))} }.into_any()));
            }
            if !l.outside.is_empty() {
                v.push(ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tf(modules::ups_key(UpsSlot::NetOutside), &[("names", &l.outside.join(", "))])} }.into_any()));
            }
            v.collect_view()
        });
        let host = st.host.clone().unwrap_or_else(|| st.hostname.clone());
        let ups = st.ups_name.clone().unwrap_or_else(|| "ups".into());
        let rows: Vec<AnyView> = c
            .clients
            .iter()
            .map(|cl| {
                let (n1, n2, n3) = (cl.name.clone(), cl.name.clone(), cl.name.clone());
                let role = cl.role;
                let aria = |k: &'static str| format!("{}: {}", t(k), cl.name);
                ui::row(vec![
                    ui::cell("username", "", view! { <span class="mono">{cl.name.clone()}</span> }.into_any()),
                    ui::cell("upsn.role", "", view! { {tr(role_key(cl.role))} }.into_any()),
                    ui::cell("upsn.note", "wrap", view! { {cl.note.clone()} }.into_any()),
                    ui::actions(view! {
                        <button type="button" class="icon-btn sm" title=tr("upsn.connect") aria-label=aria("upsn.connect")
                            on:click=move |_| shown.set(Some(Shown { name: n1.clone(), role, password: None }))>
                            <Icon icon=icons::LINK size=16 />
                        </button>
                        <button type="button" class="icon-btn sm" title=tr("pw.new") aria-label=aria("pw.new") on:click=move |_| regen.set(Some(n2.clone()))>
                            <Icon icon=icons::KEY size=16 />
                        </button>
                        <button type="button" class="icon-btn sm danger" title=tr("upsn.remove") aria-label=aria("upsn.remove") on:click=move |_| remove.set(Some(n3.clone()))>
                            <Icon icon=icons::TRASH_2 size=16 />
                        </button>
                    }.into_any()),
                ])
            })
            .collect();
        let has_rows = !rows.is_empty();
        let problems = st
            .problems
            .iter()
            .map(|p| match p {
                UpsNetProblem::ServerInactive => ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {t("upsn.pb.server_inactive")} }.into_any()),
                UpsNetProblem::FilterInactive => {
                    let k = if module { modules::ups_key(UpsSlot::NetFilterInactive) } else { "upsn.pb.filter_inactive" };
                    ui::note(Tone::Err, icons::SHIELD_ALERT, view! { {t(k)} }.into_any())
                }
                UpsNetProblem::LayerUnreadable => ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {t(modules::ups_key(UpsSlot::NetUnreadable))} }.into_any()),
                UpsNetProblem::Other => ui::note(Tone::Info, icons::INFO, view! { {t("upsn.pb.other")} }.into_any()),
            })
            .collect_view();
        view! {
            <div class="btn-row">
                {status(if c.enabled { Tone::Ok } else { Tone::Unknown }, t(if c.enabled { "upsn.on" } else { "upsn.off" }).to_string())}
                {st.pending.is_some().then(|| view! { <span class="muted small">{t("upsn.pending")}</span> })}
            </div>
            <p class="muted">{t("upsn.intro")}</p>
            <dl class="kvs">
                <div class="kv"><dt>{tr("smb.interfaces")}</dt><dd class="wrap">{listen}</dd></div>
                <div class="kv"><dt>{tr("upsn.allow")}</dt><dd class="wrap">{allow}</dd></div>
                {st.filter_active.map(|on| view! {
                    <div class="kv"><dt>{tr(filter_label)}</dt><dd>{status(if on { Tone::Ok } else { Tone::Err }, t(if on { "upsn.filter_on" } else { "upsn.filter_off" }).to_string())}</dd></div>
                })}
                <div class="kv"><dt>{tr("upsn.address")}</dt><dd class="mono wrap">{format!("{host}:{}", st.port)}</dd></div>
                <div class="kv"><dt>{tr("upsn.ups_name")}</dt><dd class="mono">{ups}</dd></div>
            </dl>
            {problems}
            {layer_notes}
            {(!module && c.enabled && c.allow.is_empty()).then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {t("upsn.open_warn")} }.into_any()))}
            <h3 class="section-title">{tr("upsn.accounts")}</h3>
            {if has_rows {
                ui::table("upsn-table", &["username", "upsn.role", "upsn.note", ""], rows)
            } else {
                view! { <p class="muted">{t("upsn.no_accounts")}</p> }.into_any()
            }}
            <div class="btn-row">
                <button type="button" class="btn ghost" on:click=move |_| settings_open.set(true)><Icon icon=icons::SETTINGS size=16 />{tr("upsn.settings")}</button>
                <button type="button" class="btn primary" on:click=move |_| add_open.set(true)><Icon icon=icons::PLUS size=16 />{tr("upsn.add")}</button>
            </div>
        }
        .into_any()
    };
    // the callbacks live in the card's owner, not in the closures that show each dialog: a
    // dialog's owner is disposed with it, and the answer of its change (a reload, a password
    // shown once) must still arrive; a dialog does not close while its change applies
    let refresh = RwSignal::new(0u32);
    Effect::new(move |prev: Option<u32>| {
        let n = refresh.get();
        if prev.is_some() {
            state.reload();
        }
        n
    });
    let done = move |s: Option<Shown>| {
        refresh.update(|n| *n += 1);
        if s.is_some() {
            shown.set(s);
        }
    };
    let close_settings = Callback::new(move |changed: bool| {
        settings_open.set(false);
        if changed {
            refresh.update(|n| *n += 1);
        }
    });
    let close_add = Callback::new(move |s: Option<Shown>| {
        add_open.set(false);
        if s.is_some() {
            done(s);
        }
    });
    let close_shown = Callback::new(move |_: ()| shown.set(None));
    let close_remove = Callback::new(move |s: Option<Option<Shown>>| {
        remove.set(None);
        if let Some(s) = s {
            done(s);
        }
    });
    let close_regen = Callback::new(move |s: Option<Option<Shown>>| {
        regen.set(None);
        if let Some(s) = s {
            done(s);
        }
    });
    view! {
        <section class="card widget">
            <WidgetHead icon=icons::NETWORK title="upsn.title" />
            {body}
            {move || settings_open.get().then(|| settings_dialog(state, close_settings))}
            {move || add_open.get().then(|| add_dialog(state, close_add))}
            {move || shown.get().map(|s| connect_dialog(state, s, close_shown))}
            {move || remove.get().map(|name| change_confirm(state, name, false, close_remove))}
            {move || regen.get().map(|name| change_confirm(state, name, true, close_regen))}
        </section>
    }
    .into_any()
}

// ------------------------------------------------------------------ settings

#[derive(Clone, PartialEq)]
struct Settings {
    enabled: bool,
    all: bool,
    listen: Vec<String>,
    allow: String,
}

/// `on_close(true)` after a change was applied.
fn settings_dialog(state: Remote<UpsNetServerState>, on_close: Callback<bool>) -> AnyView {
    let cancel = Callback::new(move |_: ()| on_close.run(false));
    let st = state.data.get_untracked().unwrap_or_default();
    let base = current(&st);
    let module = st.layer.is_some();
    // hosts set by hand in the layer's file: the starting point of a first save here
    let start_allow = match &st.layer {
        Some(l) if base.allow.is_empty() && st.applied.is_none() && st.pending.is_none() => l.allow.join("\n"),
        _ => base.allow.join("\n"),
    };
    let f = RwSignal::new(Settings { enabled: base.enabled, all: base.listen.is_empty(), listen: base.listen.clone(), allow: start_allow });
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let plan = RwSignal::new(None::<Result<UpsSetupPlan, String>>);
    let listen_err = RwSignal::new(None::<String>);
    // the NAS's addresses, plus a chosen one it no longer has (shown so it can be unticked)
    let mut addresses: Vec<String> = st.addresses.iter().map(|a| a.address.clone()).collect();
    addresses.extend(base.listen.iter().filter(|a| !addresses.contains(a)).cloned().collect::<Vec<_>>());
    let subnets: Vec<String> = {
        let mut v: Vec<String> = st.addresses.iter().map(|a| a.subnet.clone()).filter(|s| !s.is_empty()).collect();
        v.dedup();
        v
    };
    let value = move || {
        let s = f.get_untracked();
        UpsNetServerConfig { enabled: s.enabled, listen: if s.all { Vec::new() } else { s.listen }, allow: parse_list(&s.allow), clients: base.clients.clone() }
    };
    let value = StoredValue::new(value);
    let show_plan = move || {
        let body = value.with_value(|v| v());
        plan.set(None);
        spawn_local(async move {
            let r = api::post::<_, UpsSetupPlan>(PLAN, Some(&body)).await.map_err(|e| e.text());
            let _ = plan.try_set(Some(r));
        });
    };
    let submit = move || {
        let body = value.with_value(|v| v());
        if module && body.enabled && body.allow.is_empty() {
            error.set(Some(t("upsn.allow_required").to_string()));
            return;
        }
        busy.set(true);
        spawn_local(async move {
            if save(body, error, Some(listen_err)).await.is_some() {
                saved_applied();
                let _ = on_close.try_run(true);
            }
            let _ = busy.try_set(false);
        });
    };
    let mode = Signal::derive(move || if f.with(|s| s.all) { "all".to_string() } else { "chosen".to_string() });
    let allow_text = Signal::derive(move || f.with(|s| s.allow.clone()));
    let nets = StoredValue::new(subnets.clone());
    view! {
        <Modal title=Signal::stored(t("upsn.settings_title").to_string()) on_close=cancel wide=true>
            {switch("upsn.enable", Some("upsn.enable_help"), Signal::derive(move || f.with(|s| s.enabled)), Callback::new(move |v| f.update(|s| s.enabled = v)))}
            {module.then(|| view! {
                <dl class="kvs"><div class="kv"><dt>{tr("smb.interfaces")}</dt><dd class="wrap">{t(modules::ups_key(UpsSlot::NetListen))}</dd></div></dl>
            })}
            {(!module).then(|| segmented("smb.interfaces", &[("all", "upsn.listen_all"), ("chosen", "upsn.listen_chosen")], mode, Callback::new(move |v: String| f.update(|s| s.all = v == "all"))))}
            <Show when=move || !module && !f.with(|s| s.all)>
                <div class="field">
                    {addresses.iter().map(|a| {
                        let (a1, a2) = (a.clone(), a.clone());
                        view! {
                            <label class="check">
                                <input type="checkbox" prop:checked=move || f.with(|s| s.listen.contains(&a1)) on:change=move |ev| {
                                    let on = event_target_checked(&ev);
                                    let a = a2.clone();
                                    f.update(|s| { s.listen.retain(|x| x != &a); if on { s.listen.push(a); } });
                                } />
                                <span class="mono">{a.clone()}</span>
                            </label>
                        }
                    }).collect_view()}
                    <span class="field-help">{tr("upsn.listen_help")}</span>
                    <span class="field-error" aria-live="polite">
                        {move || listen_err.get().map(|e| view! { <Icon icon=icons::CIRCLE_X size=12 />{e} })}
                    </span>
                </div>
            </Show>
            {area_field("upsn.allow", allow_text, Callback::new(move |v| f.update(|s| s.allow = v)), Some(if module { modules::ups_key(UpsSlot::NetAllowHelp) } else { "upsn.allow_help" }), t("upsn.allow_ph"))}
            {(!subnets.is_empty()).then(|| view! {
                <div class="btn-row">
                    <button type="button" class="btn ghost sm" disabled=move || f.with(|s| nets.with_value(|n| add_hosts(&s.allow, n) == s.allow))
                        on:click=move |_| f.update(|s| s.allow = nets.with_value(|n| add_hosts(&s.allow, n)))>
                        <Icon icon=icons::PLUS size=14 />{tf("upsn.allow_lan", &[("nets", &subnets.join(", "))])}
                    </button>
                </div>
            })}
            <p class="sec-note"><Icon icon=icons::INFO size=16 />{t("upsn.read_note")}</p>
            <div class="btn-row">
                <button type="button" class="btn ghost sm" on:click=move |_| show_plan()><Icon icon=icons::FILE_TEXT size=14 />{tr("upsn.show_changes")}</button>
            </div>
            {move || plan.get().map(|p| plan_view(p, module))}
            {form_error(error)}
            <div class="dialog-foot">
                <button type="button" class="btn ghost" disabled=move || busy.get() on:click=move |_| cancel.run(())>{tr("cancel")}</button>
                <button type="button" class="btn primary" disabled=move || busy.get() on:click=move |_| submit()>{spinner(busy)}{tr("save")}</button>
            </div>
        </Modal>
    }
    .into_any()
}

fn plan_view(p: Result<UpsSetupPlan, String>, module: bool) -> AnyView {
    match p {
        Err(e) => view! { <p class="form-error"><Icon icon=icons::CIRCLE_X size=16 />{ui::err_view(e)}</p> }.into_any(),
        Ok(p) => view! {
            {p.files.into_iter().map(|f| view! {
                <h3 class="section-title mono wrap">{f.path}</h3>
                {(!f.removed.is_empty()).then(|| view! {
                    <p class="muted small">{t("upsw.removed")}</p>
                    <span class="pre-text log-text diff-old">{f.removed.join("\n")}</span>
                })}
                {if f.after.is_empty() {
                    view! { <p class="muted small">{t("upsn.file_removed")}</p> }.into_any()
                } else {
                    view! { <span class="pre-text log-text">{f.after}</span> }.into_any()
                }}
            }).collect_view()}
            <p class="muted small">{t(if module { modules::ups_key(UpsSlot::NetCommands) } else { "upsw.commands" })}</p>
            <span class="pre-text log-text">{p.commands.join("\n")}</span>
        }
        .into_any(),
    }
}

// ------------------------------------------------------------------ accounts

#[derive(Clone, PartialEq, Default)]
struct NewAccount {
    name: String,
    monitor: bool,
    note: String,
    own: bool,
    password: String,
}

fn add_dialog(state: Remote<UpsNetServerState>, on_close: Callback<Option<Shown>>) -> AnyView {
    let f = RwSignal::new(NewAccount::default());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let tried = RwSignal::new(false);
    let module = state.data.with_untracked(|s| s.as_ref().is_some_and(|s| s.layer.is_some()));
    let name_ok = move |n: &str| if module { modules::net_user_ok(n) } else { user_ok(n) };
    let name_err = Signal::derive(move || {
        let n = f.with(|a| a.name.trim().to_ascii_lowercase());
        let taken = state.data.with(|s| s.as_ref().is_some_and(|s| current(s).clients.iter().any(|c| c.name == n)));
        if n.is_empty() {
            tried.get().then(|| t("upsn.user_required").to_string())
        } else if !name_ok(&n) {
            Some(t(if module { modules::ups_key(UpsSlot::NetUserBad) } else { "upsn.user_bad" }).to_string())
        } else {
            taken.then(|| t("upsn.user_taken").to_string())
        }
    });
    let pw_err = Signal::derive(move || {
        let (own, p) = f.with(|a| (a.own, a.password.trim().to_string()));
        (own && (tried.get() || !p.is_empty()) && !password_ok(&p)).then(|| t("upsn.password_bad").to_string())
    });
    let submit = move || {
        tried.set(true);
        if name_err.with_untracked(Option::is_some) || pw_err.with_untracked(Option::is_some) {
            ui::focus_invalid();
            return;
        }
        let a = f.get_untracked();
        let mut cfg = state.data.get_untracked().map(|s| current(&s)).unwrap_or_default();
        let role = if a.monitor { UpsClientRole::Monitor } else { UpsClientRole::Secondary };
        let name = a.name.trim().to_ascii_lowercase();
        cfg.clients.push(UpsNetClient {
            name: name.clone(),
            role,
            note: a.note.trim().to_string(),
            password: a.own.then(|| a.password.trim().to_string()),
            generate: !a.own,
            sealed: None,
        });
        busy.set(true);
        spawn_local(async move {
            if let Some(pws) = save(cfg, error, None).await {
                let password = pws.into_iter().find(|p| p.name == name).map(|p| p.password);
                let _ = on_close.try_run(Some(Shown { name, role, password }));
            }
            let _ = busy.try_set(false);
        });
    };
    let role = Signal::derive(move || if f.with(|a| a.monitor) { "monitor".to_string() } else { "secondary".to_string() });
    let own = Signal::derive(move || f.with(|a| a.own));
    let none = Signal::stored(None::<String>);
    view! {
        <Modal title=Signal::stored(t("upsn.add_title").to_string()) on_close=Callback::new(move |_| on_close.run(None))>
            {text_field("username", Signal::derive(move || f.with(|a| a.name.clone())), Callback::new(move |v| f.update(|a| a.name = v)), name_err,
                In { mono: true, maxlength: 32, help: Some(if module { modules::ups_key(UpsSlot::NetUserHelp) } else { "upsn.user_help" }), placeholder: "homeassistant", ..Default::default() })}
            {segmented("upsn.role", &[("secondary", "upsn.role_secondary"), ("monitor", "upsn.role_monitor")], role, Callback::new(move |v: String| f.update(|a| a.monitor = v == "monitor")))}
            <p class="field-help">{move || t(match (f.with(|a| a.monitor), module) {
                (true, true) => modules::ups_key(UpsSlot::NetRoleMonitorHelp),
                (true, false) => "upsn.role_monitor_help",
                (false, _) => "upsn.role_secondary_help",
            })}</p>
            {text_field("upsn.note", Signal::derive(move || f.with(|a| a.note.clone())), Callback::new(move |v| f.update(|a| a.note = v)), none,
                In { maxlength: 64, placeholder: "Home Assistant", ..Default::default() })}
            {switch("upsn.own_password", Some("upsn.own_password_help"), own, Callback::new(move |v| f.update(|a| a.own = v)))}
            <Show when=move || own.get()>
                {text_field("upsn.password", Signal::derive(move || f.with(|a| a.password.clone())), Callback::new(move |v| f.update(|a| a.password = v)), pw_err,
                    In { kind: "password", mono: true, maxlength: 64, autocomplete: "new-password", help: Some("upsn.password_help"), ..Default::default() })}
            </Show>
            {form_error(error)}
            <div class="dialog-foot">
                <button type="button" class="btn ghost" disabled=move || busy.get() on:click=move |_| on_close.run(None)>{tr("cancel")}</button>
                <button type="button" class="btn primary" disabled=move || busy.get() on:click=move |_| submit()>{spinner(busy)}{tr("upsn.create")}</button>
            </div>
        </Modal>
    }
    .into_any()
}

/// Remove an account, or give it a new password (`regen`).
/// `on_close(None)`: cancelled; `Some(shown)`: done, with the new password of `regen`.
fn change_confirm(state: Remote<UpsNetServerState>, name: String, regen: bool, on_close: Callback<Option<Option<Shown>>>) -> AnyView {
    let c = Confirm::new(
        tf(if regen { "upsn.regen_title" } else { "upsn.remove_title" }, &[("name", &name)]),
        t(if regen { "upsn.regen_text" } else { "upsn.remove_text" }).to_string(),
        if regen { "pw.new" } else { "upsn.remove" },
        !regen,
        Callback::new(|_| {}),
        Callback::new(move |_| on_close.run(None)),
    );
    let (busy, error) = (c.busy, c.error);
    let go = Callback::new(move |_| {
        let mut cfg = state.data.get_untracked().map(|s| current(&s)).unwrap_or_default();
        let role = cfg.clients.iter().find(|c| c.name == name).map(|c| c.role).unwrap_or_default();
        if regen {
            cfg.clients.iter_mut().filter(|c| c.name == name).for_each(|c| c.generate = true);
        } else {
            cfg.clients.retain(|c| c.name != name);
        }
        let name = name.clone();
        busy.set(true);
        spawn_local(async move {
            if let Some(pws) = save(cfg, error, None).await {
                saved_applied();
                let password = pws.into_iter().find(|p| p.name == name).map(|p| p.password);
                let _ = on_close.try_run(Some(regen.then_some(Shown { name, role, password })));
            }
            let _ = busy.try_set(false);
        });
    });
    confirm(Confirm { on_confirm: go, ..c })
}

/// Connection details of an account; with its password right after it was set.
fn connect_dialog(state: Remote<UpsNetServerState>, s: Shown, on_close: Callback<()>) -> AnyView {
    let st = state.data.get_untracked().unwrap_or_default();
    let host = st.host.clone().unwrap_or_else(|| st.hostname.clone());
    let ups = st.ups_name.clone().unwrap_or_else(|| "ups".into());
    let port = st.port.to_string();
    let line = monitor_line(&ups, &host, &s.name, s.password.as_deref());
    let kv = |label: &'static str, value: String| {
        view! {
            <div class="kv"><dt>{tr(label)}</dt><dd class="mono wrap">{value.clone()}" "{copy_button("upsn.copy", value)}</dd></div>
        }
    };
    view! {
        <Modal title=Signal::stored(tf("upsn.connect_title", &[("name", &s.name)])) on_close=on_close>
            {s.password.clone().map(|pw| view! {
                {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {t("upsn.shown_once")} }.into_any())}
                <dl class="kvs">{kv("upsn.password", pw)}</dl>
            })}
            <h3 class="section-title">{tr("upsn.for_ha")}</h3>
            <p class="muted small">{t("upsn.for_ha_help")}</p>
            <dl class="kvs">
                {kv("upsn.host", host.clone())}
                {kv("upsn.port", port)}
                {kv("username", s.name.clone())}
                {kv("upsn.ups_name", ups.clone())}
            </dl>
            {if s.role == UpsClientRole::Secondary {
                view! {
                    <h3 class="section-title">{tr("upsn.for_upsmon")}</h3>
                    <p class="muted small">{t(if s.password.is_some() { "upsn.for_upsmon_help" } else { "upsn.for_upsmon_help_pw" })}</p>
                    <span class="pre-text log-text">{line.clone()}</span>
                    <div class="btn-row">{copy_button("upsn.copy", line)}</div>
                }.into_any()
            } else {
                view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{t("upsn.monitor_note")}</p> }.into_any()
            }}
            <div class="dialog-foot">
                <button type="button" class="btn primary" on:click=move |_| on_close.run(())>{tr("close")}</button>
            </div>
        </Modal>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_lines_and_lists() {
        assert_eq!(monitor_line("ups", "192.168.1.10", "pc", None), "MONITOR ups@192.168.1.10 1 pc PASSWORD secondary");
        assert_eq!(monitor_line("ups", "fd00::1", "pc", Some("pw")), "MONITOR ups@[fd00::1] 1 pc pw secondary");
        assert_eq!(parse_list("192.168.1.0/24\n 10.0.0.5, fd00::/64 "), ["192.168.1.0/24", "10.0.0.5", "fd00::/64"]);
        assert_eq!(role_key(UpsClientRole::Monitor), "upsn.role_monitor");
    }

    #[test]
    fn keys_exist() {
        for k in [
            "upsn.title", "upsn.intro", "upsn.on", "upsn.off", "upsn.pending", "upsn.need_nut", "upsn.module", "smb.interfaces", "upsn.listen_all",
            "upsn.listen_chosen", "upsn.listen_help", "upsn.allow", "upsn.allow_any", "upsn.allow_help", "upsn.allow_lan", "upsn.address", "upsn.ups_name",
            "upsn.open_warn", "upsn.accounts", "upsn.no_accounts", "upsn.settings", "upsn.settings_title", "upsn.add", "upsn.add_title", "username",
            "upsn.user_help", "upsn.role", "upsn.role_secondary", "upsn.role_monitor", "upsn.role_secondary_help", "upsn.role_monitor_help", "upsn.note",
            "upsn.own_password", "upsn.own_password_help", "upsn.password", "upsn.create", "upsn.connect", "upsn.connect_title", "pw.new",
            "upsn.remove", "upsn.remove_title", "upsn.remove_text", "upsn.regen_title", "upsn.regen_text", "upsn.enable",
            "upsn.enable_help", "upsn.read_note", "upsn.show_changes", "upsn.file_removed", "upsn.shown_once", "upsn.for_ha", "upsn.for_ha_help",
            "upsn.host", "upsn.port", "upsn.for_upsmon", "upsn.for_upsmon_help", "upsn.for_upsmon_help_pw", "upsn.monitor_note", "upsn.copy",
            "upsn.allow_none", "upsn.allow_required", "cfg.key.ups.netserver",
        ] {
            assert!(crate::i18n::try_t(k).is_some(), "{k}");
        }
    }
}
