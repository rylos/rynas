//! Tabs of the Network page besides the interfaces: revisions with restore, rescue paths and
//! diagnostics (ping, traceroute, DNS lookup).

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::network::{
    DiagnosticResult, DnsLookupRequest, LinkKind, NetApplyStarted, NetRestoreRequest, NetRevision, PingRequest, RescueSettings,
    TracerouteRequest,
};
use serde_json::Value;

use super::{Net, backend_key, timeout_select};
use crate::api::{self, ApiErr};
use crate::form::{In, switch, text_field};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::toast::saved_pending;
use crate::ui::{self, Confirm, EmptyState, Tone, cell, confirm, row, table};

// ------------------------------------------------------------------ revisions

fn rev_kind_key(kind: &str) -> &'static str {
    match kind {
        "snapshot" => "net.rk.snapshot",
        "apply" => "net.rk.apply",
        "adopt" => "net.rk.adopt",
        "rescue" => "net.rk.rescue",
        "restore" => "net.rk.restore",
        _ => "net.rk.other",
    }
}

pub fn revisions_panel(net: Net) -> AnyView {
    let restore = RwSignal::new(None::<NetRevision>);
    let list = move || {
        if let Some(v) = net.revs.gate(5) {
            return v;
        }
        let revs = net.revs.data.get().unwrap_or_default();
        if revs.is_empty() {
            return view! {
                <EmptyState icon=icons::HISTORY title=Signal::derive(|| t("net.revs_empty").to_string()) text=Signal::derive(|| t("net.revs_empty_text").to_string()) />
            }
            .into_any();
        }
        let rows = revs.into_iter().map(|r| rev_row(net, r, restore)).collect();
        table(
            "net-revs",
            &["#cfg.col_rev", "cfg.col_date", "net.col_kind", "cfg.col_author", "net.col_ifaces", "col.status", "cfg.col_comment", ""],
            rows,
        )
    };
    view! {
        <p class="muted small net-tab-intro">{tr("net.revs_intro")}</p>
        {list}
        {move || restore.get().map(|r| restore_dialog(net, r, Callback::new(move |_| restore.set(None))))}
    }
    .into_any()
}

fn rev_row(net: Net, r: NetRevision, restore: RwSignal<Option<NetRevision>>) -> AnyView {
    let state = if r.lkg {
        ui::status(Tone::Ok, t("net.rev_lkg").to_string())
    } else if r.good {
        ui::status(Tone::Ok, t("net.rev_good").to_string())
    } else {
        ui::status(Tone::Unknown, t("net.rev_unconfirmed").to_string())
    };
    let label = tf("net.restore_rev", &[("rev", &r.id.to_string())]);
    let r2 = r.clone();
    let ifaces = if r.interfaces.is_empty() { ui::DASH.to_string() } else { r.interfaces.join(", ") };
    row(vec![
        cell("cfg.col_rev", "num tnum", format!("#{}", r.id).into_any()),
        cell("cfg.col_date", "", i18n::datetime(r.ts_ms).into_any()),
        cell(
            "net.col_kind",
            "",
            view! { <div class="cell-main"><span>{tr(rev_kind_key(&r.kind))}</span><span class="cell-sub">{tr(backend_key(r.backend))}</span></div> }.into_any(),
        ),
        cell("cfg.col_author", "", r.author.clone().into_any()),
        cell("net.col_ifaces", "mono", ifaces.into_any()),
        cell("col.status", "", state),
        cell("cfg.col_comment", "muted", r.comment.clone().into_any()),
        cell(
            "",
            "actions",
            view! {
                <button class="btn ghost sm" aria-label=label disabled=move || net.trial_running() on:click=move |_| restore.set(Some(r2.clone()))>
                    <Icon icon=icons::ROTATE_CCW size=14 />{tr("cfg.restore")}
                </button>
            }
            .into_any(),
        ),
    ])
}

fn restore_dialog(net: Net, r: NetRevision, close: Callback<()>) -> AnyView {
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let timeout = RwSignal::new(net.state.data.with_untracked(|s| s.as_ref().map(|s| s.timeout_default_s).unwrap_or(120)));
    let id = r.id;
    let on_confirm = Callback::new(move |_| {
        busy.set(true);
        error.set(None);
        let body = NetRestoreRequest { timeout_s: Some(timeout.get_untracked()) };
        spawn_local(async move {
            let res = api::post::<_, NetApplyStarted>(&format!("/api/v1/network/revisions/{id}/restore"), Some(&body)).await;
            busy.set(false);
            match res {
                Ok(s) => {
                    close.run(());
                    net.trial.begin(net, s);
                    net.reload();
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    });
    confirm(Confirm {
        title: tf("net.restore_rev", &[("rev", &id.to_string())]),
        text: tf("net.restore_text", &[("date", &i18n::datetime(r.ts_ms))]),
        action: "cfg.restore",
        danger: false,
        extra: Some(timeout_select(net, timeout)),
        typed: Signal::stored(None),
        busy,
        error,
        on_confirm,
        on_cancel: close,
    })
}

// ------------------------------------------------------------------ rescue

pub fn rescue_panel(net: Net) -> AnyView {
    let busy = RwSignal::new(false);
    let set_mdns = Callback::new(move |on: bool| {
        busy.set(true);
        spawn_local(async move {
            let r = api::put::<_, rynas_api_types::network::RescueInfo>("/api/v1/network/rescue", &RescueSettings { mdns_service: on }).await;
            busy.set(false);
            match r {
                Ok(_) => {
                    saved_pending();
                    crate::config::changed();
                }
                Err(e) => crate::toast::toast(Tone::Err, e.text()),
            }
        });
    });
    let body = move || {
        if let Some(v) = net.rescue.gate(6) {
            return v;
        }
        let r = net.rescue.data.get().unwrap_or_default();
        let eff = r.pending.clone().unwrap_or(r.settings.clone());
        let port = window().location().port().unwrap_or_default();
        let scheme = window().location().protocol().unwrap_or_else(|_| "http:".into());
        let url_of = move |host: &str| if port.is_empty() { format!("{scheme}//{host}/") } else { format!("{scheme}//{host}:{port}/") };
        let mdns_url = url_of(&r.mdns_name);
        let ll_rows: Vec<AnyView> = r
            .link_local
            .iter()
            .map(|l| {
                row(vec![
                    cell("net.col_iface", "", l.interface.clone().into_any()),
                    cell("net.col_mac", "muted", ui::breakable(&l.mac, "mono")),
                    // a link-local URL (`http://[fe80::…%enp5s0]:8443/`) is long: it wraps (G7)
                    cell("net.link_local", "wrap", ui::breakable(&url_of(&l.address), "mono")),
                    cell(
                        "col.status",
                        "",
                        if l.active { ui::status(Tone::Ok, t("net.ll_active").to_string()) } else { ui::status(Tone::Unknown, t("net.ll_inactive").to_string()) },
                    ),
                ])
            })
            .collect();
        // the reset button is a hardware module's feature: the section exists only where the
        // hardware has it
        let reset = r.reset_button.as_ref().and_then(crate::modules::reset_button);
        view! {
            <div class="net-rescue">
                <section class="net-rescue-sec">
                    <h3 class="form-section"><Icon icon=icons::LIFE_BUOY size=16 />{tr("net.mdns")}</h3>
                    <div class="chips">
                        {ui::breakable(&mdns_url, "mono strong")}
                        {if r.mdns_active { ui::status(Tone::Ok, t("net.avahi_on").to_string()) } else { ui::status(Tone::Warn, t("net.avahi_off").to_string()) }}
                        {r.pending.is_some().then(|| ui::status(Tone::Info, t("pending.change").to_string()))}
                    </div>
                    <p class="muted small">{tr("net.mdns_text")}</p>
                    <fieldset class="fieldset" disabled=move || busy.get()>
                        {switch("net.mdns_service", Some("net.mdns_service_help"), Signal::stored(eff.mdns_service), set_mdns)}
                    </fieldset>
                </section>
                <section class="net-rescue-sec">
                    <h3 class="form-section"><Icon icon=icons::NETWORK size=16 />{tr("net.ll_title")}</h3>
                    <p class="muted small">{tr("net.ll_text")}</p>
                    {if ll_rows.is_empty() {
                        ui::empty_line("net.ll_none")
                    } else {
                        table("net-ll", &["net.col_iface", "net.col_mac", "net.link_local", "col.status"], ll_rows)
                    }}
                </section>
                {reset.map(|reset| view! {
                    <section class="net-rescue-sec">
                        <h3 class="form-section"><Icon icon=icons::ROTATE_CCW size=16 />{tr("reset_button")}</h3>
                        {reset}
                    </section>
                })}
            </div>
        }
        .into_any()
    };
    view! {
        <p class="muted small net-tab-intro">{tr("net.rescue_intro")}</p>
        {body}
    }
    .into_any()
}

// ------------------------------------------------------------------ diagnostics

const RECORDS: &[&str] = &["A", "AAAA", "MX", "TXT", "CNAME", "NS", "PTR", "SRV"];

/// Host names and addresses the server accepts: no spaces, no leading `-`.
fn valid_host(h: &str) -> bool {
    let h = h.trim();
    !h.is_empty() && h.len() <= 253 && !h.starts_with('-') && !h.contains(char::is_whitespace)
}

pub fn diagnostics_panel(net: Net) -> AnyView {
    let tool = RwSignal::new("ping");
    let host = RwSignal::new(String::new());
    let count = RwSignal::new("4".to_string());
    let size = RwSignal::new(String::new());
    let iface = RwSignal::new(String::new());
    let df = RwSignal::new(false);
    let record = RwSignal::new("A".to_string());
    let server = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let result = RwSignal::new(None::<DiagnosticResult>);

    let count_ok = move || count.with(|c| c.trim().parse::<u8>().is_ok_and(|n| (1..=10).contains(&n)));
    let size_ok = move || size.with(|s| s.trim().is_empty() || s.trim().parse::<u32>().is_ok_and(|n| n <= 65_000));
    let server_ok = move || server.with(|s| s.trim().is_empty() || valid_host(s));
    let ready = move || valid_host(&host.get()) && (tool.get() != "ping" || (count_ok() && size_ok())) && (tool.get() != "dns" || server_ok());

    let run = move || {
        if !ready() || busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        let h = host.get_untracked().trim().to_string();
        let which = tool.get_untracked();
        let body = match which {
            "ping" => serde_json::to_string(&PingRequest {
                host: h,
                count: count.get_untracked().trim().parse().ok(),
                interface: Some(iface.get_untracked()).filter(|s| !s.is_empty()),
                size: size.get_untracked().trim().parse().ok(),
                dont_fragment: df.get_untracked(),
            }),
            "traceroute" => serde_json::to_string(&TracerouteRequest { host: h }),
            _ => serde_json::to_string(&DnsLookupRequest {
                name: h,
                record_type: Some(record.get_untracked()),
                server: Some(server.get_untracked().trim().to_string()).filter(|s| !s.is_empty()),
            }),
        }
        .unwrap_or_default();
        let value: Value = crate::api::decode(&body).unwrap_or(Value::Null);
        spawn_local(async move {
            let r = api::post::<_, DiagnosticResult>(&format!("/api/v1/network/diagnostics/{which}"), Some(&value)).await;
            busy.set(false);
            match r {
                Ok(d) => result.set(Some(d)),
                Err(e @ ApiErr::Http { .. }) if e.status() == 429 => {
                    error.set(Some(tf("net.rate_limited", &[("n", &e.retry_after().unwrap_or(60).to_string())])))
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    };

    let seg = move |id: &'static str, key: &'static str| {
        view! {
            <button type="button" class:on=move || tool.get() == id aria-pressed=move || (tool.get() == id).to_string() on:click=move |_| { tool.set(id); result.set(None); error.set(None); }>
                {tr(key)}
            </button>
        }
    };
    let none = Signal::derive(|| None::<String>);
    let host_label = move || if tool.get() == "dns" { "net.d_name" } else { "net.d_host" };
    let iface_names = move || {
        net.ifaces.data.with(|l| l.iter().flatten().filter(|i| i.kind != LinkKind::Loopback && i.kind != LinkKind::Virtual).map(|i| i.name.clone()).collect::<Vec<_>>())
    };
    let host_err = Signal::derive(move || {
        let h = host.get();
        (!h.is_empty() && !valid_host(&h)).then(|| t("net.v_host").to_string())
    });
    let iid = crate::form::fid();
    let rid = crate::form::fid();

    let output = move || {
        result.get().map(|d| {
            let tone = if d.ok { Tone::Ok } else { Tone::Err };
            let status = if d.ok { t("net.d_ok") } else { t("net.d_failed") };
            let meta = format!(
                "{} · {}{}",
                d.tool,
                i18n::elapsed(d.duration_ms / 1000).replace(" 0s", ""),
                d.exit_code.map(|c| format!(" · {} {c}", t("net.d_exit"))).unwrap_or_default()
            );
            let hops = d.report.as_ref().and_then(mtr_hops);
            view! {
                <div class="net-out">
                    <div class="net-out-head">{ui::status(tone, status.to_string())}<span class="muted small mono">{meta}</span></div>
                    {hops}
                    <pre class="log net-log" tabindex="0" role="log" aria-label=tr("net.d_output")>{d.output.clone()}</pre>
                </div>
            }
        })
    };

    view! {
        <p class="muted small net-tab-intro">{tr("net.tools_intro")}</p>
        <form class="form net-tools" on:submit=move |ev| { ev.prevent_default(); run(); }>
            <div class="segmented" role="group" aria-label=tr("net.d_tool")>
                {seg("ping", "net.d_ping")}
                {seg("traceroute", "net.d_trace")}
                {seg("dns", "net.d_dns")}
            </div>
            <div class="form-row">
                {move || text_field(host_label(), host.into(), Callback::new(move |v| host.set(v)), host_err, In { mono: true, placeholder: "example.org", ..In::default() })}
                <div hidden=move || tool.get() != "dns" class="field">
                    <label class="field-label" for=rid.clone()>{tr("net.d_record")}</label>
                    <select id=rid class="input" on:change=move |ev| record.set(event_target_value(&ev))>
                        {RECORDS.iter().map(|r| view! { <option value=*r selected=move || record.with(|x| x == r)>{*r}</option> }).collect_view()}
                    </select>
                </div>
            </div>
            <div class="form-row three" hidden=move || tool.get() != "ping">
                {text_field("net.d_count", count.into(), Callback::new(move |v| count.set(v)), Signal::derive(move || (!count_ok()).then(|| t("net.v_count").to_string())), In { kind: "number", mono: true, ..In::default() })}
                {text_field("net.d_size", size.into(), Callback::new(move |v| size.set(v)), Signal::derive(move || (!size_ok()).then(|| t("net.v_size").to_string())), In { kind: "number", mono: true, placeholder: "56", ..In::default() })}
                <div class="field">
                    <label class="field-label" for=iid.clone()>{tr("net.d_iface")}</label>
                    <select id=iid class="input" on:change=move |ev| iface.set(event_target_value(&ev))>
                        <option value="" selected=move || iface.with(String::is_empty)>{tr("net.d_any")}</option>
                        {move || iface_names().into_iter().map(|n| {
                            let (n2, n3) = (n.clone(), n.clone());
                            view! { <option value=n3 selected=move || iface.with(|x| *x == n2)>{n}</option> }
                        }).collect_view()}
                    </select>
                </div>
            </div>
            <div hidden=move || tool.get() != "ping">
                {switch("net.d_df", Some("net.d_df_help"), df.into(), Callback::new(move |v| df.set(v)))}
            </div>
            <div hidden=move || tool.get() != "dns">
                {text_field("net.d_server", server.into(), Callback::new(move |v| server.set(v)), none, In { mono: true, placeholder: "192.0.2.53", help: Some("net.d_server_help"), ..In::default() })}
            </div>
            {crate::form::form_error(error)}
            <div class="sec-actions">
                <button type="submit" class="btn primary" disabled=move || busy.get() || !ready()>
                    {crate::form::spinner(busy)}<Icon icon=icons::PLAY size=16 />{tr("net.d_run")}
                </button>
            </div>
        </form>
        <div aria-live="polite">{output}</div>
    }
    .into_any()
}

/// Hops of `mtr --json` (`report.hubs[]`), when the server ran mtr.
fn mtr_hops(report: &Value) -> Option<AnyView> {
    let hubs = report.pointer("/report/hubs")?.as_array()?;
    if hubs.is_empty() {
        return None;
    }
    let f = |h: &Value, k: &str| h.get(k).and_then(Value::as_f64);
    let rows = hubs
        .iter()
        .map(|h| {
            let n = h.get("count").map(|c| c.to_string()).unwrap_or_default();
            let host = h.get("host").and_then(Value::as_str).unwrap_or("???").to_string();
            let ms = |k: &str| f(h, k).map(|v| i18n::num(v, 1)).unwrap_or_default();
            row(vec![
                cell("net.h_hop", "num tnum", n.trim_matches('"').to_string().into_any()),
                cell("net.h_host", "mono", host.into_any()),
                cell("net.h_loss", "num tnum", f(h, "Loss%").map(i18n::pct).unwrap_or_default().into_any()),
                cell("net.h_avg", "num tnum", ms("Avg").into_any()),
                cell("net.h_best", "num tnum", ms("Best").into_any()),
                cell("net.h_worst", "num tnum", ms("Wrst").into_any()),
            ])
        })
        .collect();
    Some(table("net-hops", &["#net.h_hop", "net.h_host", "#net.h_loss", "#net.h_avg", "#net.h_best", "#net.h_worst"], rows))
}
