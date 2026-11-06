//! Network (DSM Control Panel › Network, Proxmox System › Network, Cockpit Networking): the
//! interfaces with their live state, staged IPv4 settings with a clear diff, and the timed
//! transaction of D-NET — Apply starts a test that rolls back by itself unless it is confirmed
//! from the new network within N seconds ([`trial`]). Revisions with restore, rescue paths
//! (link-local, mDNS, Synology reset button read-only), ifupdown adoption and diagnostics
//! ([`tools`]).
//!
//! Phase 1 of the API edits IPv4 (DHCP / static / off), gateway, DNS and MTU of interfaces that
//! systemd-networkd manages; IPv6, bonds, VLANs and bridges are shown read-only.

pub mod edit;
pub mod tools;
pub mod trial;

use std::net::Ipv4Addr;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::network::{
    AdoptMode, Duplex, InterfaceConfig, IpFamily, Ipv4Method, LinkKind, NetAdoptRequest, NetAdoptScheduled, NetApplyRequest,
    NetApplyStarted, NetBackend, NetInterface, NetRevision, NetworkState, ReadOnlyCode, ReadOnlyReason, RescueInfo,
};

use crate::api;
use crate::form::{switch, tabs};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::ui::{self, Confirm, DlgKind, EmptyState, Notice, PageHeader, Remote, Tone, WidgetHead, confirm, dialog};

// ------------------------------------------------------------------ store

/// Everything the page reads, shared by its parts.
#[derive(Clone, Copy)]
pub struct Net {
    pub ifaces: Remote<Vec<NetInterface>>,
    pub state: Remote<NetworkState>,
    pub revs: Remote<Vec<NetRevision>>,
    pub rescue: Remote<RescueInfo>,
    pub trial: trial::Trial,
}

impl Net {
    fn new() -> Self {
        let ifaces = Remote::with_config("/api/v1/network/interfaces");
        let state = Remote::with_config("/api/v1/network/state");
        let revs = Remote::with_config("/api/v1/network/revisions");
        let rescue = Remote::with_config("/api/v1/network/rescue");
        let net = Net { ifaces, state, revs, rescue, trial: trial::Trial::new() };
        net.trial.init(net);
        net
    }

    /// Read everything again (after a staged change, an apply, a rollback).
    pub fn reload(self) {
        self.ifaces.reload();
        self.state.reload();
        self.revs.reload();
        self.rescue.reload();
    }

    /// A timed test (or a migration planned for the next boot) is running.
    pub fn trial_running(self) -> bool {
        self.trial.started.with(Option::is_some) || self.state.data.with(|s| s.as_ref().is_some_and(|s| s.trial.is_some()))
    }
}

// ------------------------------------------------------------------ page

#[component]
pub fn Network() -> impl IntoView {
    let net = Net::new();
    let tab = RwSignal::new(0usize);
    view! {
        <div class="page">
            <PageHeader icon=icons::NETWORK title="net.title" subtitle="net.subtitle">
                <button class="btn ghost" on:click=move |_| net.reload()>
                    <Icon icon=icons::REFRESH_CW size=16 /><span class="btn-label">{tr("cfg.refresh")}</span>
                </button>
            </PageHeader>
            {trial::banner(net)}
            <Notice msg=net.trial.outcome />
            {rollback_notice(net)}
            {adopt_card(net)}
            {pending_card(net)}
            {overview(net)}
            <section class="card widget w-12">
                {tabs("net", &["net.tab_ifaces", "net.tab_revisions", "net.tab_rescue", "net.tab_tools"], tab, "net.title")}
                <div role="tabpanel" id="net-p0" aria-labelledby="net-t0" hidden=move || tab.get() != 0>{interfaces_panel(net)}</div>
                <div role="tabpanel" id="net-p1" aria-labelledby="net-t1" hidden=move || tab.get() != 1>{tools::revisions_panel(net)}</div>
                <div role="tabpanel" id="net-p2" aria-labelledby="net-t2" hidden=move || tab.get() != 2>{tools::rescue_panel(net)}</div>
                <div role="tabpanel" id="net-p3" aria-labelledby="net-t3" hidden=move || tab.get() != 3>{tools::diagnostics_panel(net)}</div>
            </section>
        </div>
    }
}

// ------------------------------------------------------------------ overview

fn on_off(on: bool, key_on: &'static str, key_off: &'static str) -> AnyView {
    if on { ui::status(Tone::Ok, t(key_on).to_string()) } else { ui::status(Tone::Unknown, t(key_off).to_string()) }
}

fn kv(label: &'static str, value: AnyView) -> AnyView {
    view! { <div class="kv"><dt>{tr(label)}</dt><dd>{value}</dd></div> }.into_any()
}

fn overview(net: Net) -> AnyView {
    let summary = move || {
        if let Some(v) = net.state.gate(5) {
            return v;
        }
        let s = net.state.data.get().unwrap_or_default();
        let gateways: Vec<String> = s
            .routes
            .iter()
            .filter(|r| r.destination == "default")
            .filter_map(|r| r.gateway.as_ref().map(|g| format!("{g} ({})", r.interface)))
            .collect();
        let mgmt = match &s.management_interface {
            Some(m) => view! { <span class="mono">{m.clone()}</span> }.into_any(),
            None => view! { <span class="muted">{tr("net.mgmt_unknown")}</span> }.into_any(),
        };
        let dash = || "—".to_string();
        view! {
            <dl class="kvs">
                {kv("net.hostname", view! { <span class="mono">{s.hostname.clone()}</span> }.into_any())}
                {kv("net.mgmt", mgmt)}
                {kv("net.gateway", view! { <span class="mono">{if gateways.is_empty() { dash() } else { gateways.join(", ") }}</span> }.into_any())}
                {kv("net.dns", view! { <span class="mono">{if s.dns.servers.is_empty() { dash() } else { s.dns.servers.join(", ") }}</span> }.into_any())}
                {(!s.dns.search.is_empty()).then(|| kv("net.search", view! { <span class="mono">{s.dns.search.join(", ")}</span> }.into_any()))}
                {kv("net.lkg", match s.lkg_revision {
                    Some(r) => format!("#{r}").into_any(),
                    None => view! { <span class="muted">{tr("net.lkg_none")}</span> }.into_any(),
                })}
            </dl>
        }
        .into_any()
    };
    let services = move || {
        if let Some(v) = net.state.gate(5) {
            return v;
        }
        let s = net.state.data.with(|s| s.as_ref().map(|s| s.services.clone())).unwrap_or_default();
        view! {
            <dl class="kvs">
                {kv("net.svc_networkd", on_off(s.networkd, "net.running", "net.not_running"))}
                {kv("net.svc_resolved", on_off(s.resolved, "net.running", "net.not_running"))}
                {kv("net.svc_ifupdown", on_off(s.ifupdown, "net.running", "net.not_running"))}
                {kv("net.svc_nm", on_off(s.network_manager, "net.running", "net.not_running"))}
                {kv("net.svc_avahi", on_off(s.avahi, "net.running", "net.not_running"))}
            </dl>
        }
        .into_any()
    };
    view! {
        <div class="grid">
            <section class="card widget w-8">
                <WidgetHead icon=icons::NETWORK title="net.summary" />
                {summary}
            </section>
            <section class="card widget w-4">
                <WidgetHead icon=icons::SERVER title="net.services" />
                {services}
            </section>
        </div>
    }
    .into_any()
}

/// The last automatic rollback, when its result could not be verified (critical) or it
/// happened in the last 24 hours.
fn rollback_notice(net: Net) -> AnyView {
    (move || {
        let r = net.state.data.with(|s| s.as_ref().and_then(|s| s.last_rollback.clone()))?;
        let recent = js_sys::Date::now() - (r.ts_ms as f64) < 86_400_000.0;
        if r.verified && !recent {
            return None;
        }
        let reason = i18n::try_t(&format!("net.reason.{}", r.reason)).unwrap_or(t("net.reason.other"));
        let mut text = tf(
            "net.rolled_back_at",
            &[("date", &i18n::datetime(r.ts_ms)), ("reason", reason), ("rev", &r.restored_revision.to_string())],
        );
        if !r.verified {
            text.push(' ');
            text.push_str(t("net.rollback_unverified"));
            if !r.problems.is_empty() {
                text.push(' ');
                text.push_str(&r.problems.join("; "));
            }
        }
        let tone = if r.verified { Tone::Warn } else { Tone::Err };
        Some(ui::note(tone, icons::ROTATE_CCW, text.into_any()))
    })
    .into_any()
}

// ------------------------------------------------------------------ ifupdown adoption

fn adopt_card(net: Net) -> AnyView {
    let open = RwSignal::new(false);
    let show = move || net.state.data.with(|s| s.as_ref().is_some_and(|s| s.ifupdown_adoptable)) && !net.trial_running();
    view! {
        <Show when=show>
            <section class="card net-callout">
                <span class="net-callout-icon"><Icon icon=icons::IMPORT size=20 /></span>
                <div class="net-callout-text">
                    <strong>{tr("net.adopt_title")}</strong>
                    <p class="muted small">{tr("net.adopt_text")}</p>
                </div>
                <button class="btn primary" on:click=move |_| open.set(true)>{tr("net.adopt_btn")}</button>
            </section>
        </Show>
        {move || open.get().then(|| adopt_dialog(net, Callback::new(move |_| open.set(false))))}
    }
    .into_any()
}

fn adopt_dialog(net: Net, close: Callback<()>) -> AnyView {
    let mode = RwSignal::new(AdoptMode::NextBoot);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let timeout = RwSignal::new(net.state.data.with_untracked(|s| s.as_ref().map(|s| s.timeout_default_s).unwrap_or(120)));
    let radio = move |m: AdoptMode, label: &'static str, help: &'static str| {
        view! {
            <label class="net-radio">
                <input type="radio" name="adopt-mode" prop:checked=move || mode.get() == m on:change=move |_| mode.set(m) />
                <span class="switch-text">
                    <span class="switch-label">{tr(label)}</span>
                    <span class="field-help">{tr(help)}</span>
                </span>
            </label>
        }
    };
    let extra = view! {
        <fieldset class="fieldset">
            <legend class="field-label">{tr("net.adopt_when")}</legend>
            {radio(AdoptMode::NextBoot, "net.adopt_next_boot", "net.adopt_next_boot_help")}
            {radio(AdoptMode::Now, "net.adopt_now", "net.adopt_now_help")}
        </fieldset>
        <div hidden=move || mode.get() != AdoptMode::Now>{timeout_select(net, timeout)}</div>
    }
    .into_any();
    let on_confirm = Callback::new(move |_| {
        busy.set(true);
        error.set(None);
        let m = mode.get_untracked();
        let body = NetAdoptRequest { mode: m, timeout_s: Some(timeout.get_untracked()), confirm: true };
        spawn_local(async move {
            let r = match m {
                AdoptMode::Now => api::post::<_, NetApplyStarted>("/api/v1/network/adopt", Some(&body)).await.map(|s| {
                    net.trial.begin(net, s);
                }),
                AdoptMode::NextBoot => api::post::<_, NetAdoptScheduled>("/api/v1/network/adopt", Some(&body)).await.map(|s| {
                    net.trial.outcome.set(Some((Tone::Info, tf("net.adopt_scheduled", &[("ifaces", &s.interfaces.join(", "))]))));
                }),
            };
            busy.set(false);
            match r {
                Ok(()) => {
                    net.reload();
                    close.run(());
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    });
    confirm(Confirm {
        title: t("net.adopt_title").to_string(),
        text: t("net.adopt_dialog_text").to_string(),
        action: "net.adopt_go",
        danger: false,
        extra: Some(extra),
        typed: Signal::stored(None),
        busy,
        error,
        on_confirm,
        on_cancel: close,
    })
}

// ------------------------------------------------------------------ staged changes

/// Timeout choices (seconds) inside the server's limits; the default is always offered.
fn timeout_select(net: Net, value: RwSignal<u32>) -> AnyView {
    let (min, max, def) =
        net.state.data.with_untracked(|s| s.as_ref().map(|s| (s.timeout_min_s, s.timeout_max_s, s.timeout_default_s)).unwrap_or((60, 600, 120)));
    let mut opts: Vec<u32> = [60, 90, 120, 180, 300, 600].into_iter().filter(|v| (min..=max).contains(v)).collect();
    if !opts.contains(&def) {
        opts.push(def);
        opts.sort_unstable();
    }
    let id = crate::form::fid();
    view! {
        <div class="field">
            <label class="field-label" for=id.clone()>{tr("net.timeout")}</label>
            <select id=id class="input" on:change=move |ev| value.set(event_target_value(&ev).parse().unwrap_or(def))>
                {opts.into_iter().map(|v| view! {
                    <option value=v.to_string() selected=move || value.get() == v>{tf("net.seconds", &[("n", &v.to_string())])}</option>
                }).collect_view()}
            </select>
            <span class="field-help">{tr("net.timeout_help")}</span>
        </div>
    }
    .into_any()
}

fn pending_card(net: Net) -> AnyView {
    let review = RwSignal::new(false);
    let discard = RwSignal::new(false);
    let pending = move || net.state.data.with(|s| s.as_ref().map(|s| s.pending.clone()).unwrap_or_default());
    let show = move || !pending().is_empty() && !net.trial_running();
    let do_discard = Callback::new(move |_| {
        spawn_local(async move {
            match api::post_empty::<()>("/api/v1/network/discard", None).await {
                Ok(()) => {
                    crate::toast::toast(Tone::Ok, t("net.discarded").to_string());
                    net.reload();
                }
                Err(e) => crate::toast::toast(Tone::Err, e.text()),
            }
        });
    });
    view! {
        <Show when=show>
            <section class="card net-callout pending-net" aria-live="polite">
                <span class="net-callout-icon warn"><Icon icon=icons::FILE_DIFF size=20 /></span>
                <div class="net-callout-text">
                    <strong>{tr("net.pending_title")}</strong>
                    <p class="muted small">{move || tf("net.pending_text", &[("ifaces", &pending().join(", "))])}</p>
                </div>
                <div class="card-actions">
                    <button class="btn ghost" on:click=move |_| discard.set(true)>{tr("pending.discard")}</button>
                    <button class="btn primary" on:click=move |_| review.set(true)>{tr("net.review_apply")}</button>
                </div>
            </section>
        </Show>
        {move || discard.get().then(|| confirm(Confirm::new(
            t("net.discard_title").to_string(),
            t("net.discard_text").to_string(),
            "pending.discard",
            true,
            Callback::new(move |_| {
                discard.set(false);
                do_discard.run(());
            }),
            Callback::new(move |_| discard.set(false)),
        )))}
        {move || review.get().then(|| apply_dialog(net, review))}
    }
    .into_any()
}

/// Review of the staged changes and the Apply form (timeout, comment, test).
fn apply_dialog(net: Net, open: RwSignal<bool>) -> AnyView {
    let st = net.state.data.get_untracked().unwrap_or_default();
    let changed: Vec<NetInterface> = net.ifaces.data.with_untracked(|l| l.iter().flatten().filter(|i| i.pending.is_some()).cloned().collect());
    let mgmt = st.management_interface.clone();
    let touched = mgmt.as_ref().is_some_and(|m| st.pending.contains(m));
    let can_skip = mgmt.is_some() && !touched;
    let timeout = RwSignal::new(st.timeout_default_s.max(st.timeout_min_s));
    let comment = RwSignal::new(String::new());
    let skip = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let apply = move || {
        busy.set(true);
        error.set(None);
        let body = NetApplyRequest { timeout_s: Some(timeout.get_untracked()), skip_test: skip.get_untracked(), comment: comment.get_untracked() };
        spawn_local(async move {
            let r = api::post::<_, NetApplyStarted>("/api/v1/network/apply", Some(&body)).await;
            busy.set(false);
            match r {
                Ok(s) => {
                    open.set(false);
                    net.trial.begin(net, s);
                    net.reload();
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    };
    let mgmt_note = match (&mgmt, touched) {
        (Some(m), true) => {
            let new_addrs = changed
                .iter()
                .find(|i| &i.name == m)
                .and_then(|i| i.pending.as_ref())
                .map(|c| match c.ipv4 {
                    Ipv4Method::Static => c.addresses.iter().map(|a| a.split('/').next().unwrap_or(a).to_string()).collect::<Vec<_>>().join(", "),
                    _ => String::new(),
                })
                .unwrap_or_default();
            let text = if new_addrs.is_empty() {
                tf("net.mgmt_touched_dhcp", &[("iface", m)])
            } else {
                tf("net.mgmt_touched", &[("iface", m), ("addrs", &new_addrs)])
            };
            Some(text)
        }
        (None, _) => Some(t("net.mgmt_unknown_apply").to_string()),
        _ => None,
    }
    .map(|text| view! { {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {text} }.into_any())} });
    let diffs = changed
        .iter()
        .map(|i| {
            view! {
                <div class="review-key">
                    <div class="review-head"><h3 class="mono">{i.name.clone()}</h3>{i.management.then(|| view! { <span class="chip accent">{tr("net.this_connection")}</span> })}</div>
                    {diff_table(i.applied.as_ref(), i.pending.as_ref().unwrap_or(&InterfaceConfig::default()))}
                </div>
            }
        })
        .collect_view();
    let cid = crate::form::fid();
    let form_id = crate::form::fid();
    let body = view! {
        <form class="form" id=form_id.clone() on:submit=move |ev| { ev.prevent_default(); apply(); }>
            <div class="net-review">{diffs}</div>
            {mgmt_note}
            <div class="form-row">
                {timeout_select(net, timeout)}
                <div class="field">
                    <label class="field-label" for=cid.clone()>{tr("pending.comment")}</label>
                    <input id=cid class="input" maxlength="200" placeholder=tr("pending.comment_hint") prop:value=move || comment.get() on:input=move |ev| comment.set(event_target_value(&ev)) />
                </div>
            </div>
            {if can_skip {
                switch("net.skip_test", Some("net.skip_test_help"), skip.into(), Callback::new(move |v| skip.set(v)))
            } else {
                view! { <p class="sec-note"><Icon icon=icons::LOCK size=16 />{tr("net.test_mandatory")}</p> }.into_any()
            }}
            {crate::form::form_error(error)}
        </form>
    }
    .into_any();
    let foot = view! {
        <button type="button" class="btn ghost" on:click=move |_| open.set(false)>{tr("cancel")}</button>
        <button type="submit" form=form_id class="btn primary" disabled=move || busy.get()>
            {crate::form::spinner(busy)}
            {move || if skip.get() { t("net.apply_now").to_string() } else { tf("net.apply_test", &[("n", &timeout.get().to_string())]) }}
        </button>
    }
    .into_any();
    dialog(DlgKind::Wide, Signal::derive(|| t("net.apply_title").to_string()), Callback::new(move |_| open.set(false)), body, Some(foot))
}

// ------------------------------------------------------------------ interfaces

fn kind_key(k: LinkKind) -> &'static str {
    match k {
        LinkKind::Ethernet => "net.k.ethernet",
        LinkKind::Bond => "net.k.bond",
        LinkKind::Bridge => "net.k.bridge",
        LinkKind::Vlan => "net.k.vlan",
        LinkKind::Loopback => "net.k.loopback",
        LinkKind::Wireless => "net.k.wireless",
        LinkKind::Virtual => "net.k.virtual",
        LinkKind::Other => "net.k.other",
    }
}

/// Why an interface is read-only, translated from its code; the server's English text
/// (`true`: mark it `lang="en"`) for codes this build does not know and older servers.
pub fn read_only_text(i: &NetInterface) -> (String, bool) {
    match &i.read_only {
        Some(r) if r.code != ReadOnlyCode::Other => (read_only_code_text(r), false),
        _ => (i.read_only_reason.clone().unwrap_or_default(), true),
    }
}

fn read_only_code_text(r: &ReadOnlyReason) -> String {
    match r.code {
        ReadOnlyCode::Loopback => t("net.ro.loopback").to_string(),
        ReadOnlyCode::Enslaved => tf("net.ro.enslaved", &[("master", r.master.as_deref().unwrap_or("?"))]),
        ReadOnlyCode::NotEthernet => t("net.ro.not_ethernet").to_string(),
        ReadOnlyCode::Ifupdown => t("net.ro.ifupdown").to_string(),
        ReadOnlyCode::NetworkManager => t("net.ro.network_manager").to_string(),
        ReadOnlyCode::NoNetworkd => t("net.ro.no_networkd").to_string(),
        ReadOnlyCode::Other => String::new(),
    }
}

pub fn backend_key(b: NetBackend) -> &'static str {
    match b {
        NetBackend::Networkd => "net.b.networkd",
        NetBackend::Ifupdown => "net.b.ifupdown",
        NetBackend::NetworkManager => "net.b.nm",
        NetBackend::None => "net.b.none",
    }
}

pub fn method_key(m: Ipv4Method) -> &'static str {
    match m {
        Ipv4Method::Dhcp => "net.m.dhcp",
        Ipv4Method::Static => "net.m.static",
        Ipv4Method::Disabled => "net.m.disabled",
    }
}

/// Link state as tone and text.
pub fn link_state(i: &NetInterface) -> (Tone, &'static str) {
    match (i.carrier, i.oper_state.as_str()) {
        (Some(true), "up") | (None, "up") => (Tone::Ok, "net.st_up"),
        (Some(true), _) => (Tone::Warn, "net.st_dormant"),
        (Some(false), _) => (Tone::Unknown, "net.st_no_cable"),
        (None, "down") => (Tone::Unknown, "net.st_down"),
        _ => (Tone::Unknown, "net.st_unknown"),
    }
}

pub fn speed_text(i: &NetInterface) -> String {
    match (i.speed_mbps, i.duplex) {
        (Some(m), Some(Duplex::Half)) => format!("{} · {}", i18n::link_speed(m), t("net.half_duplex")),
        (Some(m), _) => i18n::link_speed(m),
        _ => "—".into(),
    }
}

/// Interfaces worth listing: no loopback; virtual links (veth, docker) only on request.
fn listed(i: &NetInterface, virt: bool) -> bool {
    i.kind != LinkKind::Loopback && (virt || i.kind != LinkKind::Virtual)
}

fn interfaces_panel(net: Net) -> AnyView {
    let virt = RwSignal::new(false);
    let open = RwSignal::new(None::<NetInterface>);
    let table = move || {
        if let Some(v) = net.ifaces.gate(5) {
            return v;
        }
        let show_virt = virt.get();
        let rows: Vec<NetInterface> = net.ifaces.data.with(|l| l.iter().flatten().filter(|i| listed(i, show_virt)).cloned().collect());
        if rows.is_empty() {
            return view! {
                <EmptyState icon=icons::NETWORK title=Signal::derive(|| t("n.none").to_string()) text=Signal::derive(|| t("net.none_text").to_string()) />
            }
            .into_any();
        }
        crate::ui::table(
            "net-table",
            &["net.col_iface", "col.status", "net.col_speed", "net.col_addresses", "net.col_config", ""],
            rows.into_iter().map(|i| iface_row(i, open)).collect(),
        )
    };
    let hidden = move || net.ifaces.data.with(|l| l.iter().flatten().filter(|i| i.kind == LinkKind::Virtual).count());
    let editor = move || open.get().map(|i| edit::editor(net, i, Callback::new(move |_| open.set(None))));
    view! {
        <div class="toolbar" hidden=move || hidden() == 0>
            {switch("net.show_virtual", None, virt.into(), Callback::new(move |v| virt.set(v)))}
            <span class="muted small net-count">{move || (!virt.get()).then(|| tf("net.hidden_virtual", &[("n", &hidden().to_string())]))}</span>
        </div>
        {table}
        {editor}
    }
    .into_any()
}

fn iface_row(i: NetInterface, open: RwSignal<Option<NetInterface>>) -> AnyView {
    use crate::ui::{cell, row};
    let (tone, key) = link_state(&i);
    let addrs: Vec<AnyView> = i
        .addresses
        .iter()
        .filter(|a| a.scope != "host")
        .map(|a| {
            let fam = if a.family == IpFamily::Ipv6 { "v6" } else { "v4" };
            let dynamic = a.dynamic.then(|| view! { <span class="chip">{tr("net.dynamic")}</span> });
            view! { <span class="net-addr"><span class=format!("net-fam {fam}")>{fam}</span><span class="mono">{format!("{}/{}", a.address, a.prefix_len)}</span>{dynamic}</span> }.into_any()
        })
        .collect();
    // what configures it: the IPv4 method when known, else who owns the interface
    let (cfg_text, cfg_sub) = match (&i.pending, &i.applied) {
        (Some(c), _) | (None, Some(c)) => (t(method_key(c.ipv4)).to_string(), Some(t(backend_key(i.backend)))),
        (None, None) => (t(backend_key(i.backend)).to_string(), None),
    };
    let sub = [Some(t(kind_key(i.kind)).to_string()), i.driver.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ");
    let mac = i.mac.clone();
    let name = i.name.clone();
    let editable = i.editable;
    let label = if editable { tf("edit_named", &[("name", &name)]) } else { tf("net.details_title", &[("name", &name)]) };
    let label2 = label.clone();
    let i2 = i.clone();
    let i3 = i.clone();
    // the translated text first, then why (translated from its code)
    let reason = match read_only_text(&i).0 {
        r if r.is_empty() => t("net.read_only_text").to_string(),
        r => format!("{} {r}", t("net.read_only_text")),
    };
    let config = view! {
        <div class="cell-main">
            <span>{cfg_text}</span>
            {cfg_sub.map(|s| view! { <span class="cell-sub">{s}</span> })}
        </div>
        <div class="chips">
            {i.pending.is_some().then(|| ui::status(Tone::Info, t("pending.change").to_string()))}
            {(!editable).then(|| view! { <span class="chip" title=reason.clone()><Icon icon=icons::LOCK size=12 />{tr("net.read_only")}</span> })}
        </div>
    };
    let cells = vec![
        cell(
            "net.col_iface",
            "",
            view! {
                <div class="cell-main">
                    <button class="link-strong mono" aria-label=label on:click=move |_| open.set(Some(i2.clone()))>{name.clone()}</button>
                    <span class="cell-sub">{sub}</span>
                    {mac.map(|m| view! { <span class="cell-sub mono" title=tr("net.col_mac")>{m}</span> })}
                    {i.management.then(|| view! { <span class="chip accent" title=tr("net.this_connection_help")>{tr("net.this_connection")}</span> })}
                </div>
            }
            .into_any(),
        ),
        cell("col.status", "", ui::status(tone, t(key).to_string())),
        cell("net.col_speed", "tnum", speed_text(&i).into_any()),
        cell(
            "net.col_addresses",
            "",
            if addrs.is_empty() { view! { <span class="muted">{tr("net.no_address")}</span> }.into_any() } else { view! { <div class="net-addrs">{addrs}</div> }.into_any() },
        ),
        cell("net.col_config", "", config.into_any()),
        cell(
            "",
            "actions",
            view! {
                <button class="icon-btn sm" title=label2.clone() aria-label=label2 on:click=move |_| open.set(Some(i3.clone()))>
                    <Icon icon=if editable { icons::PENCIL } else { icons::EYE } size=16 />
                </button>
            }
            .into_any(),
        ),
    ];
    row(cells)
}

// ------------------------------------------------------------------ diff

/// One changed setting: i18n key of the field, old text, new text.
pub type DiffLine = (&'static str, String, String);

fn cfg_field(c: &InterfaceConfig, f: u8) -> String {
    match f {
        0 => t(method_key(c.ipv4)).to_string(),
        1 => c.addresses.join(", "),
        2 => c.gateway.clone().unwrap_or_default(),
        3 => c.dns.join(", "),
        _ => c.mtu.map(|m| m.to_string()).unwrap_or_else(|| t("net.mtu_default").to_string()),
    }
}

/// What changes from `old` (None: nothing configured by rynas) to `new`. Addresses and the
/// gateway of a non-static configuration are not compared (the server ignores them).
pub fn diff(old: Option<&InterfaceConfig>, new: &InterfaceConfig) -> Vec<DiffLine> {
    const KEYS: [&str; 5] = ["net.f_method", "net.f_addresses", "net.f_gateway", "net.dns", "net.f_mtu"];
    let base = InterfaceConfig::default();
    let o = old.unwrap_or(&base);
    let statics = o.ipv4 == Ipv4Method::Static || new.ipv4 == Ipv4Method::Static;
    (0u8..5)
        .filter(|f| statics || !matches!(f, 1 | 2))
        .filter_map(|f| {
            let (a, b) = (cfg_field(o, f), cfg_field(new, f));
            (a != b || (old.is_none() && f == 0)).then(|| (KEYS[f as usize], a, b))
        })
        .collect()
}

pub fn diff_table(old: Option<&InterfaceConfig>, new: &InterfaceConfig) -> AnyView {
    let lines = diff(old, new);
    if lines.is_empty() {
        return view! { <p class="muted small">{tr("net.no_changes")}</p> }.into_any();
    }
    let rows = lines
        .into_iter()
        .map(|(k, a, b)| {
            let a = if a.is_empty() { "—".to_string() } else { a };
            let b = if b.is_empty() { "—".to_string() } else { b };
            crate::ui::row(vec![
                crate::ui::cell("diff.field", "", tr(k).into_any()),
                crate::ui::cell(
                    "diff.value",
                    "",
                    view! { <div class="diff-val mono"><span class="diff-old">{a}</span><span class="diff-arrow">"→"</span><span class="diff-new">{b}</span></div> }.into_any(),
                ),
            ])
        })
        .collect();
    crate::ui::table("diff", &["diff.field", "diff.value"], rows)
}

// ------------------------------------------------------------------ validation (mirrors rynas_net::txn)

/// A host address with its prefix (`a.b.c.d/nn`, prefix 8–32): not the network or broadcast
/// address, not loopback, link-local, multicast or unspecified.
pub fn parse_host_cidr(s: &str) -> Option<(Ipv4Addr, u8)> {
    let (ip, p) = s.trim().split_once('/')?;
    let ip: Ipv4Addr = ip.parse().ok()?;
    let p: u8 = p.parse().ok()?;
    if !(8..=32).contains(&p) || ip.is_loopback() || ip.is_link_local() || ip.is_multicast() || ip.is_unspecified() || ip.is_broadcast() {
        return None;
    }
    if p <= 30 {
        let bits = u32::from(ip);
        let host = bits & (u32::MAX >> p);
        if host == 0 || host == u32::MAX >> p {
            return None;
        }
    }
    Some((ip, p))
}

pub fn same_subnet(a: Ipv4Addr, b: Ipv4Addr, prefix: u8) -> bool {
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix as u32) };
    u32::from(a) & mask == u32::from(b) & mask
}

/// A unicast DNS server (IPv4 or IPv6).
pub fn valid_dns(s: &str) -> bool {
    match s.trim().parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v)) => !(v.is_multicast() || v.is_unspecified() || v.is_broadcast()),
        Ok(std::net::IpAddr::V6(v)) => !(v.is_multicast() || v.is_unspecified()),
        Err(_) => false,
    }
}

/// Words of a list typed one per line or separated by commas/spaces.
pub fn words(s: &str) -> Vec<String> {
    s.split(|c: char| c == ',' || c.is_whitespace()).filter(|w| !w.is_empty()).map(str::to_string).collect()
}

/// Problems of a draft, as (field, i18n key); empty when it may be staged.
pub fn check(c: &InterfaceConfig, max_mtu: Option<u32>) -> Vec<(&'static str, &'static str)> {
    let mut v = Vec::new();
    if c.ipv4 == Ipv4Method::Static {
        let nets: Vec<(Ipv4Addr, u8)> = c.addresses.iter().filter_map(|a| parse_host_cidr(a)).collect();
        if c.addresses.is_empty() {
            v.push(("addresses", "net.v_addresses_none"));
        } else if nets.len() != c.addresses.len() {
            v.push(("addresses", "net.v_address"));
        }
        if let Some(g) = &c.gateway {
            match g.trim().parse::<Ipv4Addr>() {
                Ok(gw) if nets.iter().any(|(ip, p)| same_subnet(*ip, gw, *p) && *ip != gw) => {}
                Ok(_) => v.push(("gateway", "net.v_gateway_subnet")),
                Err(_) => v.push(("gateway", "net.v_gateway")),
            }
        }
    }
    if c.dns.iter().any(|d| !valid_dns(d)) {
        v.push(("dns", "net.v_dns"));
    }
    if let Some(m) = c.mtu
        && (m < 576 || m > max_mtu.unwrap_or(65_535))
    {
        v.push(("mtu", "net.v_mtu"));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(addrs: &[&str], gw: Option<&str>) -> InterfaceConfig {
        InterfaceConfig {
            ipv4: Ipv4Method::Static,
            addresses: addrs.iter().map(|s| s.to_string()).collect(),
            gateway: gw.map(str::to_string),
            dns: vec![],
            mtu: None,
        }
    }

    #[test]
    fn host_addresses() {
        assert!(parse_host_cidr("192.0.2.10/24").is_some());
        assert!(parse_host_cidr("192.0.2.0/24").is_none());
        assert!(parse_host_cidr("192.0.2.255/24").is_none());
        assert!(parse_host_cidr("192.0.2.10/33").is_none());
        assert!(parse_host_cidr("192.0.2.10/7").is_none());
        assert!(parse_host_cidr("127.0.0.2/8").is_none());
        assert!(parse_host_cidr("169.254.1.1/16").is_none());
        assert!(parse_host_cidr("224.0.0.1/24").is_none());
        assert!(parse_host_cidr("192.0.2.10").is_none());
        assert!(parse_host_cidr("198.51.100.1/32").is_some());
        assert!(parse_host_cidr("198.51.100.0/31").is_some());
    }

    #[test]
    fn checks() {
        assert!(check(&st(&["192.0.2.10/24"], Some("192.0.2.1")), None).is_empty());
        assert_eq!(check(&st(&["192.0.2.10/24"], Some("198.51.100.1")), None), vec![("gateway", "net.v_gateway_subnet")]);
        assert_eq!(check(&st(&["192.0.2.10/24"], Some("192.0.2.10")), None), vec![("gateway", "net.v_gateway_subnet")]);
        assert_eq!(check(&st(&[], None), None), vec![("addresses", "net.v_addresses_none")]);
        assert_eq!(check(&st(&["192.0.2.10"], None), None), vec![("addresses", "net.v_address")]);
        let mut c = st(&["192.0.2.10/24"], None);
        c.dns = vec!["192.0.2.53".into(), "2001:db8::53".into()];
        c.mtu = Some(9000);
        assert!(check(&c, Some(9216)).is_empty());
        assert_eq!(check(&c, Some(1500)), vec![("mtu", "net.v_mtu")]);
        c.dns.push("dns.example".into());
        assert_eq!(check(&c, Some(9216)), vec![("dns", "net.v_dns")]);
        // DHCP ignores addresses and gateway
        let d = InterfaceConfig { ipv4: Ipv4Method::Dhcp, addresses: vec!["bad".into()], ..Default::default() };
        assert!(check(&d, None).is_empty());
    }

    #[test]
    fn diffs() {
        let a = st(&["192.0.2.10/24"], Some("192.0.2.1"));
        let mut b = a.clone();
        assert!(diff(Some(&a), &b).is_empty());
        b.addresses = vec!["192.0.2.20/24".into()];
        b.mtu = Some(9000);
        let d = diff(Some(&a), &b);
        assert_eq!(d.iter().map(|l| l.0).collect::<Vec<_>>(), vec!["net.f_addresses", "net.f_mtu"]);
        assert_eq!(d[0].1, "192.0.2.10/24");
        assert_eq!(d[0].2, "192.0.2.20/24");
        // DHCP to DHCP with leftover addresses: nothing to show
        let dh = InterfaceConfig::default();
        let dh2 = InterfaceConfig { addresses: vec!["192.0.2.9/24".into()], ..Default::default() };
        assert!(diff(Some(&dh), &dh2).is_empty());
    }

    #[test]
    fn word_lists() {
        assert_eq!(words("192.0.2.1, 192.0.2.2\n2001:db8::1 "), vec!["192.0.2.1", "192.0.2.2", "2001:db8::1"]);
        assert!(words(" \n").is_empty());
    }
}

#[cfg(test)]
mod read_only_tests {
    use super::*;

    #[test]
    fn read_only_reason_is_translated_from_its_code() {
        let mut i = NetInterface {
            read_only_reason: Some("enslaved to bond0: bonds and bridges are configured in a later version".into()),
            read_only: Some(ReadOnlyReason { code: ReadOnlyCode::Enslaved, master: Some("bond0".into()) }),
            ..Default::default()
        };
        assert_eq!(read_only_text(&i), ("It is a port of bond0: bonds and bridges become configurable in a later version.".into(), false));
        // a code of a newer server, and an older server without codes: the English text
        i.read_only = Some(ReadOnlyReason { code: ReadOnlyCode::Other, master: None });
        assert!(read_only_text(&i).1);
        i.read_only = None;
        assert_eq!(read_only_text(&i).0, i.read_only_reason.clone().unwrap());
    }
}
