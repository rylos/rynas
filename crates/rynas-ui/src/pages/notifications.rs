//! Notifications (D-NOTIFY): the notification center (`/notifications`: every event of
//! rynasd's engine with severity, acknowledge, acknowledge all, silence) and its settings
//! (`/system/notifications`: targets with a Test button, rules (matchers), silences), on the
//! model of Proxmox targets + matchers and TrueNAS alerts.
//!
//! Targets, rules and silences are rynas' own state: they take effect at once, without the
//! pending/apply cycle of the system configuration.

use std::collections::BTreeMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::notify::{
    BodyFormat, FieldMatch, MatchMode, Notification, NotifyCategory, NotifyLanguage, NotifyRule, NotifySeverity, NotifyTarget, QuietWindow, Silence,
    SilenceRequest, SmtpSecurity, TargetConfig, TargetRequest, TargetTestResult, WebhookHeader, WebhookMethod,
};

use crate::api;
use crate::form::{self, In, lens, lens_bool, save_bar, select_field, switch, tabs, text_field};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::notify::{event_body, event_title, severity_key, severity_tone, use_notes};
use crate::toast::toast;
use crate::ui::{self, Confirm, EmptyState, ErrorBox, PageHeader, Remote, Skeleton, Tone, confirm, drawer};

/// Notifications per page of the center (A17 default).
const PAGE: usize = 50;

/// The name of a target as listed: the built-in web interface target still carrying the name
/// the notification store seeds it with ("Web interface") shows in the current language.
fn target_name(tg: &NotifyTarget) -> String {
    if matches!(tg.config, TargetConfig::Ui {}) && tg.name == "Web interface" {
        t("nt.kind.ui").to_string()
    } else {
        tg.name.clone()
    }
}

/// The source of an event as shown, and whether it is the raw identifier: rynas' own
/// sources (`nt.src.<source>`), a hardware module's senders as the module labels them,
/// anything else (plugins) as sent.
fn source_label(source: &str) -> (String, bool) {
    if let Some(l) = crate::modules::source_label(source) {
        return (l.to_string(), false);
    }
    if source == "scheduler" {
        return (t("sch.title").to_string(), false);
    }
    match i18n::try_t(&format!("nt.src.{source}")) {
        Some(s) => (s.to_string(), false),
        None => (source.to_string(), true),
    }
}

pub fn category_key(c: NotifyCategory) -> &'static str {
    match c {
        NotifyCategory::Storage => "nt.cat.storage",
        NotifyCategory::Hardware => "nt.cat.hardware",
        NotifyCategory::Ups => "nt.cat.ups",
        NotifyCategory::System => "nt.cat.system",
        NotifyCategory::Security => "nt.cat.security",
        NotifyCategory::Update => "nt.cat.update",
        NotifyCategory::Backup => "nt.cat.backup",
        NotifyCategory::App => "nt.cat.app",
        NotifyCategory::Task => "nt.cat.task",
    }
}

const SEV_FILTER: &[(&str, &str)] = &[
    ("", "nt.f.any_sev"),
    ("notice", "nt.f.notice"),
    ("warning", "nt.f.warning"),
    ("error", "nt.f.error"),
    ("critical", "nt.sev.critical"),
];
const CAT_FILTER: &[(&str, &str)] = &[
    ("", "nt.f.any_cat"),
    ("storage", "nt.cat.storage"),
    ("hardware", "nt.cat.hardware"),
    ("ups", "nt.cat.ups"),
    ("system", "nt.cat.system"),
    ("security", "nt.cat.security"),
    ("update", "nt.cat.update"),
    ("backup", "nt.cat.backup"),
    ("app", "nt.cat.app"),
    ("task", "nt.cat.task"),
];
const ACK_FILTER: &[(&str, &str)] = &[("false", "nt.f.unacked"), ("", "nt.f.all"), ("true", "nt.f.acked")];

/// Query string of the center's list.
fn list_url(sev: &str, cat: &str, acked: &str, limit: usize, before: Option<u64>) -> String {
    let mut u = format!("/api/v1/notifications?limit={limit}");
    for (k, v) in [("severity", sev), ("category", cat), ("acked", acked)] {
        if !v.is_empty() {
            u.push_str(&format!("&{k}={v}"));
        }
    }
    if let Some(b) = before {
        u.push_str(&format!("&before={b}"));
    }
    u
}

/// State pill of a notification: acknowledged, resolved, still active.
fn state_pill(n: &Notification) -> Option<AnyView> {
    use rynas_api_types::notify::EventState;
    if n.acked_ms.is_some() {
        Some(ui::status(Tone::Ok, t("nt.acked").to_string()))
    } else if n.resolved_ms.is_some() || n.state == EventState::Resolved {
        Some(ui::status(Tone::Ok, t("nt.resolved").to_string()))
    } else if n.state == EventState::Firing {
        Some(ui::status(Tone::Warn, t("nt.active").to_string()))
    } else {
        None
    }
}

fn severity_pill(s: NotifySeverity) -> AnyView {
    ui::status(severity_tone(s), t(severity_key(s)).to_string())
}

/// A select bound to a string signal, for filters (no label row: `aria-label`).
fn filter_select(label: &'static str, opts: &'static [(&'static str, &'static str)], value: RwSignal<String>) -> AnyView {
    view! {
        <select class="input filter" aria-label=tr(label) on:change=move |ev| value.set(event_target_value(&ev))>
            {opts.iter().map(|(v, k)| view! { <option value=*v selected=move || value.with(|x| x == v)>{tr(k)}</option> }).collect_view()}
        </select>
    }
    .into_any()
}

// ------------------------------------------------------------------ center

#[component]
pub fn NotificationsPage() -> impl IntoView {
    let server = use_notes().server;
    let sev = RwSignal::new(String::new());
    let cat = RwSignal::new(String::new());
    let acked = RwSignal::new("false".to_string());
    let items = RwSignal::new(None::<Vec<Notification>>);
    let err = RwSignal::new(None::<String>);
    let more = RwSignal::new(false);
    let busy_more = RwSignal::new(false);
    let detail = RwSignal::new(None::<Notification>);
    let silence = RwSignal::new(None::<String>);

    // first page (or as many as already shown) again on filter changes and on every event
    let load = move || {
        let n = items.with_untracked(|i| i.as_ref().map(Vec::len).unwrap_or(0)).clamp(PAGE, 500);
        let url = list_url(&sev.get_untracked(), &cat.get_untracked(), &acked.get_untracked(), n, None);
        spawn_local(async move {
            match api::get::<Vec<Notification>>(&url).await {
                Ok(l) => {
                    more.set(l.len() >= n);
                    items.set(Some(l));
                    err.set(None);
                }
                Err(e) if e.status() == 401 => {}
                Err(e) => err.set(Some(e.code().to_string())),
            }
        });
    };
    Effect::new(move |_| {
        sev.track();
        cat.track();
        acked.track();
        items.set(None);
        load();
    });
    Effect::new(move |prev: Option<u64>| {
        let g = server.generation.get();
        if prev.is_some() {
            load();
        }
        g
    });
    let load_more = move |_| {
        let Some(last) = items.with_untracked(|i| i.as_ref().and_then(|l| l.last().map(|n| n.id))) else { return };
        busy_more.set(true);
        let url = list_url(&sev.get_untracked(), &cat.get_untracked(), &acked.get_untracked(), PAGE, Some(last));
        spawn_local(async move {
            let r = api::get::<Vec<Notification>>(&url).await;
            busy_more.set(false);
            match r {
                Ok(l) => {
                    more.set(l.len() >= PAGE);
                    items.update(|i| i.get_or_insert_with(Vec::new).extend(l));
                }
                Err(e) => toast(Tone::Err, e.text()),
            }
        });
    };

    let table = move || {
        if items.with(Option::is_none) {
            return match err.get() {
                Some(code) => view! { <ErrorBox code=code on_retry=Callback::new(move |_| load()) /> }.into_any(),
                None => view! { <Skeleton lines=6 /> }.into_any(),
            };
        }
        let rows = items.get().unwrap_or_default();
        if rows.is_empty() {
            return view! {
                <EmptyState icon=icons::BELL_OFF title=Signal::derive(|| t("nt.none").to_string()) text=Signal::derive(|| t("nt.none_text").to_string()) />
            }
            .into_any();
        }
        view! {
            {ui::table("", &["nt.col.severity", "nt.col.event", "nt.col.category", "nt.col.when", "col.status", ""], rows.into_iter().map(|n| center_row(n, detail, silence)).collect::<Vec<_>>())}
            <Show when=move || more.get()>
                <div class="load-more">
                    <button class="btn ghost" disabled=move || busy_more.get() on:click=load_more>
                        {crate::form::spinner(busy_more)}{tr("nt.load_more")}
                    </button>
                </div>
            </Show>
        }
        .into_any()
    };

    view! {
        <div class="page">
            <PageHeader icon=icons::BELL title="nav.notifications" subtitle="nt.subtitle">
                <a class="btn ghost" href="/system/notifications"><Icon icon=icons::SETTINGS size=16 /><span class="btn-label">{tr("nt.settings")}</span></a>
                <button class="btn primary" disabled=move || server.unacked.get() == 0 on:click=move |_| server.ack_all()>
                    <Icon icon=icons::CHECK_CHECK size=16 /><span class="btn-label">{tr("nt.ack_all")}</span>
                </button>
            </PageHeader>
            <section class="card widget w-12">
                <div class="toolbar">
                    {filter_select("nt.col.severity", SEV_FILTER, sev)}
                    {filter_select("nt.col.category", CAT_FILTER, cat)}
                    {filter_select("col.status", ACK_FILTER, acked)}
                </div>
                {table}
            </section>
            {move || detail.get().map(|n| detail_drawer(n, Callback::new(move |_| detail.set(None)), silence))}
            {move || silence.get().map(|k| silence_dialog(k, Callback::new(move |_| silence.set(None)), Callback::new(|_| {})))}
        </div>
    }
}

fn center_row(n: Notification, detail: RwSignal<Option<Notification>>, silence: RwSignal<Option<String>>) -> AnyView {
    let server = use_notes().server;
    let id = n.id;
    let acked = n.acked_ms.is_some();
    let key = n.key.clone();
    let n2 = n.clone();
    let title = event_title(&n);
    let body = event_body(&n);
    let log = n.body_format == BodyFormat::Log;
    let open_label = ui::verb_name("nt.details", &title);
    let ack_label = ui::verb_name("nt.ack", &title);
    let sil_label = ui::verb_name("nt.silence", &title);
    let (source, raw_source) = source_label(&n.source);
    ui::row_class(
        vec![
            ui::cell("nt.col.severity", "", view! { {severity_pill(n.severity)} }.into_any()),
            ui::cell("nt.col.event", "wrap-cell", view! {
                <div class="cell-main">
                    <button class="link-strong" aria-label=open_label on:click=move |_| detail.set(Some(n2.clone()))>{title}</button>
                    <span class=if log { "cell-sub clamp-2 pre-line mono" } else { "cell-sub clamp-2" }>{body}</span>
                </div>
            }
            .into_any()),
            ui::cell("nt.col.category", "", view! {
                <div class="cell-main">
                    <span>{tr(category_key(n.category))}</span>
                    <span class=if raw_source { "cell-sub mono" } else { "cell-sub" }>{source}</span>
                </div>
            }
            .into_any()),
            ui::cell("nt.col.when", "tnum", view! {
                <div class="cell-main">
                    <span>{i18n::short_when(n.last_ms)}</span>
                    {(n.count > 1).then(|| view! { <span class="cell-sub">{tf("nt.times", &[("n", &n.count.to_string())])}</span> })}
                </div>
            }
            .into_any()),
            ui::cell("col.status", "", view! {
                <div class="chips">
                    {state_pill(&n)}
                    {n.silenced.then(|| view! { <span class="chip"><Icon icon=icons::BELL_OFF size=12 />{tr("nt.silenced")}</span> })}
                </div>
            }
            .into_any()),
            ui::actions(view! {
                <button class="icon-btn sm" title=tr("nt.ack") aria-label=ack_label disabled=acked on:click=move |_| server.ack(id)>
                    <Icon icon=icons::CHECK size=16 />
                </button>
                <button class="icon-btn sm" title=tr("nt.silence") aria-label=sil_label on:click=move |_| silence.set(Some(key.clone()))>
                    <Icon icon=icons::BELL_OFF size=16 />
                </button>
            }
            .into_any()),
        ],
        if acked { "muted-row" } else { "" },
    )
}


fn detail_drawer(n: Notification, close: Callback<()>, silence: RwSignal<Option<String>>) -> AnyView {
    let server = use_notes().server;
    let id = n.id;
    let acked = n.acked_ms.is_some();
    let key = n.key.clone();
    let body_text = event_body(&n);
    let log = n.body_format == BodyFormat::Log;
    let mut kvs = vec![
        ui::kv_text("nt.col.severity", t(severity_key(n.severity)).to_string(), ""),
        ui::kv_text("nt.col.category", t(category_key(n.category)).to_string(), ""),
        match source_label(&n.source) {
            (_, true) => ui::kv_text("nt.source", n.source.clone(), "mono"),
            (label, false) => ui::kv_text("nt.source", format!("{label} · {}", n.source), ""),
        },
        ui::kv_text("nt.key", n.key.clone(), "mono"),
        ui::kv_text("nt.first", i18n::datetime(n.ts_ms), "tnum"),
    ];
    if n.count > 1 {
        kvs.push(ui::kv_text("nt.last", format!("{} · {}", i18n::datetime(n.last_ms), tf("nt.times", &[("n", &n.count.to_string())])), "tnum"));
    }
    if let Some(r) = n.resolved_ms {
        kvs.push(ui::kv_text("nt.resolved", i18n::datetime(r), "tnum"));
    }
    if let Some(a) = n.acked_ms {
        kvs.push(ui::kv_text("nt.acked", format!("{} · {}", i18n::datetime(a), n.acked_by.clone().unwrap_or_default()), "tnum"));
    }
    let fields: Vec<AnyView> = n
        .fields
        .iter()
        .map(|(k, v)| view! { <div class="kv"><dt class="mono">{k.clone()}</dt><dd class="mono">{v.clone()}</dd></div> }.into_any())
        .collect();
    let has_fields = !fields.is_empty();
    let actions: Vec<AnyView> = n
        .actions
        .iter()
        .filter(|a| a.url.starts_with('/') || a.url.starts_with("https://") || a.url.starts_with("http://"))
        .map(|a| {
            let external = !a.url.starts_with('/');
            view! {
                <a class="btn ghost sm" href=a.url.clone() target=external.then_some("_blank") rel=external.then_some("noopener noreferrer")>
                    <Icon icon=icons::LINK size=14 />{a.label.clone()}
                </a>
            }
            .into_any()
        })
        .collect();
    let body = view! {
        <div class="chips">{severity_pill(n.severity)}{state_pill(&n)}</div>
        {(!body_text.is_empty()).then(|| view! { <p class=if log { "pre-text log-text" } else { "pre-text" }>{body_text}</p> })}
        {(!actions.is_empty()).then(|| view! { <div class="card-actions">{actions}</div> })}
        <dl class="kvs">{kvs}</dl>
        {has_fields.then(|| view! { <h3 class="mini-title">{tr("nt.fields")}</h3><dl class="kvs">{fields}</dl> })}
    }
    .into_any();
    let foot = view! {
        <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("close")}</button>
        <button type="button" class="btn ghost" on:click=move |_| { close.run(()); silence.set(Some(key.clone())); }>
            <Icon icon=icons::BELL_OFF size=16 />{tr("nt.silence")}
        </button>
        <button type="button" class="btn primary" disabled=acked on:click=move |_| { server.ack(id); close.run(()); }>
            <Icon icon=icons::CHECK size=16 />{tr("nt.ack")}
        </button>
    }
    .into_any();
    drawer(Signal::stored(event_title(&n)), close, body, foot)
}

const SILENCE_FOR: &[(&str, &str)] = &[
    ("3600", "nt.for_1h"),
    ("28800", "nt.for_8h"),
    ("86400", "nt.for_1d"),
    ("604800", "nt.for_7d"),
    ("", "nt.for_ever"),
];

/// Silence a key (or a prefix ending with `*`) for a while.
pub fn silence_dialog(key: String, close: Callback<()>, done: Callback<()>) -> AnyView {
    let k = RwSignal::new(key);
    let dur = RwSignal::new("86400".to_string());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let key_err = Signal::derive(move || k.with(|k| k.trim().is_empty()).then(|| t("nt.key_required").to_string()));
    let extra = view! {
        {text_field("nt.key", Signal::derive(move || k.get()), Callback::new(move |v| k.set(v)), key_err, In { mono: true, help: Some("nt.key_help"), ..In::default() })}
        {select_field("nt.silence_for", None, SILENCE_FOR, Signal::derive(move || dur.get()), Callback::new(move |v| dur.set(v)))}
    }
    .into_any();
    confirm(Confirm {
        title: t("nt.silence_title").to_string(),
        text: t("nt.silence_text").to_string(),
        action: "nt.silence",
        danger: false,
        extra: Some(extra),
        typed: Signal::stored(None),
        busy,
        error,
        on_confirm: Callback::new(move |_| {
            let key = k.get_untracked().trim().to_string();
            if key.is_empty() {
                return;
            }
            busy.set(true);
            error.set(None);
            let body = SilenceRequest { key, duration_s: dur.get_untracked().parse().ok() };
            spawn_local(async move {
                let r = api::post::<_, Silence>("/api/v1/notifications/silence", Some(&body)).await;
                busy.set(false);
                match r {
                    Ok(_) => {
                        toast(Tone::Ok, t("nt.silenced_ok").to_string());
                        done.run(());
                        close.run(());
                    }
                    Err(e) => error.set(Some(e.text())),
                }
            });
        }),
        on_cancel: close,
    })
}

// ------------------------------------------------------------------ settings

#[component]
pub fn NotificationSettings() -> impl IntoView {
    let tab = RwSignal::new(0usize);
    let targets = Remote::<Vec<NotifyTarget>>::with_config("/api/v1/notifications/targets");
    let rules = Remote::<Vec<NotifyRule>>::with_config("/api/v1/notifications/rules");
    let silences = Remote::<Vec<Silence>>::with_config("/api/v1/notifications/silences");
    view! {
        <div class="page">
            <PageHeader icon=icons::BELL_RING title="nt.settings_title" subtitle="nt.settings_subtitle">
                <a class="btn ghost" href="/notifications"><Icon icon=icons::BELL size=16 /><span class="btn-label">{tr("nt.view_all")}</span></a>
            </PageHeader>
            <section class="card widget w-12">
                {tabs("nts", &["nt.tab.targets", "nt.tab.rules", "nt.tab.silences"], tab, "nt.settings_title")}
                <div role="tabpanel" id="nts-p0" aria-labelledby="nts-t0" hidden=move || tab.get() != 0>{targets_panel(targets)}</div>
                <div role="tabpanel" id="nts-p1" aria-labelledby="nts-t1" hidden=move || tab.get() != 1>{rules_panel(rules, targets)}</div>
                <div role="tabpanel" id="nts-p2" aria-labelledby="nts-t2" hidden=move || tab.get() != 2>{silences_panel(silences)}</div>
            </section>
        </div>
    }
}

// ------------------------------------------------------------------ targets

/// Kinds a target can have: (value, i18n key).
const KINDS: &[(&str, &str)] = &[
    ("smtp", "nt.kind.smtp"),
    ("ntfy", "nt.kind.ntfy"),
    ("telegram", "nt.kind.telegram"),
    ("gotify", "nt.kind.gotify"),
    ("webhook", "nt.kind.webhook"),
    ("ui", "nt.kind.ui"),
];
const SECURITY: &[(&str, &str)] = &[("starttls", "nt.sec.starttls"), ("tls", "nt.sec.tls"), ("none", "nt.sec.none")];
const METHODS: &[(&str, &str)] = &[("post", "nt.m.post"), ("put", "nt.m.put"), ("get", "nt.m.get")];

fn kind_key(kind: &str) -> &'static str {
    KINDS.iter().find(|(k, _)| *k == kind).map(|(_, l)| *l).unwrap_or("nt.kind.ui")
}

/// Secrets a kind uses (webhooks name their own).
fn kind_secrets(kind: &str) -> &'static [&'static str] {
    match kind {
        "smtp" => &["password"],
        "ntfy" => &["token", "password"],
        "telegram" => &["bot_token"],
        "gotify" => &["token"],
        _ => &[],
    }
}

/// One secret in the editor: write-only, `set` when the server has a value.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SecretEdit {
    pub name: String,
    pub value: String,
    pub set: bool,
    pub remove: bool,
}

/// A target as the editor holds it (flat text fields for every kind).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TargetDraft {
    pub id: Option<u64>,
    pub name: String,
    pub enabled: bool,
    pub language: String,
    pub kind: String,
    pub host: String,
    pub port: String,
    pub security: String,
    pub username: String,
    pub from: String,
    /// Recipients, one per line or comma separated.
    pub to: String,
    pub server: String,
    pub topic: String,
    pub chat_id: String,
    pub api_url: String,
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub secrets: Vec<SecretEdit>,
}

fn opt(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

impl TargetDraft {
    pub fn new_target() -> Self {
        let mut d = TargetDraft { enabled: true, language: "en".into(), port: "587".into(), security: "starttls".into(), method: "post".into(), ..Default::default() };
        d.set_kind("ntfy");
        d.server = "https://ntfy.sh".into();
        d
    }

    pub fn from_target(t: &NotifyTarget) -> Self {
        let mut d = TargetDraft {
            id: Some(t.id),
            name: t.name.clone(),
            enabled: t.enabled,
            // a language the table no longer has: English (the server refuses to save it, and
            // renders it in English anyway)
            language: crate::i18n::Lang::from_code(t.language.as_str()).map_or("en", crate::i18n::Lang::code).into(),
            port: "587".into(),
            security: "starttls".into(),
            method: "post".into(),
            ..Default::default()
        };
        match &t.config {
            TargetConfig::Smtp { host, port, security, username, from, to } => {
                d.host = host.clone();
                d.port = port.to_string();
                d.security = match security {
                    SmtpSecurity::Starttls => "starttls",
                    SmtpSecurity::Tls => "tls",
                    SmtpSecurity::None => "none",
                }
                .into();
                d.username = username.clone().unwrap_or_default();
                d.from = from.clone();
                d.to = to.join("\n");
            }
            TargetConfig::Ntfy { server, topic, username } => {
                d.server = server.clone();
                d.topic = topic.clone();
                d.username = username.clone().unwrap_or_default();
            }
            TargetConfig::Telegram { chat_id, api_url } => {
                d.chat_id = chat_id.clone();
                d.api_url = api_url.clone().unwrap_or_default();
            }
            TargetConfig::Gotify { server } => d.server = server.clone(),
            TargetConfig::Webhook { url, method, headers, body } => {
                d.url = url.clone();
                d.method = match method {
                    WebhookMethod::Get => "get",
                    WebhookMethod::Post => "post",
                    WebhookMethod::Put => "put",
                }
                .into();
                d.headers = headers.iter().map(|h| (h.name.clone(), h.value.clone())).collect();
                d.body = body.clone().unwrap_or_default();
            }
            TargetConfig::Ui {} => {}
        }
        d.kind = t.config.kind().into();
        d.secrets = t.secrets.iter().map(|n| SecretEdit { name: n.clone(), set: true, ..Default::default() }).collect();
        d.set_kind(t.config.kind());
        d
    }

    /// Change the kind: the secret list becomes the kind's (values already set are kept).
    pub fn set_kind(&mut self, kind: &str) {
        self.kind = kind.into();
        if kind == "webhook" {
            return;
        }
        let old = std::mem::take(&mut self.secrets);
        let mut list: Vec<SecretEdit> = kind_secrets(kind)
            .iter()
            .map(|n| old.iter().find(|s| s.name == *n).cloned().unwrap_or(SecretEdit { name: (*n).into(), ..Default::default() }))
            .collect();
        // secrets set on the server that the new kind does not use are removed on save
        list.extend(old.into_iter().filter(|s| s.set && !kind_secrets(kind).contains(&s.name.as_str())).map(|s| SecretEdit { remove: true, ..s }));
        self.secrets = list;
    }

    pub fn config(&self) -> Result<TargetConfig, &'static str> {
        Ok(match self.kind.as_str() {
            "smtp" => {
                let to: Vec<String> = self.to.split([',', '\n', ';']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect();
                if self.host.trim().is_empty() {
                    return Err("nt.v.host");
                }
                if self.from.trim().is_empty() || to.is_empty() {
                    return Err("nt.v.addresses");
                }
                TargetConfig::Smtp {
                    host: self.host.trim().into(),
                    port: self.port.trim().parse().map_err(|_| "nt.v.port")?,
                    security: match self.security.as_str() {
                        "tls" => SmtpSecurity::Tls,
                        "none" => SmtpSecurity::None,
                        _ => SmtpSecurity::Starttls,
                    },
                    username: opt(&self.username),
                    from: self.from.trim().into(),
                    to,
                }
            }
            "ntfy" => {
                if self.server.trim().is_empty() || self.topic.trim().is_empty() {
                    return Err("nt.v.ntfy");
                }
                TargetConfig::Ntfy { server: self.server.trim().into(), topic: self.topic.trim().into(), username: opt(&self.username) }
            }
            "telegram" => {
                if self.chat_id.trim().is_empty() {
                    return Err("nt.v.chat_id");
                }
                TargetConfig::Telegram { chat_id: self.chat_id.trim().into(), api_url: opt(&self.api_url) }
            }
            "gotify" => {
                if self.server.trim().is_empty() {
                    return Err("nt.v.server");
                }
                TargetConfig::Gotify { server: self.server.trim().into() }
            }
            "webhook" => {
                if self.url.trim().is_empty() {
                    return Err("nt.v.url");
                }
                TargetConfig::Webhook {
                    url: self.url.trim().into(),
                    method: match self.method.as_str() {
                        "get" => WebhookMethod::Get,
                        "put" => WebhookMethod::Put,
                        _ => WebhookMethod::Post,
                    },
                    headers: self
                        .headers
                        .iter()
                        .filter(|(n, _)| !n.trim().is_empty())
                        .map(|(n, v)| WebhookHeader { name: n.trim().into(), value: v.clone() })
                        .collect(),
                    body: (!self.body.trim().is_empty()).then(|| self.body.clone()),
                }
            }
            _ => TargetConfig::Ui {},
        })
    }

    /// The request body: a typed value sets a secret, `remove` deletes it, empty keeps it.
    pub fn request(&self) -> Result<TargetRequest, &'static str> {
        if self.name.trim().is_empty() {
            return Err("nt.v.name");
        }
        let mut secrets = BTreeMap::new();
        for s in &self.secrets {
            let name = s.name.trim();
            if name.is_empty() {
                continue;
            }
            if s.remove {
                if s.set {
                    secrets.insert(name.to_string(), None);
                }
            } else if !s.value.is_empty() {
                secrets.insert(name.to_string(), Some(s.value.clone()));
            }
        }
        Ok(TargetRequest {
            name: self.name.trim().into(),
            enabled: self.enabled,
            language: NotifyLanguage::new(&self.language).unwrap_or_default(),
            config: self.config()?,
            secrets,
        })
    }
}

/// Where a target sends, in one line.
fn target_summary(c: &TargetConfig) -> String {
    match c {
        TargetConfig::Smtp { to, .. } => to.join(", "),
        TargetConfig::Ntfy { server, topic, .. } => format!("{}/{topic}", server.trim_end_matches('/')),
        TargetConfig::Telegram { chat_id, .. } => chat_id.clone(),
        TargetConfig::Gotify { server } => server.clone(),
        TargetConfig::Webhook { url, .. } => url.clone(),
        TargetConfig::Ui {} => String::new(),
    }
}

fn targets_panel(targets: Remote<Vec<NotifyTarget>>) -> AnyView {
    let edit = RwSignal::new(None::<TargetDraft>);
    let del = RwSignal::new(None::<(u64, String)>);
    let testing = RwSignal::new(None::<u64>);
    let test = move |id: u64, name: String| {
        testing.set(Some(id));
        spawn_local(async move {
            let r = api::post::<(), TargetTestResult>(&format!("/api/v1/notifications/targets/{id}/test"), None).await;
            testing.set(None);
            match r {
                Ok(res) if res.ok => toast(Tone::Ok, tf("nt.test_ok", &[("name", &name), ("ms", &res.duration_ms.to_string())])),
                Ok(res) => toast(Tone::Err, format!("{} {}", tf("nt.test_failed", &[("name", &name)]), res.error.unwrap_or_default())),
                Err(e) => toast(Tone::Err, e.text()),
            }
            targets.reload();
        });
    };
    let table = move || {
        if let Some(v) = targets.gate(4) {
            return v;
        }
        let rows = targets.data.get().unwrap_or_default();
        if rows.is_empty() {
            return view! {
                <EmptyState icon=icons::SEND title=Signal::derive(|| t("nt.no_targets").to_string()) text=Signal::derive(|| t("nt.no_targets_text").to_string()) />
            }
            .into_any();
        }
        view! {
            {ui::table("", &["nt.col.name", "nt.col.kind", "nt.col.delivery", "col.status", ""], rows.into_iter().map(|tg| {
                            let id = tg.id;
                            let name = target_name(&tg);
                            let d = TargetDraft::from_target(&tg);
                            let st = &tg.status;
                            let (tone, key) = if !tg.enabled {
                                (Tone::Unknown, "nt.disabled")
                            } else if st.last_error_ms.is_some() && st.last_error_ms > st.last_ok_ms {
                                (Tone::Err, "nt.failing")
                            } else if st.last_ok_ms.is_some() {
                                (Tone::Ok, "nt.delivering")
                            } else {
                                (Tone::Info, "nt.never_used")
                            };
                            let detail = if tone == Tone::Err {
                                st.last_error.clone().unwrap_or_default()
                            } else {
                                st.last_ok_ms.map(|m| tf("nt.last_ok", &[("when", &i18n::short_when(m))])).unwrap_or_default()
                            };
                            let queued = st.queued;
                            let is_ui = matches!(tg.config, TargetConfig::Ui {});
                            let (n2, n3, n4) = (name.clone(), name.clone(), name.clone());
                            ui::row(vec![
                                ui::cell("nt.col.name", "", view! { <button class="link-strong" on:click={ let d = d.clone(); move |_| edit.set(Some(d.clone())) }>{name.clone()}</button> }.into_any()),
                                ui::cell("nt.col.kind", "", view! { <span class="chip">{tr(kind_key(tg.config.kind()))}</span> }.into_any()),
                                ui::cell("nt.col.delivery", "mono muted ellipsis-cell", view! { {target_summary(&tg.config)} }.into_any()),
                                ui::cell("col.status", "", view! {
                                    <div class="cell-main">
                                        <div class="chips">
                                            {ui::status(tone, t(key).to_string())}
                                            {(queued > 0).then(|| view! { <span class="chip">{tf("nt.queued", &[("n", &queued.to_string())])}</span> })}
                                        </div>
                                        <span class="cell-sub">{detail}</span>
                                    </div>
                                }
                                .into_any()),
                                ui::actions(view! {
                                    <button class="icon-btn sm" title=tr("nt.test") aria-label=ui::verb_name("nt.test", &name) disabled=move || is_ui || testing.get().is_some()
                                        on:click=move |_| test(id, n2.clone())>
                                        {crate::form::spinner_icon(Signal::derive(move || testing.get() == Some(id)), icons::SEND)}
                                    </button>
                                    <button class="icon-btn sm" title=tr("edit") aria-label=format!("{} {}", t("edit"), n3) on:click=move |_| edit.set(Some(d.clone()))>
                                        <Icon icon=icons::PENCIL size=16 />
                                    </button>
                                    <button class="icon-btn sm danger" title=tr("delete") aria-label=format!("{} {}", t("delete"), n4) on:click=move |_| del.set(Some((id, n4.clone())))>
                                        <Icon icon=icons::TRASH_2 size=16 />
                                    </button>
                                }
                                .into_any()),
                            ])
                        }).collect::<Vec<_>>())}
        }
        .into_any()
    };
    let editor = move || edit.get().map(|d| target_editor(d, targets, Callback::new(move |_| edit.set(None))));
    let deleter = move || {
        del.get().map(|(id, name)| {
            let busy = RwSignal::new(false);
            let error = RwSignal::new(None::<String>);
            let close = Callback::new(move |_| del.set(None));
            confirm(Confirm {
                title: tf("nt.delete_target", &[("name", &name)]),
                text: t("nt.delete_target_text").to_string(),
                action: "delete",
                danger: true,
                extra: None,
                typed: Signal::stored(None),
                busy,
                error,
                on_confirm: Callback::new(move |_| {
                    busy.set(true);
                    spawn_local(async move {
                        let r = api::delete_empty(&format!("/api/v1/notifications/targets/{id}")).await;
                        busy.set(false);
                        match r {
                            Ok(()) => {
                                targets.reload();
                                close.run(());
                            }
                            Err(e) if e.code() == rynas_api_types::error_codes::CONFLICT => error.set(Some(t("nt.target_in_use").to_string())),
                            Err(e) => error.set(Some(e.text())),
                        }
                    });
                }),
                on_cancel: close,
            })
        })
    };
    view! {
        <div class="toolbar">
            <p class="muted small grow">{tr("nt.targets_help")}</p>
            <button class="btn primary" on:click=move |_| edit.set(Some(TargetDraft::new_target()))>
                <Icon icon=icons::PLUS size=16 /><span class="btn-label">{tr("nt.add_target")}</span>
            </button>
        </div>
        {table}
        {editor}
        {deleter}
    }
    .into_any()
}

/// Write-only secret field: placeholder "set" when the server has a value, a Remove switch.
fn secret_row(d: RwSignal<TargetDraft>, i: usize, custom: bool) -> AnyView {
    let get = move |f: fn(&SecretEdit) -> String| Signal::derive(move || d.with(|d| d.secrets.get(i).map(f).unwrap_or_default()));
    let name = get(|s| s.name.clone());
    let value = get(|s| s.value.clone());
    let is_set = Signal::derive(move || d.with(|d| d.secrets.get(i).is_some_and(|s| s.set)));
    let removed = Signal::derive(move || d.with(|d| d.secrets.get(i).is_some_and(|s| s.remove)));
    let label = Signal::derive(move || {
        let n = name.get();
        match i18n::try_t(&format!("nt.secret.{n}")) {
            Some(l) if !custom => l.to_string(),
            _ => n,
        }
    });
    let id = form::fid();
    let id2 = id.clone();
    view! {
        <div class="secret-row">
            {custom.then(|| view! {
                <input class="input mono" aria-label=tr("nt.secret_name") placeholder=tr("nt.secret_name") disabled=move || is_set.get()
                    prop:value=move || name.get()
                    on:input=move |ev| { let v = event_target_value(&ev); d.update(|d| if let Some(s) = d.secrets.get_mut(i) { s.name = v; }); } />
            })}
            <div class="field grow">
                {(!custom).then(|| view! { <label class="field-label" for=id.clone()>{move || label.get()}</label> })}
                <input id=id2 class="input" type="password" autocomplete="new-password"
                    aria-label=move || label.get()
                    placeholder=move || if is_set.get() { t("nt.secret_set") } else { t("not_set") }
                    disabled=move || removed.get()
                    prop:value=move || value.get()
                    on:input=move |ev| { let v = event_target_value(&ev); d.update(|d| if let Some(s) = d.secrets.get_mut(i) { s.value = v; }); } />
            </div>
            {move || if is_set.get() {
                view! {
                    <label class="check">
                        <input type="checkbox" prop:checked=move || removed.get()
                            on:change=move |ev| { let on = event_target_checked(&ev); d.update(|d| if let Some(s) = d.secrets.get_mut(i) { s.remove = on; }); } />
                        {tr("nt.secret_remove")}
                    </label>
                }.into_any()
            } else if custom {
                view! {
                    <button type="button" class="icon-btn sm" aria-label=tr("delete") on:click=move |_| d.update(|d| { if i < d.secrets.len() { d.secrets.remove(i); } })>
                        <Icon icon=icons::X size=14 />
                    </button>
                }.into_any()
            } else {
                ().into_any()
            }}
        </div>
    }
    .into_any()
}

fn target_editor(init: TargetDraft, targets: Remote<Vec<NotifyTarget>>, close: Callback<()>) -> AnyView {
    let id = init.id;
    let d = RwSignal::new(init);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let none = Signal::derive(|| None::<String>);
    let kind = Memo::new(move |_| d.with(|d| d.kind.clone()));

    let (name, set_name) = lens(d, |d| d.name.clone(), |d, v| d.name = v);
    let (enabled, set_enabled) = lens_bool(d, |d| d.enabled, |d, v| d.enabled = v);
    let (lang, set_lang) = lens(d, |d| d.language.clone(), |d, v| d.language = v);
    let (kind_s, set_kind) = lens(d, |d| d.kind.clone(), |d, v| d.set_kind(&v));

    let fields = move || {
        let f = |label: &'static str, get: fn(&TargetDraft) -> String, set: fn(&mut TargetDraft, String), o: In| {
            let (v, s) = lens(d, get, set);
            text_field(label, v, s, none, o)
        };
        let secrets_count = Memo::new(move |_| d.with(|d| d.secrets.len()));
        let secrets = move |custom: bool| {
            move || (0..secrets_count.get()).map(|i| secret_row(d, i, custom)).collect_view()
        };
        match kind.get().as_str() {
            "smtp" => view! {
                <div class="form-row">
                    {f("nt.f.host", |d| d.host.clone(), |d, v| d.host = v, In { mono: true, placeholder: "smtp.example.com", ..In::default() })}
                    {f("nt.f.port", |d| d.port.clone(), |d, v| d.port = v, In { kind: "number", ..In::default() })}
                </div>
                {select_field("nt.f.security", None, SECURITY, Signal::derive(move || d.with(|d| d.security.clone())), Callback::new(move |v| d.update(|d| d.security = v)))}
                {f("username", |d| d.username.clone(), |d, v| d.username = v, In { autocomplete: "off", ..In::default() })}
                {secrets(false)}
                {f("nt.f.from", |d| d.from.clone(), |d, v| d.from = v, In { kind: "email", placeholder: "nas@example.com", ..In::default() })}
                {form::area_field("nt.f.to", Signal::derive(move || d.with(|d| d.to.clone())), Callback::new(move |v| d.update(|d| d.to = v)), Some("nt.f.to_help"), "alice@example.com")}
            }
            .into_any(),
            "ntfy" => view! {
                {f("nt.f.server", |d| d.server.clone(), |d, v| d.server = v, In { mono: true, placeholder: "https://ntfy.sh", ..In::default() })}
                {f("nt.f.topic", |d| d.topic.clone(), |d, v| d.topic = v, In { mono: true, help: Some("nt.f.topic_help"), ..In::default() })}
                {f("username", |d| d.username.clone(), |d, v| d.username = v, In { help: Some("nt.f.ntfy_user_help"), ..In::default() })}
                {secrets(false)}
            }
            .into_any(),
            "telegram" => view! {
                {secrets(false)}
                {f("nt.f.chat_id", |d| d.chat_id.clone(), |d, v| d.chat_id = v, In { mono: true, help: Some("nt.f.chat_id_help"), ..In::default() })}
                {f("nt.f.api_url", |d| d.api_url.clone(), |d, v| d.api_url = v, In { mono: true, placeholder: "https://api.telegram.org", ..In::default() })}
            }
            .into_any(),
            "gotify" => view! {
                {f("nt.f.server", |d| d.server.clone(), |d, v| d.server = v, In { mono: true, placeholder: "https://gotify.example.org", ..In::default() })}
                {secrets(false)}
            }
            .into_any(),
            "webhook" => {
                let headers_count = Memo::new(move |_| d.with(|d| d.headers.len()));
                view! {
                    <div class="form-row">
                        {f("nt.f.url", |d| d.url.clone(), |d, v| d.url = v, In { mono: true, help: Some("nt.f.url_help"), placeholder: "https://hooks.example.org/{{ secrets.path }}", ..In::default() })}
                        {select_field("nt.f.method", None, METHODS, Signal::derive(move || d.with(|d| d.method.clone())), Callback::new(move |v| d.update(|d| d.method = v)))}
                    </div>
                    <fieldset class="fieldset">
                        <legend class="field-label">{tr("nt.f.headers")}</legend>
                        {move || (0..headers_count.get()).map(|i| view! {
                            <div class="secret-row">
                                <input class="input mono" aria-label=tr("nt.f.header_name") placeholder=tr("nt.f.header_name")
                                    prop:value=move || d.with(|d| d.headers.get(i).map(|h| h.0.clone()).unwrap_or_default())
                                    on:input=move |ev| { let v = event_target_value(&ev); d.update(|d| if let Some(h) = d.headers.get_mut(i) { h.0 = v; }); } />
                                <input class="input mono grow" aria-label=tr("nt.f.header_value") placeholder=tr("nt.f.header_value")
                                    prop:value=move || d.with(|d| d.headers.get(i).map(|h| h.1.clone()).unwrap_or_default())
                                    on:input=move |ev| { let v = event_target_value(&ev); d.update(|d| if let Some(h) = d.headers.get_mut(i) { h.1 = v; }); } />
                                <button type="button" class="icon-btn sm" aria-label=tr("delete") on:click=move |_| d.update(|d| { if i < d.headers.len() { d.headers.remove(i); } })>
                                    <Icon icon=icons::X size=14 />
                                </button>
                            </div>
                        }).collect_view()}
                        <button type="button" class="btn ghost sm self-start" on:click=move |_| d.update(|d| d.headers.push(Default::default()))>
                            <Icon icon=icons::PLUS size=14 />{tr("nt.f.add_header")}
                        </button>
                    </fieldset>
                    {form::area_field("nt.f.body", Signal::derive(move || d.with(|d| d.body.clone())), Callback::new(move |v| d.update(|d| d.body = v)), Some("nt.f.body_help"), "{\"text\": {{ message|tojson }}}")}
                    <fieldset class="fieldset">
                        <legend class="field-label">{tr("nt.f.secrets")}</legend>
                        <p class="field-help">{tr("nt.f.secrets_help")}</p>
                        {secrets(true)}
                        <button type="button" class="btn ghost sm self-start" on:click=move |_| d.update(|d| d.secrets.push(SecretEdit::default()))>
                            <Icon icon=icons::PLUS size=14 />{tr("nt.f.add_secret")}
                        </button>
                    </fieldset>
                }
                .into_any()
            }
            _ => view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("nt.kind.ui_help")}</p> }.into_any(),
        }
    };

    let save = Callback::new(move |_| {
        let req = match d.with_untracked(TargetDraft::request) {
            Ok(r) => r,
            Err(k) => {
                error.set(Some(t(k).to_string()));
                return;
            }
        };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = match id {
                None => api::post::<_, NotifyTarget>("/api/v1/notifications/targets", Some(&req)).await,
                Some(id) => api::put::<_, NotifyTarget>(&format!("/api/v1/notifications/targets/{id}"), &req).await,
            };
            busy.set(false);
            match r {
                Ok(tg) => {
                    toast(Tone::Ok, tf("nt.target_saved", &[("name", &tg.name)]));
                    targets.reload();
                    close.run(());
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    });
    let title = {
        let n = d.with_untracked(|d| d.name.clone());
        Signal::derive(move || if id.is_none() { t("nt.add_target").to_string() } else { tf("edit_named", &[("name", &n)]) })
    };
    let body = view! {
        <form class="form" on:submit=move |ev| { ev.prevent_default(); save.run(()); }>
            {text_field("nt.col.name", name, set_name, none, In { maxlength: 64, ..In::default() })}
            <div class="form-row">
                {select_field("nt.col.kind", None, KINDS, kind_s, set_kind)}
                {form::language_select("nt.f.language", Some("nt.f.language_help"), None, lang, set_lang)}
            </div>
            {switch("nt.f.enabled", None, enabled, set_enabled)}
            <h3 class="form-section">{tr("nt.f.connection")}</h3>
            {fields}
            <button type="submit" hidden=true></button>
        </form>
    }
    .into_any();
    let foot = save_bar(busy, Signal::derive(|| true), if id.is_none() { "create" } else { "save" }, close, save, error);
    drawer(title, close, body, foot)
}

// ------------------------------------------------------------------ rules

const SEV_OPTS: &[(&str, &str)] = &[
    ("info", "nt.sev.info"),
    ("notice", "nt.sev.notice"),
    ("warning", "nt.sev.warning"),
    ("error", "nt.sev.error"),
    ("critical", "nt.sev.critical"),
];
const MODES: &[(&str, &str)] = &[("all", "nt.mode.all"), ("any", "nt.mode.any")];
const GROUP_S: &[u64] = &[0, 60, 300, 900, 3600];
const REPEAT_S: &[u64] = &[0, 300, 900, 3600, 4 * 3600, 12 * 3600, 86400, 7 * 86400];
const DAYS: [&str; 7] = ["day.1", "day.2", "day.3", "day.4", "day.5", "day.6", "day.7"];

/// Select of a duration in seconds (0 = off); a value outside the list stays selectable.
fn secs_select(label: &'static str, help: &'static str, opts: &'static [u64], value: Signal<u64>, set: Callback<u64>) -> AnyView {
    let id = form::fid();
    let hid = format!("{id}h");
    let options = move || {
        let cur = value.get();
        let mut v = opts.to_vec();
        if !v.contains(&cur) {
            v.push(cur);
            v.sort_unstable();
        }
        v.into_iter()
            .map(|s| {
                let text = if s == 0 { t("off").to_string() } else { i18n::duration(s) };
                view! { <option value=s.to_string() selected=s == cur>{text}</option> }
            })
            .collect_view()
    };
    view! {
        <div class="field">
            <label class="field-label" for=id.clone()>{tr(label)}</label>
            <select id=id class="input" aria-describedby=hid.clone() on:change=move |ev| set.run(event_target_value(&ev).parse().unwrap_or(0))>{options}</select>
            <span class="field-help" id=hid>{tr(help)}</span>
        </div>
    }
    .into_any()
}

fn rules_panel(rules: Remote<Vec<NotifyRule>>, targets: Remote<Vec<NotifyTarget>>) -> AnyView {
    let edit = RwSignal::new(None::<NotifyRule>);
    let del = RwSignal::new(None::<(u64, String)>);
    let name_of = move |id: u64| targets.data.with(|l| l.iter().flatten().find(|t| t.id == id).map(target_name)).unwrap_or_else(|| format!("#{id}"));
    let table = move || {
        if let Some(v) = rules.gate(4) {
            return v;
        }
        let rows = rules.data.get().unwrap_or_default();
        if rows.is_empty() {
            return view! {
                <EmptyState icon=icons::LIST_CHECKS title=Signal::derive(|| t("nt.no_rules").to_string()) text=Signal::derive(|| t("nt.no_rules_text").to_string()) />
            }
            .into_any();
        }
        view! {
            {ui::table("", &["nt.col.name", "nt.col.matches", "nt.col.sends_to", "col.status", ""], rows.into_iter().map(|r| {
                            let (id, name) = (r.id, r.name.clone());
                            let (r2, r3) = (r.clone(), r.clone());
                            let n2 = name.clone();
                            ui::row(vec![
                                ui::cell("nt.col.name", "", view! { <button class="link-strong" on:click=move |_| edit.set(Some(r2.clone()))>{name.clone()}</button> }.into_any()),
                                ui::cell("nt.col.matches", "wrap-cell", view! {
                                    <div class="chips">
                                        {r.invert.then(|| view! { {ui::chip(ui::Tone::Warn, tr("nt.inverted").into_any())} })}
                                        <span class="chip">{format!("≥ {}", t(severity_key(r.min_severity)))}</span>
                                        {if r.categories.is_empty() {
                                            view! { <span class="chip">{tr("nt.f.any_cat")}</span> }.into_any()
                                        } else {
                                            r.categories.iter().map(|c| view! { <span class="chip">{tr(category_key(*c))}</span> }).collect_view().into_any()
                                        }}
                                        {r.source.clone().map(|s| view! { <span class="chip mono">{s}</span> })}
                                        {r.fields.iter().map(|f| view! { <span class="chip mono">{format!("{}={}", f.field, f.value)}</span> }).collect_view()}
                                    </div>
                                }
                                .into_any()),
                                ui::cell("nt.col.sends_to", "", view! { <div class="chips">{r.targets.iter().map(|id| view! { <span class="chip accent">{name_of(*id)}</span> }).collect_view()}</div> }.into_any()),
                                ui::cell("col.status", "", view! {
                                    <div class="chips">
                                        {if r.enabled { ui::status(Tone::Ok, t("nt.enabled").to_string()) } else { ui::status(Tone::Unknown, t("nt.disabled").to_string()) }}
                                        {(!r.quiet_hours.is_empty()).then(|| view! { <span class="chip"><Icon icon=icons::MOON size=12 />{tr("nt.quiet")}</span> })}
                                    </div>
                                }
                                .into_any()),
                                ui::actions(view! {
                                    <button class="icon-btn sm" title=tr("edit") aria-label=format!("{} {}", t("edit"), r.name) on:click=move |_| edit.set(Some(r3.clone()))>
                                        <Icon icon=icons::PENCIL size=16 />
                                    </button>
                                    <button class="icon-btn sm danger" title=tr("delete") aria-label=format!("{} {}", t("delete"), r.name) on:click=move |_| del.set(Some((id, n2.clone())))>
                                        <Icon icon=icons::TRASH_2 size=16 />
                                    </button>
                                }
                                .into_any()),
                            ])
                        }).collect::<Vec<_>>())}
        }
        .into_any()
    };
    let editor = move || {
        edit.get().map(|r| {
            let all: Vec<(u64, String)> = targets.data.with_untracked(|l| l.iter().flatten().map(|t| (t.id, target_name(t))).collect());
            rule_editor(r, all, rules, Callback::new(move |_| edit.set(None)))
        })
    };
    let deleter = move || {
        del.get().map(|(id, name)| {
            let busy = RwSignal::new(false);
            let error = RwSignal::new(None::<String>);
            let close = Callback::new(move |_| del.set(None));
            confirm(Confirm {
                title: tf("nt.delete_rule", &[("name", &name)]),
                text: t("nt.delete_rule_text").to_string(),
                action: "delete",
                danger: true,
                extra: None,
                typed: Signal::stored(None),
                busy,
                error,
                on_confirm: Callback::new(move |_| {
                    busy.set(true);
                    spawn_local(async move {
                        let r = api::delete_empty(&format!("/api/v1/notifications/rules/{id}")).await;
                        busy.set(false);
                        match r {
                            Ok(()) => {
                                rules.reload();
                                close.run(());
                            }
                            Err(e) => error.set(Some(e.text())),
                        }
                    });
                }),
                on_cancel: close,
            })
        })
    };
    let new_rule = move || {
        let ui = targets.data.with_untracked(|l| l.iter().flatten().filter(|t| t.enabled).map(|t| t.id).collect::<Vec<_>>());
        edit.set(Some(NotifyRule { enabled: true, min_severity: NotifySeverity::Warning, targets: ui, ..Default::default() }))
    };
    view! {
        <div class="toolbar">
            <p class="muted small grow">{tr("nt.rules_help")}</p>
            <button class="btn primary" on:click=move |_| new_rule()>
                <Icon icon=icons::PLUS size=16 /><span class="btn-label">{tr("add_rule")}</span>
            </button>
        </div>
        {table}
        {editor}
        {deleter}
    }
    .into_any()
}

fn rule_editor(init: NotifyRule, all_targets: Vec<(u64, String)>, rules: Remote<Vec<NotifyRule>>, close: Callback<()>) -> AnyView {
    let is_new = init.id == 0;
    let d = RwSignal::new(init);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let none = Signal::derive(|| None::<String>);
    let (name, set_name) = lens(d, |r| r.name.clone(), |r, v| r.name = v);
    let (enabled, set_enabled) = lens_bool(d, |r| r.enabled, |r, v| r.enabled = v);
    let (invert, set_invert) = lens_bool(d, |r| r.invert, |r, v| r.invert = v);
    let (sev, set_sev) = lens(d, |r| r.min_severity.as_str().to_string(), |r, v| r.min_severity = NotifySeverity::parse(&v).unwrap_or_default());
    let (mode, set_mode) = lens(d, |r| if r.mode == MatchMode::Any { "any" } else { "all" }.to_string(), |r, v| r.mode = if v == "any" { MatchMode::Any } else { MatchMode::All });
    let (source, set_source) = lens(d, |r| r.source.clone().unwrap_or_default(), |r, v| r.source = opt(&v));

    let categories = NotifyCategory::ALL
        .into_iter()
        .map(|c| {
            view! {
                <label class="check">
                    <input type="checkbox" prop:checked=move || d.with(|r| r.categories.contains(&c))
                        on:change=move |ev| { let on = event_target_checked(&ev); d.update(|r| { r.categories.retain(|x| *x != c); if on { r.categories.push(c); r.categories.sort(); } }); } />
                    {tr(category_key(c))}
                </label>
            }
        })
        .collect_view();
    let no_targets = all_targets.is_empty();
    let targets = all_targets
        .into_iter()
        .map(|(id, n)| {
            view! {
                <label class="check">
                    <input type="checkbox" prop:checked=move || d.with(|r| r.targets.contains(&id))
                        on:change=move |ev| { let on = event_target_checked(&ev); d.update(|r| { r.targets.retain(|x| *x != id); if on { r.targets.push(id); } }); } />
                    {n}
                </label>
            }
        })
        .collect_view();
    let fields_count = Memo::new(move |_| d.with(|r| r.fields.len()));
    let field_rows = move || {
        (0..fields_count.get())
            .map(|i| {
                view! {
                    <div class="secret-row">
                        <input class="input mono" aria-label=tr("nt.f.field") placeholder=tr("nt.f.field")
                            prop:value=move || d.with(|r| r.fields.get(i).map(|f| f.field.clone()).unwrap_or_default())
                            on:input=move |ev| { let v = event_target_value(&ev); d.update(|r| if let Some(f) = r.fields.get_mut(i) { f.field = v; }); } />
                        <input class="input mono grow" aria-label=tr("nt.f.value") placeholder=tr("nt.f.value")
                            prop:value=move || d.with(|r| r.fields.get(i).map(|f| f.value.clone()).unwrap_or_default())
                            on:input=move |ev| { let v = event_target_value(&ev); d.update(|r| if let Some(f) = r.fields.get_mut(i) { f.value = v; }); } />
                        <label class="check">
                            <input type="checkbox" prop:checked=move || d.with(|r| r.fields.get(i).is_some_and(|f| f.regex))
                                on:change=move |ev| { let on = event_target_checked(&ev); d.update(|r| if let Some(f) = r.fields.get_mut(i) { f.regex = on; }); } />
                            {tr("nt.f.regex")}
                        </label>
                        <button type="button" class="icon-btn sm" aria-label=tr("delete") on:click=move |_| d.update(|r| { if i < r.fields.len() { r.fields.remove(i); } })>
                            <Icon icon=icons::X size=14 />
                        </button>
                    </div>
                }
            })
            .collect_view()
    };
    let quiet_count = Memo::new(move |_| d.with(|r| r.quiet_hours.len()));
    let quiet_rows = move || {
        (0..quiet_count.get())
            .map(|i| {
                let days = (1..=7u8)
                    .map(|day| {
                        view! {
                            <label class="check day">
                                <input type="checkbox" prop:checked=move || d.with(|r| r.quiet_hours.get(i).is_some_and(|q| q.days.contains(&day)))
                                    on:change=move |ev| {
                                        let on = event_target_checked(&ev);
                                        d.update(|r| if let Some(q) = r.quiet_hours.get_mut(i) { q.days.retain(|x| *x != day); if on { q.days.push(day); q.days.sort_unstable(); } });
                                    } />
                                {tr(DAYS[day as usize - 1])}
                            </label>
                        }
                    })
                    .collect_view();
                view! {
                    <div class="quiet-row">
                        <div class="quiet-days" role="group" aria-label=tr("nt.f.days")>{days}</div>
                        <div class="secret-row">
                            <input class="input" type="time" aria-label=tr("nt.f.from_time")
                                prop:value=move || d.with(|r| r.quiet_hours.get(i).map(|q| q.start.clone()).unwrap_or_default())
                                on:input=move |ev| { let v = event_target_value(&ev); d.update(|r| if let Some(q) = r.quiet_hours.get_mut(i) { q.start = v; }); } />
                            <span class="muted">"–"</span>
                            <input class="input" type="time" aria-label=tr("nt.f.to_time")
                                prop:value=move || d.with(|r| r.quiet_hours.get(i).map(|q| q.end.clone()).unwrap_or_default())
                                on:input=move |ev| { let v = event_target_value(&ev); d.update(|r| if let Some(q) = r.quiet_hours.get_mut(i) { q.end = v; }); } />
                            <button type="button" class="icon-btn sm" aria-label=tr("delete") on:click=move |_| d.update(|r| { if i < r.quiet_hours.len() { r.quiet_hours.remove(i); } })>
                                <Icon icon=icons::X size=14 />
                            </button>
                        </div>
                    </div>
                }
            })
            .collect_view()
    };

    let save = Callback::new(move |_| {
        let mut r = d.get_untracked();
        r.name = r.name.trim().to_string();
        r.fields.retain(|f| !f.field.trim().is_empty());
        if r.name.is_empty() {
            error.set(Some(t("nt.v.name").to_string()));
            return;
        }
        if r.targets.is_empty() {
            error.set(Some(t("nt.v.targets").to_string()));
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let res = if is_new {
                api::post::<_, NotifyRule>("/api/v1/notifications/rules", Some(&r)).await
            } else {
                api::put::<_, NotifyRule>(&format!("/api/v1/notifications/rules/{}", r.id), &r).await
            };
            busy.set(false);
            match res {
                Ok(r) => {
                    toast(Tone::Ok, tf("nt.rule_saved", &[("name", &r.name)]));
                    rules.reload();
                    close.run(());
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    });
    let title = {
        let n = d.with_untracked(|r| r.name.clone());
        Signal::derive(move || if is_new { t("add_rule").to_string() } else { tf("edit_named", &[("name", &n)]) })
    };
    let body = view! {
        <form class="form" on:submit=move |ev| { ev.prevent_default(); save.run(()); }>
            {text_field("nt.col.name", name, set_name, none, In { maxlength: 64, ..In::default() })}
            {switch("nt.f.enabled", None, enabled, set_enabled)}
            <h3 class="form-section">{tr("nt.f.when")}</h3>
            <div class="form-row">
                {select_field("nt.f.min_severity", None, SEV_OPTS, sev, set_sev)}
                {select_field("nt.f.mode", Some("nt.f.mode_help"), MODES, mode, set_mode)}
            </div>
            <fieldset class="fieldset">
                <legend class="field-label">{tr("nt.col.category")}</legend>
                <p class="field-help">{tr("nt.f.categories_help")}</p>
                <div class="check-grid">{categories}</div>
            </fieldset>
            {text_field("nt.source", source, set_source, none, In { mono: true, help: Some("nt.f.source_help"), placeholder: "^storage$", ..In::default() })}
            <fieldset class="fieldset">
                <legend class="field-label">{tr("nt.fields")}</legend>
                {field_rows}
                <button type="button" class="btn ghost sm self-start" on:click=move |_| d.update(|r| r.fields.push(FieldMatch::default()))>
                    <Icon icon=icons::PLUS size=14 />{tr("nt.f.add_field")}
                </button>
            </fieldset>
            {switch("nt.f.invert", Some("nt.f.invert_help"), invert, set_invert)}
            <h3 class="form-section">{tr("nt.f.deliver")}</h3>
            <fieldset class="fieldset">
                <legend class="field-label">{tr("nt.col.sends_to")}</legend>
                {if no_targets {
                    view! { <p class="field-help">{tr("nt.no_targets")}</p> }.into_any()
                } else {
                    view! { <div class="check-grid">{targets}</div> }.into_any()
                }}
            </fieldset>
            <div class="form-row">
                {secs_select("nt.f.group", "nt.f.group_help", GROUP_S, Signal::derive(move || d.with(|r| r.group_window_s)), Callback::new(move |v| d.update(|r| r.group_window_s = v)))}
                {secs_select("nt.f.repeat", "nt.f.repeat_help", REPEAT_S, Signal::derive(move || d.with(|r| r.repeat_interval_s)), Callback::new(move |v| d.update(|r| r.repeat_interval_s = v)))}
            </div>
            <fieldset class="fieldset">
                <legend class="field-label">{tr("nt.quiet")}</legend>
                <p class="field-help">{tr("nt.f.quiet_help")}</p>
                {quiet_rows}
                <button type="button" class="btn ghost sm self-start"
                    on:click=move |_| d.update(|r| r.quiet_hours.push(QuietWindow { days: vec![], start: "22:00".into(), end: "07:00".into() }))>
                    <Icon icon=icons::PLUS size=14 />{tr("nt.f.add_quiet")}
                </button>
            </fieldset>
            <button type="submit" hidden=true></button>
        </form>
    }
    .into_any();
    let foot = save_bar(busy, Signal::derive(|| true), if is_new { "create" } else { "save" }, close, save, error);
    drawer(title, close, body, foot)
}

// ------------------------------------------------------------------ silences

fn silences_panel(silences: Remote<Vec<Silence>>) -> AnyView {
    let add = RwSignal::new(false);
    let removing = RwSignal::new(None::<u64>);
    let remove = move |id: u64| {
        removing.set(Some(id));
        spawn_local(async move {
            let r = api::delete_empty(&format!("/api/v1/notifications/silences/{id}")).await;
            removing.set(None);
            match r {
                Ok(()) => silences.reload(),
                Err(e) => toast(Tone::Err, e.text()),
            }
        });
    };
    let table = move || {
        if let Some(v) = silences.gate(3) {
            return v;
        }
        let rows = silences.data.get().unwrap_or_default();
        if rows.is_empty() {
            return view! {
                <EmptyState icon=icons::BELL_OFF title=Signal::derive(|| t("nt.no_silences").to_string()) text=Signal::derive(|| t("nt.no_silences_text").to_string()) />
            }
            .into_any();
        }
        view! {
            {ui::table("", &["nt.key", "nt.col.until", "nt.col.created", ""], rows.into_iter().map(|s| {
                            let id = s.id;
                            ui::row(vec![
                                ui::cell("nt.key", "mono", view! { {s.key.clone()} }.into_any()),
                                ui::cell("nt.col.until", "tnum", view! { {s.until_ms.map(i18n::datetime).unwrap_or_else(|| t("nt.for_ever").to_string())} }.into_any()),
                                ui::cell("nt.col.created", "tnum", view! { {format!("{} · {}", i18n::short_when(s.created_ms), s.created_by)} }.into_any()),
                                ui::actions(view! {
                                    <button class="icon-btn sm danger" title=tr("nt.unsilence") aria-label=ui::verb_name("nt.unsilence", &s.key)
                                        disabled=move || removing.get().is_some() on:click=move |_| remove(id)>
                                        <Icon icon=icons::TRASH_2 size=16 />
                                    </button>
                                }
                                .into_any()),
                            ])
                        }).collect::<Vec<_>>())}
        }
        .into_any()
    };
    view! {
        <div class="toolbar">
            <p class="muted small grow">{tr("nt.silences_help")}</p>
            <button class="btn primary" on:click=move |_| add.set(true)>
                <Icon icon=icons::BELL_OFF size=16 /><span class="btn-label">{tr("nt.add_silence")}</span>
            </button>
        </div>
        {table}
        {move || add.get().then(|| silence_dialog(String::new(), Callback::new(move |_| add.set(false)), Callback::new(move |_| silences.reload())))}
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::notify::TargetStatus;

    fn target(config: TargetConfig, secrets: &[&str]) -> NotifyTarget {
        NotifyTarget {
            id: 7,
            name: "alice phone".into(),
            enabled: true,
            language: NotifyLanguage::new("it").unwrap(),
            config,
            secrets: secrets.iter().map(|s| s.to_string()).collect(),
            status: TargetStatus::default(),
        }
    }

    #[test]
    fn draft_roundtrip_keeps_config_and_secrets() {
        let t = target(TargetConfig::Ntfy { server: "https://ntfy.sh".into(), topic: "nas".into(), username: None }, &["token"]);
        let d = TargetDraft::from_target(&t);
        let r = d.request().unwrap();
        assert_eq!(r.config, t.config);
        assert_eq!(r.language, NotifyLanguage::new("it").unwrap());
        // a language the table no longer has is edited as English, so the target can be saved
        let mut gone = t.clone();
        gone.language = NotifyLanguage::new("xx").unwrap();
        assert_eq!(TargetDraft::from_target(&gone).request().unwrap().language, NotifyLanguage::EN);
        // an untouched secret is kept (absent from the request)
        assert!(r.secrets.is_empty());
        let mut d2 = d.clone();
        d2.secrets[0].value = "tk_new".into();
        d2.secrets[1].remove = true; // never set: nothing to remove
        assert_eq!(d2.request().unwrap().secrets, BTreeMap::from([("token".to_string(), Some("tk_new".to_string()))]));
    }

    #[test]
    fn changing_kind_removes_unused_secrets() {
        let t = target(TargetConfig::Gotify { server: "https://gotify.example.org".into() }, &["token"]);
        let mut d = TargetDraft::from_target(&t);
        d.set_kind("telegram");
        d.chat_id = "12345".into();
        let r = d.request().unwrap();
        assert_eq!(r.secrets.get("token"), Some(&None));
        assert!(matches!(r.config, TargetConfig::Telegram { .. }));
    }

    #[test]
    fn smtp_recipients_and_validation() {
        let mut d = TargetDraft::new_target();
        d.name = "mail".into();
        d.set_kind("smtp");
        d.host = "smtp.example.com".into();
        d.from = "nas@example.com".into();
        assert_eq!(d.request().unwrap_err(), "nt.v.addresses");
        d.to = "alice@example.com, bob@example.com\n".into();
        match d.request().unwrap().config {
            TargetConfig::Smtp { to, port, .. } => {
                assert_eq!(to, ["alice@example.com", "bob@example.com"]);
                assert_eq!(port, 587);
            }
            _ => panic!("smtp expected"),
        }
        d.port = "x".into();
        assert_eq!(d.request().unwrap_err(), "nt.v.port");
    }

    #[test]
    fn center_query() {
        assert_eq!(list_url("", "", "false", 50, None), "/api/v1/notifications?limit=50&acked=false");
        assert_eq!(list_url("warning", "storage", "", 50, Some(9)), "/api/v1/notifications?limit=50&severity=warning&category=storage&before=9");
    }
}
