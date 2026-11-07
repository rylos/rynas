//! Network › Remote access (`docs/phase2/remote-access.md`): Tailscale on this NAS.
//!
//! - Not installed: what it is and Install (the task's progress in place).
//! - Logged out: browser login (the address and its QR code once tailscaled has it, read
//!   again every 5 s while the login task runs) or an auth key that is never shown again.
//! - Connected: name, addresses, tailnet, devices, key expiry, version; the options of a
//!   server (accept routes and DNS off, exit node, LAN subnets, Tailscale SSH off); Log out
//!   and Uninstall.
//! - Anything that may end the session asks first, and says so plainly when this page itself
//!   is open over the tailnet. An option change is tested: the page confirms it as soon as it
//!   reaches the NAS after the task, otherwise rynasd puts the previous options back.

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::tailscale::{TailscaleBlock, TailscaleLogin, TailscaleOptions, TailscaleState, TailscaleStatus, TailscaleUninstall};

use crate::api;
use crate::form::{self, form_error, spinner, switch, text_field};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::qr::QrSvg;
use crate::tasks::TaskRun;
use crate::toast::toast;
use crate::ui::{self, Confirm, PageHeader, Remote, Tone, WidgetHead, confirm};

const URL: &str = "/api/v1/tailscale";

/// The page's data and the task it started.
#[derive(Clone, Copy)]
struct Ts {
    st: Remote<TailscaleStatus>,
    task: RwSignal<Option<String>>,
    /// An option change whose task is running: confirmed when it ends.
    confirm_after: RwSignal<bool>,
}

/// This page is open over the tailnet: rynasd saw a tailnet local address, or the browser
/// uses a `*.ts.net` name or one of the NAS's Tailscale addresses.
fn via_tailnet(s: &TailscaleStatus) -> bool {
    let host = window().location().hostname().unwrap_or_default();
    let host = host.trim_start_matches('[').trim_end_matches(']');
    s.via_tailscale || host.ends_with(".ts.net") || s.addresses.iter().any(|a| a == host) || s.dns_name.as_deref() == Some(host)
}

fn state_key(s: TailscaleState) -> (Tone, &'static str) {
    match s {
        TailscaleState::Running => (Tone::Ok, "ts.st.running"),
        TailscaleState::Stopped => (Tone::Warn, "ts.st.stopped"),
        TailscaleState::NeedsLogin => (Tone::Warn, "ts.st.needs_login"),
        TailscaleState::NeedsMachineAuth => (Tone::Warn, "ts.st.needs_machine_auth"),
        TailscaleState::Starting => (Tone::Info, "ts.st.starting"),
        TailscaleState::NoState => (Tone::Unknown, "ts.st.no_state"),
        TailscaleState::Unavailable => (Tone::Err, "ts.st.unavailable"),
    }
}

fn kv(label: &'static str, value: AnyView) -> AnyView {
    view! { <div class="kv"><dt>{tr(label)}</dt><dd>{value}</dd></div> }.into_any()
}

fn mono(s: String) -> AnyView {
    view! { <span class="mono">{s}</span> }.into_any()
}

#[component]
pub fn RemoteAccess() -> impl IntoView {
    let ts = Ts { st: Remote::new(URL), task: RwSignal::new(None), confirm_after: RwSignal::new(false) };
    // read again every 5 s while a login waits for its address or approval (no event
    // source for tailscaled's state), never in a hidden tab
    let poll = set_interval_with_handle(
        move || {
            let waiting = ts.st.data.with_untracked(|s| {
                s.as_ref().is_some_and(|s| s.task_id.is_some() || matches!(s.state, TailscaleState::Starting | TailscaleState::NeedsMachineAuth))
            });
            if waiting && !document().hidden() {
                ts.st.reload();
            }
        },
        Duration::from_secs(5),
    );
    if let Ok(h) = poll {
        on_cleanup(move || h.clear());
    }
    view! {
        <div class="page">
            <PageHeader icon=icons::LINK title="ts.title" subtitle="ts.subtitle">
                <a class="btn ghost" href="/network"><Icon icon=icons::ARROW_LEFT size=16 /><span class="btn-label">{tr("nav.network")}</span></a>
            </PageHeader>
            {move || ts.st.view(6, move || body(ts))}
        </div>
    }
}

fn body(ts: Ts) -> AnyView {
    let Some(s) = ts.st.data.get() else { return ().into_any() };
    // a task started here, or one the server reports (another tab, a reload)
    let running = ts.task.get().or(s.task_id.clone());
    let task = running.map(|id| {
        view! {
            <section class="card widget w-12">
                <TaskRun id=id on_done=Callback::new(move |t: rynas_api_types::TaskInfo| {
                    ts.task.set(None);
                    if ts.confirm_after.get_untracked() {
                        ts.confirm_after.set(false);
                        if t.status == rynas_api_types::TaskStatus::Ok {
                            confirm_options(ts);
                            return;
                        }
                    }
                    ts.st.reload();
                }) />
            </section>
        }
    });
    let content = if !s.installed {
        install_card(ts, &s)
    } else if !s.running {
        view! {
            <section class="card widget w-12">
                <WidgetHead icon=icons::LINK title="ts.status_title" />
                {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(t("ts.not_running").to_string()))}
                {actions(ts, &s)}
            </section>
        }
        .into_any()
    } else {
        match s.state {
            TailscaleState::NeedsLogin | TailscaleState::NoState => login_card(ts, &s),
            _ => connected(ts, &s),
        }
    };
    view! {
        {s.confirm.is_some().then(|| pending_confirm(ts))}
        <div class="grid">{task}{content}</div>
    }
    .into_any()
}

// ------------------------------------------------------------------ install

fn install_card(ts: Ts, s: &TailscaleStatus) -> AnyView {
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let block = s.install_block.map(|b| match b {
        TailscaleBlock::UnsupportedOs => "ts.block.unsupported_os",
        TailscaleBlock::UnknownRelease => "ts.block.unknown_release",
    });
    let install = move |_| {
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = api::start_task("/api/v1/tailscale/install", None).await;
            busy.set(false);
            match r {
                Ok(id) => ts.task.set(Some(id)),
                Err(e) => error.set(Some(e.text())),
            }
        });
    };
    view! {
        <section class="card widget w-12">
            <WidgetHead icon=icons::LINK title="ts.install_title" />
            <p class="muted">{tr("ts.install_text")}</p>
            {block.map(|k| ui::note(Tone::Info, icons::INFO, ui::text(t(k).to_string())))}
            {form_error(error)}
            <div class="sec-actions">
                <button class="btn primary" disabled=move || busy.get() || block.is_some() || ts.task.with(Option::is_some) on:click=install>
                    {spinner(busy)}<Icon icon=icons::DOWNLOAD size=16 />{tr("ts.install")}
                </button>
            </div>
        </section>
    }
    .into_any()
}

// ------------------------------------------------------------------ login

fn login_card(ts: Ts, s: &TailscaleStatus) -> AnyView {
    let key = RwSignal::new(String::new());
    let host = RwSignal::new(String::new());
    let server = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let field_err = RwSignal::new(None::<(String, String)>);
    let err_of = move |f: &'static str| Signal::derive(move || field_err.with(|e| e.as_ref().filter(|(n, _)| n == f).map(|(_, m)| m.clone())));
    let login = move |_| {
        busy.set(true);
        error.set(None);
        field_err.set(None);
        let opt = |v: String| (!v.trim().is_empty()).then(|| v.trim().to_string());
        let req = TailscaleLogin { auth_key: opt(key.get_untracked()), hostname: opt(host.get_untracked()), login_server: opt(server.get_untracked()) };
        spawn_local(async move {
            let r = api::start_task("/api/v1/tailscale/login", serde_json::to_string(&req).ok()).await;
            busy.set(false);
            match r {
                Ok(id) => {
                    // the key is gone from the page as soon as rynasd has it
                    key.set(String::new());
                    ts.task.set(Some(id));
                    ts.st.reload();
                }
                Err(e) => match e.field() {
                    Some(f) => field_err.set(Some((f.to_string(), e.text()))),
                    None => error.set(Some(e.text())),
                },
            }
        });
    };
    let url = s.auth_url.clone();
    let waiting = s.task_id.is_some() && url.is_none();
    view! {
        <section class="card widget w-12">
            <WidgetHead icon=icons::KEY title="ts.login_title" />
            {match url {
                Some(u) => view! {
                    <div class="ts-login">
                        <div class="ts-qr"><QrSvg text=u.clone() /></div>
                        <div>
                            <p>{tr("ts.login_url_text")}</p>
                            <p><a class="mono ts-url" href=u.clone() target="_blank" rel="noopener">{u.clone()}</a></p>
                            <a class="btn primary" href=u target="_blank" rel="noopener"><Icon icon=icons::EXTERNAL_LINK size=16 />{tr("ts.open_login")}</a>
                        </div>
                    </div>
                }.into_any(),
                None if waiting => view! { <p class="muted">{spinner(RwSignal::new(true))}{tr("ts.login_wait")}</p> }.into_any(),
                None => view! {
                    <p class="muted">{tr("ts.login_text")}</p>
                    <div class="fieldset">
                        {text_field("ts.auth_key", key.into(), Callback::new(move |v| key.set(v)), err_of("auth_key"),
                            form::In { kind: "password", autocomplete: "new-password", help: Some("ts.auth_key_help"), mono: true, ..Default::default() })}
                        {text_field("ts.hostname", host.into(), Callback::new(move |v| host.set(v)), err_of("hostname"),
                            form::In { help: Some("ts.hostname_help"), maxlength: 63, ..Default::default() })}
                        {text_field("ts.login_server", server.into(), Callback::new(move |v| server.set(v)), err_of("login_server"),
                            form::In { help: Some("ts.login_server_help"), placeholder: "https://", mono: true, ..Default::default() })}
                    </div>
                    {form_error(error)}
                    <div class="sec-actions">
                        <button class="btn primary" disabled=move || busy.get() || ts.task.with(Option::is_some) on:click=login>
                            {spinner(busy)}<Icon icon=icons::KEY size=16 />{tr("ts.login")}
                        </button>
                    </div>
                }.into_any(),
            }}
        </section>
        <section class="card widget w-12">{actions(ts, s)}</section>
    }
    .into_any()
}

// ------------------------------------------------------------------ connected

fn connected(ts: Ts, s: &TailscaleStatus) -> AnyView {
    let (tone, key) = state_key(s.state);
    let peers = tf("ts.peers_val", &[("online", &s.peers_online.to_string()), ("total", &s.peers.to_string())]);
    let expiry = match s.key_expiry_ms {
        Some(ms) => i18n::datetime(ms),
        None => t("ts.key_never").to_string(),
    };
    // within 14 days
    let soon = s.key_expiry_ms.filter(|&ms| (ms as f64) < js_sys::Date::now() + 14.0 * 86_400_000.0);
    let version = match (&s.version, &s.update_available) {
        (Some(v), Some(n)) => format!("{v} · {}", tf("ts.update", &[("v", n)])),
        (Some(v), None) => v.clone(),
        _ => "—".into(),
    };
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::LINK title="ts.status_title" />
            {(s.state == TailscaleState::NeedsMachineAuth).then(|| ui::note(Tone::Warn, icons::CLOCK, ui::text(t("ts.machine_auth").to_string())))}
            {(s.state == TailscaleState::Running && !s.online).then(|| ui::note(Tone::Warn, icons::UNPLUG, ui::text(t("ts.offline").to_string())))}
            {soon.map(|ms| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(tf("ts.key_soon", &[("date", &i18n::datetime(ms))]))))}
            {(!s.health.is_empty()).then(|| ui::note(Tone::Info, icons::INFO, view! { {tr("ts.health")}" "{ui::tech_detail(s.health.join(" · "))} }.into_any()))}
            <dl class="kvs">
                {kv("ts.state", ui::status(tone, t(key).to_string()))}
                {s.dns_name.clone().map(|n| kv("ts.dns_name", mono(n)))}
                {(!s.addresses.is_empty()).then(|| kv("ts.addresses", view! { <span class="wa-ranges mono small">{s.addresses.iter().map(|a| view! { <span>{a.clone()}</span> }).collect_view()}</span> }.into_any()))}
                {s.tailnet.clone().map(|n| kv("ts.tailnet", ui::text(n)))}
                {kv("ts.peers", ui::text(peers))}
                {kv("ts.key_expiry", ui::text(expiry))}
                {kv("ts.version", ui::text(version))}
            </dl>
        </section>
        <section class="card widget w-6">
            <WidgetHead icon=icons::SETTINGS title="ts.options_title" />
            {options_form(ts, s)}
        </section>
        <section class="card widget w-12">{actions(ts, s)}</section>
    }
    .into_any()
}

fn options_form(ts: Ts, s: &TailscaleStatus) -> AnyView {
    let o = s.options.clone();
    let accept_routes = RwSignal::new(o.accept_routes);
    let accept_dns = RwSignal::new(o.accept_dns);
    let exit_node = RwSignal::new(o.exit_node);
    let ssh = RwSignal::new(o.ssh);
    // the LAN subnets, plus any advertised one that is not (or no longer) on an interface
    let mut subnets = s.lan_subnets.clone();
    subnets.extend(o.advertise_routes.iter().filter(|r| !s.lan_subnets.contains(r)).cloned());
    let routes = RwSignal::new(o.advertise_routes.clone());
    let pending = s.routes_pending_approval.clone();
    let via = via_tailnet(s);
    let asking = RwSignal::new(false);
    let current = move || TailscaleOptions {
        accept_routes: accept_routes.get(),
        accept_dns: accept_dns.get(),
        exit_node: exit_node.get(),
        advertise_routes: routes.get(),
        ssh: ssh.get(),
    };
    let changed = {
        let o = o.clone();
        move || current() != o
    };
    let sw = |label: &'static str, help: &'static str, v: RwSignal<bool>| switch(label, Some(help), v.into(), Callback::new(move |x| v.set(x)));
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let go = Callback::new(move |_| {
        busy.set(true);
        error.set(None);
        let body = current();
        spawn_local(async move {
            let r = api::put::<TailscaleOptions, rynas_api_types::TaskStarted>("/api/v1/tailscale/options", &body).await.map(|t| t.task_id);
            busy.set(false);
            match r {
                Ok(id) => {
                    asking.set(false);
                    ts.confirm_after.set(true);
                    ts.task.set(Some(id));
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    });
    let dialog = move || {
        let mut c = Confirm::new(t("ts.confirm_title").to_string(), t("ts.confirm_text").to_string(), "save", false, go, Callback::new(move |_| asking.set(false)));
        c.extra = via.then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(t("ts.via_warning").to_string())));
        c.busy = busy;
        c.error = error;
        confirm(c)
    };
    view! {
        <div class="ts-options">
            {sw("ts.opt.accept_routes", "ts.opt.accept_routes_help", accept_routes)}
            {sw("ts.opt.accept_dns", "ts.opt.accept_dns_help", accept_dns)}
            {sw("ts.opt.exit_node", "ts.opt.exit_node_help", exit_node)}
            <fieldset class="fieldset">
                <legend class="field-label">{tr("ts.opt.routes")}</legend>
                <span class="field-help">{tr("ts.opt.routes_help")}</span>
                {if subnets.is_empty() {
                    view! { <span class="muted small">{tr("ts.opt.no_lan")}</span> }.into_any()
                } else {
                    subnets.into_iter().map(|net| {
                        let n2 = net.clone();
                        let n3 = net.clone();
                        view! {
                            <label class="check">
                                <input type="checkbox" prop:checked=move || routes.with(|r| r.contains(&n2))
                                    on:change=move |ev| {
                                        let on = event_target_checked(&ev);
                                        let n = n3.clone();
                                        routes.update(|r| if on { if !r.contains(&n) { r.push(n) } } else { r.retain(|x| *x != n) });
                                    } />
                                <span class="mono">{net}</span>
                            </label>
                        }
                    }).collect_view().into_any()
                }}
            </fieldset>
            {sw("ts.opt.ssh", "ts.opt.ssh_help", ssh)}
            {(!pending.is_empty()).then(|| ui::note(Tone::Info, icons::CLOCK, ui::text(tf("ts.routes_pending", &[("routes", &pending.join(", "))]))))}
            <div class="sec-actions">
                <button class="btn primary" disabled=move || !changed() || ts.task.with(Option::is_some) on:click=move |_| asking.set(true)>{tr("save")}</button>
            </div>
            {move || asking.get().then(dialog)}
        </div>
    }
    .into_any()
}

/// The page reached rynas after the option task: the new options keep (this request is
/// the proof the session survived them).
fn confirm_options(ts: Ts) {
    spawn_local(async move {
        let r = api::post_empty::<()>("/api/v1/tailscale/options/confirm", None).await;
        match r {
            Ok(_) => toast(Tone::Ok, t("ts.saved").to_string()),
            Err(e) => toast(Tone::Err, format!("{} {}", t("ts.confirm_failed"), e.text())),
        }
        ts.st.reload();
    });
}

fn pending_confirm(ts: Ts) -> AnyView {
    ui::note(Tone::Warn, icons::TIMER, view! {
        <span>{tr("ts.pending_confirm")}" "</span>
        <button class="btn ghost sm" on:click=move |_| confirm_options(ts)>{tr("ts.keep")}</button>
    }.into_any())
}

// ------------------------------------------------------------------ log out, uninstall

fn actions(ts: Ts, s: &TailscaleStatus) -> AnyView {
    let via = via_tailnet(s);
    let logged_in = s.running && !matches!(s.state, TailscaleState::NeedsLogin | TailscaleState::NoState);
    let which = RwSignal::new(None::<&'static str>);
    let purge = RwSignal::new(false);
    let start = move |url: &'static str, body: Option<String>, c_busy: RwSignal<bool>, c_err: RwSignal<Option<String>>| {
        c_busy.set(true);
        c_err.set(None);
        spawn_local(async move {
            let r = api::start_task(url, body).await;
            c_busy.set(false);
            match r {
                Ok(id) => {
                    which.set(None);
                    ts.task.set(Some(id));
                }
                Err(e) => c_err.set(Some(e.text())),
            }
        });
    };
    let warn = move || via.then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(t("ts.via_warning").to_string())));
    let dialog = move || {
        which.get().map(|w| {
            let cancel = Callback::new(move |_| which.set(None));
            if w == "logout" {
                let mut c = Confirm::new(t("ts.logout_title").to_string(), t("ts.logout_text").to_string(), "ts.logout", true, Callback::new(|_| {}), cancel);
                c.extra = Some(view! { {warn()} }.into_any());
                let (b, e) = (c.busy, c.error);
                c.on_confirm = Callback::new(move |_| start("/api/v1/tailscale/logout", None, b, e));
                confirm(c)
            } else {
                let mut c = Confirm::new(t("ts.uninstall_title").to_string(), t("ts.uninstall_text").to_string(), "ts.uninstall", true, Callback::new(|_| {}), cancel);
                c.extra = Some(view! {
                    {warn()}
                    {switch("ts.purge", None, purge.into(), Callback::new(move |v| purge.set(v)))}
                }.into_any());
                c.typed = Signal::stored(Some("tailscale".to_string()));
                let (b, e) = (c.busy, c.error);
                c.on_confirm = Callback::new(move |_| {
                    let body = serde_json::to_string(&TailscaleUninstall { purge_state: purge.get_untracked() }).ok();
                    start("/api/v1/tailscale/uninstall", body, b, e)
                });
                confirm(c)
            }
        })
    };
    let idle = move || ts.task.with(Option::is_none);
    view! {
        <div class="sec-actions">
            {logged_in.then(|| view! {
                <button class="btn ghost" disabled=move || !idle() on:click=move |_| which.set(Some("logout"))><Icon icon=icons::LOG_OUT size=16 />{tr("ts.logout")}</button>
            })}
            <button class="btn ghost danger-text" disabled=move || !idle() on:click=move |_| which.set(Some("uninstall"))>{tr("ts.uninstall")}</button>
        </div>
        {dialog}
    }
    .into_any()
}

/// The line about Tailscale on the web access overview: whether "This network and VPN"
/// admits tailnet devices right now, and the way to the page above.
pub fn web_access_note() -> AnyView {
    let st = Remote::<TailscaleStatus>::new(URL);
    view! {
        {move || st.data.get().map(|s| {
            let on = s.state == TailscaleState::Running;
            let text = match (&s.dns_name, on) {
                (Some(n), true) => tf("ts.wa_on", &[("name", n)]),
                _ => t("ts.wa_off").to_string(),
            };
            ui::note(Tone::Info, icons::LINK, view! { <span>{text}" "<a href="/network/remote-access">{tr("ts.title")}</a></span> }.into_any())
        })}
    }
    .into_any()
}
