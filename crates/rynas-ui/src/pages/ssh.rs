//! The SSH service (Services › SSH, D-SSH): settings with a preview of who may log in, the
//! trial banner of a change that can shut someone out (keep it or it goes back by itself),
//! host key fingerprints, the keys of each account and the logins of the last day.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::ssh::*;

use crate::api;
use crate::form::{Draft, In, lens, lens_bool, select_field, stale_notice, switch, text_field};
use crate::i18n::{t, tf, tr};
use crate::icons::{self, Icon};
use crate::toast::toast;
use crate::ui::{self, Confirm, Meter, PageHeader, Remote, Skeleton, Tone, WidgetHead, confirm};

const URL: &str = "/api/v1/services/ssh";
const ROOT_OPTS: &[(&str, &str)] = &[("no", "ssh.root_no"), ("keys", "ssh.root_keys"), ("yes", "ssh.root_yes")];
const ALLOW_OPTS: &[(&str, &str)] = &[("everyone", "ssh.allow_everyone"), ("listed", "ssh.allow_listed")];

fn root_str(r: SshRootLogin) -> String {
    match r {
        SshRootLogin::No => "no",
        SshRootLogin::Keys => "keys",
        SshRootLogin::Yes => "yes",
    }
    .into()
}

fn host() -> String {
    web_sys::window().and_then(|w| w.location().hostname().ok()).unwrap_or_else(|| "nas".into())
}

fn me() -> String {
    crate::auth::auth().session.with_untracked(|s| s.as_ref().map(|s| s.user.username.clone())).unwrap_or_else(|| "admin".into())
}

/// `ssh [-p N] user@host`.
fn command(port: u16) -> String {
    let p = if port == 22 { String::new() } else { format!("-p {port} ") };
    format!("ssh {p}{}@{}", me(), host())
}

fn problem_text(p: &SshProblem) -> String {
    let s = p.subject.clone().unwrap_or_default();
    match p.code {
        SshProblemCode::NotInstalled => t("ssh.p.not_installed").to_string(),
        SshProblemCode::NotIncluded => t("ssh.p.not_included").to_string(),
        SshProblemCode::Overridden => {
            let (key, file) = s.split_once(' ').unwrap_or((s.as_str(), "?"));
            tf("ssh.p.overridden", &[("key", key), ("file", file)])
        }
        SshProblemCode::ExtraPorts => tf("ssh.p.extra_ports", &[("ports", &s)]),
        SshProblemCode::Drift => t("ssh.p.drift").to_string(),
        SshProblemCode::SocketActivation => t("ssh.p.socket").to_string(),
        SshProblemCode::NotRunning => t("ssh.p.not_running").to_string(),
    }
}

/// The page's shared state: the server state and the change under test.
#[derive(Clone, Copy)]
struct Page {
    st: Remote<SshState>,
    /// Local time of the deadline of my own trial (`Date.now()` + timeout at the answer).
    local_deadline: RwSignal<Option<f64>>,
    now: RwSignal<f64>,
    /// The account of the keys card: kept across the reloads that rebuild the card.
    key_user: RwSignal<Option<String>>,
}

#[component]
pub fn SshPage() -> impl IntoView {
    let pg = Page {
        st: Remote::<SshState>::new(URL),
        local_deadline: RwSignal::new(None),
        now: RwSignal::new(js_sys::Date::now()),
        key_user: RwSignal::new(None),
    };
    // the accounts of the keys card: a reload after a key change (other counts, same names)
    // does not rebuild it
    let key_names = Memo::new(move |_| {
        pg.st.data.with(|s| {
            s.as_ref()
                .map(|s| s.accounts.iter().filter(|a| a.level != SshLevel::None || a.uid == 0).map(|a| a.name.clone()).collect::<Vec<_>>())
                .unwrap_or_default()
        })
    });
    // the countdown and a read every 5 s while a change waits
    let tick = StoredValue::new(0u32);
    if let Ok(h) = set_interval_with_handle(
        move || {
            if pg.st.data.with_untracked(|s| s.as_ref().is_some_and(|s| s.trial.is_some())) || pg.local_deadline.with_untracked(Option::is_some) {
                pg.now.set(js_sys::Date::now());
                let n = tick.get_value().wrapping_add(1);
                tick.set_value(n);
                if n.is_multiple_of(5) {
                    pg.st.reload();
                }
            }
        },
        std::time::Duration::from_secs(1),
    ) {
        on_cleanup(move || h.clear());
    }
    view! {
        <div class="page">
            <PageHeader icon=icons::COMMAND title="ssh.title" subtitle="ssh.subtitle" />
            {move || trial_banner(pg)}
            <div class="grid">
                <section class="card widget w-12">
                    <WidgetHead icon=icons::SETTINGS title="ssh.settings">
                        {move || pg.st.data.with(|s| s.as_ref().map(status_pill))}
                    </WidgetHead>
                    {move || pg.st.gate(8).unwrap_or_else(|| {
                        let s = pg.st.data.get_untracked().unwrap_or_default();
                        if !s.installed {
                            return ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text_code(t("ssh.p.not_installed"))).into_any();
                        }
                        settings_form(pg, s.settings)
                    })}
                </section>
                <div class="w-6 col-stack">
                    <section class="card widget">
                        <WidgetHead icon=icons::KEY title="ssh.keys" />
                        {move || pg.st.gate(4).unwrap_or_else(|| keys_card(pg, key_names.get()))}
                    </section>
                    <section class="card widget">
                        <WidgetHead icon=icons::SHIELD_CHECK title="ssh.host_keys" />
                        {move || pg.st.gate(3).unwrap_or_else(|| host_keys(pg))}
                    </section>
                </div>
                <section class="card widget w-6 self-start">
                    <WidgetHead icon=icons::SCROLL_TEXT title="ssh.logins" />
                    {logins()}
                </section>
            </div>
        </div>
    }
}

/// Off, stopped or running (on which ports): the settings card and the Services page.
pub(crate) fn status_pill(s: &SshState) -> AnyView {
    match (s.installed, s.settings.enabled, s.running) {
        (false, _, _) => ui::status(Tone::Unknown, t("wa.eng.not_installed").to_string()),
        (_, _, true) => ui::status(Tone::Ok, tf("ssh.running_on", &[("ports", &s.ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", "))])),
        (_, true, false) => ui::status(Tone::Warn, t("svc.stopped").to_string()),
        (_, false, false) => ui::status(Tone::Unknown, t("svc.off").to_string()),
    }
}

// ------------------------------------------------------------------ trial

fn clock(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn trial_banner(pg: Page) -> Option<AnyView> {
    let trial = pg.st.data.with(|s| s.as_ref().and_then(|s| s.trial.clone()))?;
    let mine = !trial.token.is_empty();
    let deadline = pg.local_deadline.get().filter(|_| mine).unwrap_or(trial.deadline_ms as f64);
    let left = Memo::new(move |_| ((deadline - pg.now.get()) / 1000.0).max(0.0).ceil() as u64);
    let timeout = trial.timeout_s.max(1) as f64;
    let pct = Signal::derive(move || left.get() as f64 * 100.0 / timeout);
    let tone = Signal::derive(move || if left.get() <= 30 { Tone::Err } else { Tone::Warn });
    let spoken = Memo::new(move |_| left.get().div_ceil(30) * 30);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let token = trial.token.clone();
    let keep = move |_| {
        let token = token.clone();
        busy.set(true);
        spawn_local(async move {
            let r = api::post_empty(&format!("{URL}/confirm"), Some(&SshConfirm { token })).await;
            busy.set(false);
            match r {
                Ok(()) => {
                    pg.local_deadline.set(None);
                    toast(Tone::Ok, t("ssh.kept").to_string());
                }
                Err(e) => error.set(Some(e.text())),
            }
            pg.st.reload();
        });
    };
    let undo = move |_| {
        busy.set(true);
        spawn_local(async move {
            let r = api::post_empty::<()>(&format!("{URL}/rollback"), None).await;
            busy.set(false);
            match r {
                Ok(()) => {
                    pg.local_deadline.set(None);
                    toast(Tone::Info, t("ssh.undone").to_string());
                }
                Err(e) => error.set(Some(e.text())),
            }
            pg.st.reload();
        });
    };
    let cmd = command(trial.port);
    Some(
        view! {
            <section class="card net-trial" aria-labelledby="ssh-trial-h">
                <div class="net-trial-head">
                    <span class="net-trial-icon"><Icon icon=icons::TIMER size=22 /></span>
                    <div class="net-trial-text">
                        <h2 id="ssh-trial-h">{tr("ssh.trial_title")}</h2>
                        <p>{if mine { t("ssh.trial_text").to_string() } else { tf("ssh.trial_other", &[("user", &trial.owner)]) }}</p>
                    </div>
                    <div class="net-trial-count tnum" class:urgent=move || left.get() <= 30 aria-hidden="true">{move || clock(left.get())}</div>
                    <span class="sr-only" aria-live="polite">{move || tf("ssh.left_sr", &[("n", &spoken.get().to_string())])}</span>
                </div>
                <Meter pct=pct tone=tone />
                {mine.then(|| view! {
                    <div class="repl-copy">
                        <code class="secret mono ssh-cmd">{cmd.clone()}</code>
                        {crate::pages::security::copy_button("ssh.copy", cmd.clone())}
                    </div>
                })}
                {crate::form::form_error(error)}
                <div class="net-trial-actions">
                    {mine.then(|| view! {
                        <button class="btn primary lg" disabled=move || busy.get() on:click=keep.clone()>
                            {crate::form::spinner(busy)}<Icon icon=icons::CHECK size=18 />{tr("ssh.keep")}
                        </button>
                    })}
                    <button class="btn ghost lg" disabled=move || busy.get() on:click=undo>
                        <Icon icon=icons::ROTATE_CCW size=18 />{tr("ssh.undo")}
                    </button>
                </div>
            </section>
        }
        .into_any(),
    )
}

// ------------------------------------------------------------------ settings

fn level_key(l: SshLevel) -> &'static str {
    match l {
        SshLevel::Shell => "ssh.level_shell",
        SshLevel::Sftp => "ssh.level_sftp",
        SshLevel::None => "ssh.level_none",
    }
}

fn settings_form(pg: Page, init: SshSettings) -> AnyView {
    let server = Signal::derive(move || pg.st.data.with(|s| s.as_ref().map(|s| s.settings.clone())));
    let draft = Draft::new(init, move || server.get());
    let d = draft.d;
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let (enabled, set_enabled) = lens_bool(d, |s| s.enabled, |s, v| s.enabled = v);
    let (port, set_port) = lens(d, |s| s.port.to_string(), |s, v| s.port = v.trim().parse().unwrap_or(0));
    let (pw, set_pw) = lens_bool(d, |s| s.password_auth, |s, v| s.password_auth = v);
    let (root, set_root) = lens(d, |s| root_str(s.root_login), |s, v| {
        s.root_login = match v.as_str() {
            "yes" => SshRootLogin::Yes,
            "keys" => SshRootLogin::Keys,
            _ => SshRootLogin::No,
        }
    });
    let (allow, set_allow) = lens(d, |s| if s.allow == SshAllow::Listed { "listed".into() } else { "everyone".into() }, |s, v| {
        s.allow = if v == "listed" { SshAllow::Listed } else { SshAllow::Everyone }
    });
    let (groups, set_groups) = lens(d, |s| s.allow_groups.join(", "), |s, v| {
        s.allow_groups = v.split([',', ' ']).map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect()
    });
    let (tcp, set_tcp) = lens_bool(d, |s| s.tcp_forwarding, |s, v| s.tcp_forwarding = v);
    let (agent, set_agent) = lens_bool(d, |s| s.agent_forwarding, |s, v| s.agent_forwarding = v);
    let (x11, set_x11) = lens_bool(d, |s| s.x11_forwarding, |s, v| s.x11_forwarding = v);
    let port_err = Signal::derive(move || (port.with(|p| p.trim().parse::<u16>().map(|n| n == 0).unwrap_or(true))).then(|| t("repl.v.port").to_string()));
    let none = Signal::derive(|| None::<String>);
    let dirty = draft.dirty();
    let accounts = Memo::new(move |_| pg.st.data.with(|s| s.as_ref().map(|s| s.accounts.clone()).unwrap_or_default()));
    // the same rule as the server's lockout guard, live
    let admin_in = Signal::derive(move || d.with(|s| accounts.with(|a| a.iter().any(|x| admin_can_log_in(x, s)))));
    let admin_in_now = Signal::derive(move || server.with(|s| s.as_ref().is_some_and(|s| accounts.with(|a| a.iter().any(|x| admin_can_log_in(x, s))))));
    let lockout = Signal::derive(move || d.with(|s| s.enabled) && admin_in_now.get() && !admin_in.get());
    let reset = Callback::new(move |_| {
        draft.reset();
        error.set(None);
    });
    let save = move || {
        if port_err.get_untracked().is_some() {
            error.set(Some(t("form.check").to_string()));
            return;
        }
        busy.set(true);
        error.set(None);
        let body = SshApply { settings: d.get_untracked(), timeout_s: None };
        spawn_local(async move {
            let started = js_sys::Date::now();
            let r = api::put::<_, SshApplied>(URL, &body).await;
            match r {
                Ok(a) => match a.trial {
                    Some(t) => pg.local_deadline.set(Some(started + f64::from(t.timeout_s) * 1000.0)),
                    None => toast(Tone::Ok, t("ssh.saved").to_string()),
                },
                Err(e) => error.set(Some(e.text())),
            }
            let _ = busy.try_set(false);
            pg.st.reload();
        });
    };
    let problems = move || {
        pg.st.data.with(|s| {
            s.as_ref()
                .map(|s| s.problems.iter().map(|p| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text_code(&problem_text(p)))).collect_view())
        })
    };
    let preview = move || {
        let s = d.get();
        let rows = accounts
            .get()
            .into_iter()
            .filter(|a| a.uid != 0 || s.root_login != SshRootLogin::No)
            .map(|a| {
                let ok = s.enabled && a.level != SshLevel::None && passes(&a.groups, &s) && (a.keys > 0 || (s.password_auth && (a.uid != 0 || s.root_login == SshRootLogin::Yes)));
                let keys = if a.keys_unread { t("ssh.keys_unread").to_string() } else { tf("ssh.n_keys", &[("n", &a.keys.to_string())]) };
                let mut sub = vec![t(level_key(a.level)).to_string(), keys];
                if a.admin {
                    sub.insert(0, t("ssh.admin").to_string());
                }
                view! {
                    <li class="ssh-acct">
                        <Icon icon=if ok { icons::CIRCLE_CHECK } else { icons::CIRCLE_X } size=16 />
                        {ui::two_lines(ui::text(a.name.clone()), Some(sub.join(" · ")), "cell-sub muted small")}
                    </li>
                }
            })
            .collect_view();
        view! { <ul class="plain-list ssh-accts">{rows}</ul> }
    };
    view! {
        <form class="form" on:submit=move |ev| { ev.prevent_default(); save(); }>
            {problems}
            {switch("ssh.enabled", Some("ssh.enabled_help"), enabled, set_enabled)}
            <div class="form" hidden=move || !enabled.get()>
                <div class="form-row">
                    {text_field("ssh.port", port, set_port, port_err, In { kind: "number", mono: true, maxlength: 5, help: Some("ssh.port_help"), ..In::default() })}
                    {select_field("ssh.root", Some("ssh.root_help"), ROOT_OPTS, root, set_root)}
                </div>
                {switch("ssh.password", Some("ssh.password_help"), pw, set_pw)}
                {select_field("ssh.allow", None, ALLOW_OPTS, allow, set_allow)}
                <p class="field-help">{move || if allow.get() == "listed" { t("ssh.allow_listed_help") } else { t("ssh.allow_everyone_help") }}</p>
                <div hidden=move || allow.get() != "listed">
                    {text_field("ssh.allow_groups", groups, set_groups, none, In { help: Some("ssh.allow_groups_help"), mono: true, placeholder: "sudo", ..In::default() })}
                </div>
                <div class="field">
                    <span class="field-label">{tr("ssh.who")}</span>
                    {preview}
                </div>
                {switch("ssh.tcp", Some("ssh.tcp_help"), tcp, set_tcp)}
                {switch("ssh.agent", Some("ssh.agent_help"), agent, set_agent)}
                {switch("ssh.x11", Some("ssh.x11_help"), x11, set_x11)}
            </div>
            {move || lockout.get().then(|| ui::note(Tone::Err, icons::SHIELD_ALERT, ui::text(t("ssh.lockout").to_string())))}
            {move || (dirty.get() && !lockout.get()).then(|| ui::note(Tone::Info, icons::TIMER, ui::text(t("ssh.trial_note").to_string())))}
            {stale_notice(draft.stale, reset)}
            {crate::form::form_error(error)}
            <div class="sec-actions">
                <button type="submit" class="btn primary" disabled=move || busy.get() || !dirty.get() || lockout.get()>{crate::form::spinner(busy)}{tr("save")}</button>
                <button type="button" class="btn ghost" disabled=move || !dirty.get() on:click=move |_| reset.run(())>{tr("reset")}</button>
            </div>
        </form>
    }
    .into_any()
}

// ------------------------------------------------------------------ host keys

fn host_keys(pg: Page) -> AnyView {
    let keys = pg.st.data.with(|s| s.as_ref().map(|s| s.host_keys.clone()).unwrap_or_default());
    if keys.is_empty() {
        return ui::note(Tone::Info, icons::INFO, ui::text(t("ssh.no_host_keys").to_string()));
    }
    view! {
        <p class="muted small">{tr("ssh.host_keys_help")}</p>
        <ul class="plain-list ssh-hostkeys">
            {keys.into_iter().map(|k| view! {
                <li class="ssh-key">
                    <div class="ssh-key-text">
                        <span class="cell-main">{format!("{} {}", k.kind, k.bits)}</span>
                        <code class="cell-sub muted small mono ssh-fp">{k.fingerprint}</code>
                    </div>
                </li>
            }).collect_view()}
        </ul>
    }
    .into_any()
}

// ------------------------------------------------------------------ logins

fn kind_key(k: SshLoginKind) -> &'static str {
    match k {
        SshLoginKind::Accepted => "ssh.login_ok",
        SshLoginKind::Failed => "ssh.login_failed",
        SshLoginKind::InvalidUser => "ssh.login_invalid",
    }
}

/// One line of the grouped view: the same account, address, method and outcome.
struct LoginGroup {
    kind: SshLoginKind,
    user: String,
    address: String,
    method: String,
    count: u32,
    /// The newest one (events come newest first).
    last_ms: u64,
}

/// Groups the events (newest first): failures before logins, then by the newest one.
fn group_logins(events: &[SshLoginEvent]) -> Vec<LoginGroup> {
    let mut groups: Vec<LoginGroup> = Vec::new();
    for e in events {
        match groups.iter_mut().find(|g| g.kind == e.kind && g.user == e.user && g.address == e.address && g.method == e.method) {
            Some(g) => g.count += 1,
            None => groups.push(LoginGroup {
                kind: e.kind,
                user: e.user.clone(),
                address: e.address.clone(),
                method: e.method.clone(),
                count: 1,
                last_ms: e.at_ms,
            }),
        }
    }
    // stable: within each outcome the order stays newest first
    groups.sort_by_key(|g| g.kind == SshLoginKind::Accepted);
    groups
}

fn logins() -> AnyView {
    let r = Remote::<SshLogins>::new(format!("{URL}/logins"));
    let timeline = RwSignal::new(false);
    r.view(4, move || {
        let l = r.data.get().unwrap_or_default();
        let total = l.events.len();
        let top = l.top_failed.iter().map(|(a, n)| format!("{a} ({n})")).collect::<Vec<_>>().join(", ");
        let partial = (l.accepted + l.failed) as usize > total;
        let list = if timeline.get() {
            // one compact line per entry, newest first
            view! {
                <ul class="plain-list ssh-timeline">
                    {l.events.into_iter().map(|e| {
                        let ok = e.kind == SshLoginKind::Accepted;
                        view! {
                            <li class="ssh-ev ssh-login" class:bad=!ok title=t(kind_key(e.kind))>
                                <Icon icon=if ok { icons::CIRCLE_CHECK } else { icons::CIRCLE_X } size=14 />
                                <span class="ssh-ev-when muted mono">{crate::i18n::datetime(e.at_ms)}</span>
                                <span class="ssh-ev-who">{e.user}</span>
                                <span class="muted">{e.address}</span>
                                {(!e.method.is_empty()).then(|| view! { <span class="muted">{e.method}</span> })}
                            </li>
                        }
                    }).collect_view()}
                </ul>
            }
            .into_any()
        } else {
            view! {
                <ul class="plain-list ssh-events">
                    {group_logins(&l.events).into_iter().map(|g| {
                        let ok = g.kind == SshLoginKind::Accepted;
                        let times = if g.count == 1 { t("ssh.logins_once").to_string() } else { tf("nt.times", &[("n", &g.count.to_string())]) };
                        let method = if g.method.is_empty() { String::new() } else { format!(" · {}", g.method) };
                        let sub = format!("{times}{method} · {}", tf("ssh.logins_last", &[("when", &crate::i18n::datetime(g.last_ms))]));
                        let main = if ok { format!("{} · {}", g.user, g.address) } else { format!("{} · {} — {}", g.user, g.address, t(kind_key(g.kind))) };
                        view! {
                            <li class="ssh-acct ssh-login" class:bad=!ok>
                                <Icon icon=if ok { icons::CIRCLE_CHECK } else { icons::CIRCLE_X } size=16 />
                                {ui::two_lines(ui::text(main), Some(sub), "cell-sub muted small")}
                            </li>
                        }
                    }).collect_view()}
                </ul>
            }
            .into_any()
        };
        view! {
            <div class="ssh-login-stats">
                {ui::status(Tone::Ok, tf("ssh.logins_ok", &[("n", &l.accepted.to_string())]))}
                {ui::status(if l.failed == 0 { Tone::Unknown } else { Tone::Warn }, tf("ssh.logins_failed", &[("n", &l.failed.to_string())]))}
            </div>
            {(!top.is_empty()).then(|| view! { <p class="muted small">{tf("ssh.logins_top", &[("list", &top)])}</p> })}
            {l.since_ms.map(|at| view! { <p class="muted small">{tf("ssh.logins_since", &[("when", &crate::i18n::datetime(at))])}</p> })}
            {(partial && total != 0).then(|| view! { <p class="muted small">{tf("ssh.logins_recent", &[("n", &total.to_string())])}</p> })}
            {list}
            <div class="card-actions">
                <button type="button" class="btn ghost sm" on:click=move |_| r.reload()><Icon icon=icons::REFRESH_CW size=14 />{tr("ssh.refresh")}</button>
                {(total > 1).then(|| view! {
                    <button type="button" class="btn ghost sm" aria-pressed=move || timeline.get().to_string() on:click=move |_| timeline.update(|a| *a = !*a)>
                        {move || if timeline.get() { t("ssh.logins_grouped").to_string() } else { t("ssh.logins_timeline").to_string() }}
                    </button>
                })}
            </div>
        }
        .into_any()
    })
}

// ------------------------------------------------------------------ keys

/// The key a removal waits to confirm: account, fingerprint and the name shown.
#[derive(Clone)]
struct Removal {
    user: String,
    fingerprint: String,
    title: String,
}

fn keys_card(pg: Page, names: Vec<String>) -> AnyView {
    if names.is_empty() {
        return ui::note(Tone::Info, icons::INFO, ui::text(t("ssh.no_accounts").to_string()));
    }
    // the account chosen before (still listed), else my own, else the first that is not root
    let first = pg
        .key_user
        .get_untracked()
        .filter(|u| names.contains(u))
        .or_else(|| names.iter().find(|n| **n == me()).cloned())
        .or_else(|| names.iter().find(|n| *n != "root").cloned())
        .unwrap_or_else(|| names[0].clone());
    let who = RwSignal::new(first);
    let list = RwSignal::new(None::<Result<SshKeyList, String>>);
    let load = move || {
        let u = who.get_untracked();
        spawn_local(async move {
            let r = api::get::<SshKeyList>(&format!("{URL}/keys/{}", api::encode(&u))).await.map_err(|e| e.text());
            // an answer for an account no longer selected is dropped
            if who.try_with_untracked(|w| *w == u).unwrap_or(false) {
                let _ = list.try_set(Some(r));
            }
        });
    };
    Effect::new(move |_| {
        pg.key_user.set(Some(who.get()));
        load();
    });
    let new_key = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let add = move || {
        if busy.get_untracked() {
            return;
        }
        let (u, key) = (who.get_untracked(), new_key.get_untracked());
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = api::post::<_, SshKey>(&format!("{URL}/keys/{}", api::encode(&u)), Some(&SshKeyAdd { key })).await;
            let _ = busy.try_set(false);
            match r {
                Ok(_) => {
                    new_key.set(String::new());
                    toast(Tone::Ok, t("ssh.key_added").to_string());
                    load();
                    pg.st.reload();
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    };
    let del = RwSignal::new(None::<Removal>);
    let deleter = move || {
        del.get().map(|k| {
            let busy = RwSignal::new(false);
            let error = RwSignal::new(None::<String>);
            let close = Callback::new(move |_| del.set(None));
            let title = tf("ssh.key_remove_title", &[("name", &k.title)]);
            confirm(Confirm {
                title,
                text: tf("ssh.key_remove_text", &[("user", &k.user)]),
                action: "ssh.key_remove",
                danger: true,
                extra: None,
                typed: Signal::stored(None),
                busy,
                error,
                on_confirm: Callback::new(move |_| {
                    if busy.get_untracked() {
                        return;
                    }
                    busy.set(true);
                    let k = k.clone();
                    spawn_local(async move {
                        let url = format!("{URL}/keys/{}/remove", api::encode(&k.user));
                        let r = api::post_empty(&url, Some(&SshKeyRemove { fingerprint: k.fingerprint })).await;
                        let _ = busy.try_set(false);
                        match r {
                            Ok(()) => {
                                toast(Tone::Ok, t("ssh.key_removed").to_string());
                                close.run(());
                                load();
                                pg.st.reload();
                            }
                            Err(e) => {
                                let _ = error.try_set(Some(e.text()));
                            }
                        }
                    });
                }),
                on_cancel: close,
            })
        })
    };
    view! {
        <div class="form">
            <div class="field">
                <label class="field-label" for="ssh-key-user">{tr("ssh.account")}</label>
                <select id="ssh-key-user" class="input" on:change=move |ev| who.set(event_target_value(&ev))>
                    {names.into_iter().map(|v| {
                        let (v2, label) = (v.clone(), v.clone());
                        view! { <option value=v selected=move || who.with(|w| *w == v2)>{label}</option> }
                    }).collect_view()}
                </select>
            </div>
            {move || match list.get() {
                None => view! { <Skeleton lines=2 /> }.into_any(),
                Some(Err(e)) => ui::note(Tone::Err, icons::CIRCLE_X, ui::err_view(e)),
                Some(Ok(l)) => {
                    let others = (l.other_lines > 0).then(|| ui::note(Tone::Info, icons::INFO, ui::text(tf("ssh.other_lines", &[("n", &l.other_lines.to_string())]))));
                    let keys = if l.keys.is_empty() {
                        view! { <p class="muted">{tr("ssh.no_keys")}</p> }.into_any()
                    } else {
                        let user = l.user.clone();
                        view! {
                            <ul class="plain-list ssh-keys">
                                {l.keys.into_iter().map(|k| {
                                    let title = if k.comment.is_empty() { k.kind.clone() } else { k.comment.clone() };
                                    let mut sub = format!("{} {} · {}", k.kind, k.bits, k.fingerprint);
                                    if k.options {
                                        sub.push_str(&format!(" · {}", t("ssh.key_options")));
                                    }
                                    let label = format!("{} {}", t("ssh.key_remove"), title);
                                    let r = Removal { user: user.clone(), fingerprint: k.fingerprint.clone(), title: title.clone() };
                                    view! {
                                        <li class="ssh-key">
                                            <div class="ssh-key-text">
                                                <span class="cell-main">{title}{k.weak.then(|| view! { " " <span class="chip warn-chip">{tr("ssh.key_weak")}</span> })}</span>
                                                <span class="cell-sub muted small mono">{sub}</span>
                                            </div>
                                            <button type="button" class="icon-btn sm danger" title=tr("ssh.key_remove") aria-label=label
                                                disabled=move || del.with(Option::is_some) on:click=move |_| del.set(Some(r.clone()))>
                                                <Icon icon=icons::TRASH_2 size=16 />
                                            </button>
                                        </li>
                                    }
                                }).collect_view()}
                            </ul>
                        }
                        .into_any()
                    };
                    view! { {keys}{others} }.into_any()
                }
            }}
            {deleter}
            <form class="form" on:submit=move |ev| { ev.prevent_default(); add(); }>
                <div class="field">
                    <label class="field-label" for="ssh-new-key">{tr("ssh.key_new")}</label>
                    <textarea id="ssh-new-key" class="input mono" rows="3" spellcheck="false" autocomplete="off"
                        placeholder="ssh-ed25519 AAAA… user@laptop"
                        prop:value=move || new_key.get() on:input=move |ev| new_key.set(event_target_value(&ev))></textarea>
                    <span class="field-help">{tr("ssh.key_new_help")}</span>
                </div>
                {crate::form::form_error(error)}
                <div class="sec-actions">
                    <button type="submit" class="btn primary" disabled=move || busy.get() || new_key.with(|k| k.trim().is_empty())>
                        {crate::form::spinner(busy)}<Icon icon=icons::PLUS size=16 />{tr("ssh.key_add")}
                    </button>
                </div>
            </form>
        </div>
    }
    .into_any()
}
