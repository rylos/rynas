//! Side drawer of one interface: live details, the read-only ifupdown view, and — when the
//! interface may be edited — the IPv4 form whose Save stages the change (nothing touches the
//! network until Apply on the page starts the timed test).

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::network::{BondConfig, InterfaceConfig, IpFamily, Ipv4Method, LinkKind, NetBackend, NetInterface};

use super::{Net, bond, backend_key, read_only_text, check, diff, diff_table, link_state, method_key, speed_text, words};
use crate::api;
use crate::form::{In, area_field, save_bar, select_field, text_field};
use crate::ui::{self, drawer};
use crate::i18n::{t, tf, tr};
use crate::icons::{self, Icon};
use crate::toast::toast;
use crate::ui::Tone;

const METHODS: &[(&str, &str)] = &[("dhcp", "net.m.dhcp"), ("static", "net.m.static"), ("disabled", "net.m.disabled")];

fn method_value(m: Ipv4Method) -> &'static str {
    match m {
        Ipv4Method::Dhcp => "dhcp",
        Ipv4Method::Static => "static",
        Ipv4Method::Disabled => "disabled",
    }
}

fn kv(label: &'static str, value: String, class: &'static str) -> AnyView {
    view! { <div class="kv"><dt>{tr(label)}</dt><dd class=class>{value}</dd></div> }.into_any()
}

/// The draft as the user typed it (lists stay text until saved, so a trailing newline or comma
/// is not eaten while typing).
#[derive(Clone)]
struct Draft {
    method: RwSignal<String>,
    addresses: RwSignal<String>,
    gateway: RwSignal<String>,
    dns: RwSignal<String>,
    mtu: RwSignal<String>,
}

impl Draft {
    fn new(c: &InterfaceConfig) -> Self {
        Draft {
            method: RwSignal::new(method_value(c.ipv4).to_string()),
            addresses: RwSignal::new(c.addresses.join("\n")),
            gateway: RwSignal::new(c.gateway.clone().unwrap_or_default()),
            dns: RwSignal::new(c.dns.join("\n")),
            mtu: RwSignal::new(c.mtu.map(|m| m.to_string()).unwrap_or_default()),
        }
    }

    /// `None` when the MTU is not a number.
    fn config(&self) -> Option<InterfaceConfig> {
        let ipv4 = match self.method.get().as_str() {
            "static" => Ipv4Method::Static,
            "disabled" => Ipv4Method::Disabled,
            _ => Ipv4Method::Dhcp,
        };
        let st = ipv4 == Ipv4Method::Static;
        let mtu = self.mtu.get();
        let mtu = match mtu.trim() {
            "" => None,
            m => Some(m.parse::<u32>().ok()?),
        };
        let gw = self.gateway.get().trim().to_string();
        Some(InterfaceConfig {
            ipv4,
            addresses: if st { words(&self.addresses.get()) } else { Vec::new() },
            gateway: (st && !gw.is_empty()).then_some(gw),
            dns: if ipv4 == Ipv4Method::Disabled { Vec::new() } else { words(&self.dns.get()) },
            mtu,
            ..Default::default()
        })
    }
}

pub fn editor(net: Net, i: NetInterface, close: Callback<()>) -> AnyView {
    editor_with(net, i, None, close)
}

/// The editor of a new bond: the port this session comes through (else the first free port)
/// as member, with its IPv4 settings, so the address does not change.
pub fn create_bond(net: Net, close: Callback<()>) -> AnyView {
    let all: Vec<NetInterface> = net.ifaces.data.with_untracked(|l| l.iter().flatten().cloned().collect());
    let name = bond::next_name(&all);
    let ports = bond::candidates(&all, &name);
    let seed = ports.iter().find(|p| p.management).or(ports.first());
    let mut base = seed.and_then(|p| p.pending.clone().or_else(|| p.applied.clone())).unwrap_or_default();
    base.bond = Some(BondConfig { members: seed.map(|p| vec![p.name.clone()]).unwrap_or_default(), miimon_ms: 100, ..Default::default() });
    let i = NetInterface { name, kind: LinkKind::Bond, editable: true, backend: NetBackend::Networkd, ..Default::default() };
    editor_with(net, i, Some(base), close)
}

fn editor_with(net: Net, i: NetInterface, seed: Option<InterfaceConfig>, close: Callback<()>) -> AnyView {
    let creating = seed.is_some();
    let title = if creating {
        tf("net.bond_create_title", &[("name", &i.name)])
    } else if i.editable {
        tf("edit_named", &[("name", &i.name)])
    } else {
        tf("net.details_title", &[("name", &i.name)])
    };
    let details = details(&i);
    if !i.editable {
        let (reason, en) = read_only_text(&i);
        let body = view! {
            <div class="form">
                {ui::note(Tone::Info, icons::LOCK, view! { {tr("net.read_only_text")}" "<span class="muted" lang=en.then_some("en")>{reason}</span> }.into_any())}
                {details}
                {phase_note()}
            </div>
        }
        .into_any();
        let foot = view! { <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("close")}</button> }.into_any();
        return drawer(Signal::stored(title), close, body, foot);
    }

    let base = seed.clone().or_else(|| i.pending.clone().filter(|p| !p.removed)).or_else(|| i.applied.clone()).unwrap_or_default();
    let d = Draft::new(&base);
    let show = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let applied = i.applied.clone();
    let has_pending = i.pending.is_some();
    let removing = i.pending.as_ref().is_some_and(|p| p.removed);
    let name = i.name.clone();
    // a bond: its members, mode... (a bond rynas made, or one being created)
    let bd = base.bond.as_ref().map(bond::BondDraft::new);
    let ports = bd
        .map(|_| net.ifaces.data.with_untracked(|l| bond::candidates(l.as_deref().unwrap_or_default(), &name)))
        .unwrap_or_default();
    // a bond's MTU: what every member accepts (the server checks it too)
    let max_mtu = if bd.is_some() { ports.iter().filter_map(|p| p.max_mtu).min() } else { i.max_mtu };
    let existing_bond = applied.as_ref().is_some_and(|a| a.bond.is_some());

    let d1 = d.clone();
    let cfg = Memo::new(move |_| {
        d1.config().map(|mut c| {
            c.bond = bd.map(|b| b.config());
            c
        })
    });
    let problems = Memo::new(move |_| match cfg.get() {
        Some(c) => {
            let mut v = check(&c, max_mtu);
            v.extend(c.bond.as_ref().map(bond::check).unwrap_or_default());
            v
        }
        None => vec![("mtu", "net.v_mtu")],
    });
    let err_of = move |field: &'static str| {
        Signal::derive(move || {
            if !show.get() && field != "mtu" && field != "gateway" {
                return None;
            }
            problems.with(|p| p.iter().find(|(f, _)| *f == field).map(|(_, k)| t(k).to_string()))
        })
    };
    let is_static = move || d.method.get() == "static";
    let base2 = base.clone();
    let changed = Signal::derive(move || creating || cfg.with(|c| c.as_ref().is_some_and(|c| *c != base2)));

    let stage = move |body: InterfaceConfig| {
        busy.set(true);
        error.set(None);
        let url = format!("/api/v1/network/interfaces/{}", api::encode(&name));
        spawn_local(async move {
            let r = api::put::<_, NetInterface>(&url, &body).await;
            busy.set(false);
            match r {
                Ok(_) => {
                    toast(Tone::Ok, t("net.staged").to_string());
                    net.reload();
                    close.run(());
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    };
    let stage2 = stage.clone();
    let save = Callback::new(move |_| {
        show.set(true);
        if !problems.with_untracked(Vec::is_empty) {
            error.set(Some(t("form.check").to_string()));
            return;
        }
        if let Some(c) = cfg.get_untracked() {
            stage(c);
        }
    });
    // DELETE stages the deletion of a bond, or drops a bond that is only staged
    let name2 = i.name.clone();
    let remove = Callback::new(move |toast_key: &'static str| {
        busy.set(true);
        error.set(None);
        let url = format!("/api/v1/network/interfaces/{}", api::encode(&name2));
        spawn_local(async move {
            let r = api::delete_empty(&url).await;
            busy.set(false);
            match r {
                Ok(()) => {
                    toast(Tone::Ok, t(toast_key).to_string());
                    net.reload();
                    close.run(());
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    });
    // staging the applied value drops the staged change (a new bond: DELETE)
    let revert = match applied.clone() {
        Some(a) => Some(Callback::new(move |_| stage2(a.clone()))),
        None if bd.is_some() && has_pending => Some(Callback::new(move |_| remove.run("net.bond_dropped"))),
        None => None,
    };

    let mgmt = i.management.then(|| {
        view! {
            {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tr("net.mgmt_edit")} }.into_any())}
        }
    });
    let applied2 = applied.clone();
    let preview = move || {
        cfg.get().map(|c| {
            let lines = diff(applied2.as_ref(), &c);
            (!lines.is_empty()).then(|| view! {
                <h3 class="form-section"><Icon icon=icons::FILE_DIFF size=16 />{tr("net.sec_diff")}</h3>
                {diff_table(applied2.as_ref(), &c)}
            })
        })
    };
    let mtu_help = match max_mtu {
        Some(m) => tf("net.mtu_help_max", &[("max", &m.to_string())]),
        None => t("net.mtu_help").to_string(),
    };

    let body = view! {
        <form class="form" on:submit=move |ev| { ev.prevent_default(); save.run(()); }>
            {mgmt}
            {(has_pending && !removing).then(|| view! {
                {ui::note(Tone::Info, icons::INFO, view! { {tr("net.has_pending")} }.into_any())}
            })}
            {removing.then(|| view! {
                {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tr("net.bond_removed_text")} }.into_any())}
            })}
            {bd.map(|b| view! {
                <h3 class="form-section">{tr("net.k.bond")}</h3>
                {bond::form(b, ports.clone(), show.into())}
            })}
            <h3 class="form-section">{tr("net.sec_ipv4")}</h3>
            {select_field("net.f_method", Some("net.method_help"), METHODS, d.method.into(), Callback::new(move |v| d.method.set(v)))}
            <div class="sub-form form" hidden=move || !is_static()>
                {area_field("net.f_addresses", d.addresses.into(), Callback::new(move |v| d.addresses.set(v)), Some("net.addresses_help"), "192.0.2.10/24")}
                <span class="field-error" role="alert">{move || err_of("addresses").get().map(|e| view! { <Icon icon=icons::CIRCLE_X size=12 />{e} })}</span>
                {text_field("net.f_gateway", d.gateway.into(), Callback::new(move |v| d.gateway.set(v)), err_of("gateway"), In { mono: true, placeholder: "192.0.2.1", help: Some("net.gateway_help"), ..In::default() })}
            </div>
            <div hidden=move || d.method.get() == "disabled">
                {area_field("net.dns", d.dns.into(), Callback::new(move |v| d.dns.set(v)), Some("net.dns_help"), "192.0.2.53")}
                <span class="field-error" role="alert">{move || err_of("dns").get().map(|e| view! { <Icon icon=icons::CIRCLE_X size=12 />{e} })}</span>
            </div>
            <div class="net-mtu">
                {text_field("net.f_mtu", d.mtu.into(), Callback::new(move |v| d.mtu.set(v)), err_of("mtu"), In { kind: "number", mono: true, placeholder: "1500", ..In::default() })}
                <span class="field-help">{mtu_help}</span>
            </div>
            {preview}
            <button type="submit" hidden=true></button>
            <h3 class="form-section">{tr("net.sec_details")}</h3>
            {details}
            {phase_note()}
        </form>
    }
    .into_any();

    let foot = view! {
        {(existing_bond && !removing).then(|| view! {
            <button type="button" class="btn ghost net-revert" disabled=move || busy.get() on:click=move |_| remove.run("net.bond_deleted")>
                <Icon icon=icons::TRASH_2 size=16 />{tr("net.bond_delete")}
            </button>
        })}
        {revert.filter(|_| has_pending).map(|cb| view! {
            <button type="button" class="btn ghost net-revert" disabled=move || busy.get() on:click=move |_| cb.run(())>
                <Icon icon=icons::ROTATE_CCW size=16 />{tr("net.revert")}
            </button>
        })}
        {save_bar(busy, changed, "net.stage", close, save, error)}
    }
    .into_any();
    drawer(Signal::stored(title), close, body, foot)
}

/// Live state, addresses, ifupdown stanzas.
fn details(i: &NetInterface) -> AnyView {
    let (tone, key) = link_state(i);
    let mtu = match i.max_mtu {
        Some(m) => format!("{} ({} {m})", i.mtu, t("net.max")),
        None => i.mtu.to_string(),
    };
    let addrs = i
        .addresses
        .iter()
        .map(|a| {
            let fam = if a.family == IpFamily::Ipv6 { "IPv6" } else { "IPv4" };
            let how = if a.dynamic { t("net.dynamic") } else { t("net.configured") };
            view! {
                <li class="net-addr">
                    <span class="mono">{format!("{}/{}", a.address, a.prefix_len)}</span>
                    <span class="muted small">{format!("{fam} · {} · {how}", a.scope)}</span>
                </li>
            }
        })
        .collect_view();
    let applied = i.applied.as_ref().map(|c| {
        let addrs = if c.ipv4 == Ipv4Method::Static { c.addresses.join(", ") } else { String::new() };
        view! {
            <h3 class="form-section">{tr("net.sec_applied")}</h3>
            <dl class="kvs">
                {kv("net.f_method", t(method_key(c.ipv4)).to_string(), "")}
                {(!addrs.is_empty()).then(|| kv("net.f_addresses", addrs.clone(), "mono"))}
                {c.gateway.clone().map(|g| kv("net.f_gateway", g, "mono"))}
                {(!c.dns.is_empty()).then(|| kv("net.dns", c.dns.join(", "), "mono"))}
                {kv("net.f_mtu", c.mtu.map(|m| m.to_string()).unwrap_or_else(|| t("net.mtu_default").to_string()), "mono")}
            </dl>
        }
    });
    // a bond now: mode, members with their link, the LACP partner
    let bond_state = i.bond.as_ref().map(|s| {
        let members = s
            .members
            .iter()
            .map(|m| {
                let state = if !m.link_up {
                    t("net.member_down")
                } else if m.active {
                    t("net.member_active")
                } else if s.active_member.is_some() {
                    t("net.member_standby")
                } else {
                    t("net.st_up")
                };
                let speed = m.speed_mbps.map(crate::i18n::link_speed);
                let failures = (m.link_failures > 0).then(|| tf("net.member_failures", &[("n", &m.link_failures.to_string())]));
                let sub = [Some(state.to_string()), speed, failures].into_iter().flatten().collect::<Vec<_>>().join(" · ");
                view! {
                    <li class="net-addr">
                        <span class="mono">{m.name.clone()}</span>
                        <span class="muted small">{sub}</span>
                    </li>
                }
            })
            .collect_view();
        let partner = s.lacp_ok.map(|_| match s.partner_mac.as_deref().filter(|m| *m != "00:00:00:00:00:00") {
            Some(m) => kv("net.lacp_partner", m.to_string(), "mono"),
            None => kv("net.lacp_partner", t("net.lacp_none").to_string(), ""),
        });
        view! {
            <h3 class="form-section">{tr("net.f_members")}</h3>
            <dl class="kvs">
                {s.mode.as_deref().map(|m| kv("net.f_bond_mode", t(bond::mode_key(m)).to_string(), ""))}
                {partner}
            </dl>
            <ul class="plain-list net-addr-list">{members}</ul>
        }
    });
    let stanzas = i
        .ifupdown
        .iter()
        .map(|s| {
            let mut rows = vec![kv("net.ifu_method", format!("{} {}", s.family, s.method), "mono")];
            if let Some(a) = &s.address {
                rows.push(kv("net.f_addresses", a.clone(), "mono"));
            }
            if let Some(g) = &s.gateway {
                rows.push(kv("net.f_gateway", g.clone(), "mono"));
            }
            if !s.dns.is_empty() {
                rows.push(kv("net.dns", s.dns.join(", "), "mono"));
            }
            if let Some(m) = s.mtu {
                rows.push(kv("net.f_mtu", m.to_string(), "mono"));
            }
            if !s.bond_slaves.is_empty() {
                rows.push(kv("net.ifu_bond", format!("{} {}", s.bond_slaves.join(", "), s.bond_mode.clone().unwrap_or_default()), "mono"));
            }
            if !s.bridge_ports.is_empty() {
                rows.push(kv("net.ifu_bridge", s.bridge_ports.join(", "), "mono"));
            }
            if let Some(v) = &s.vlan_raw_device {
                rows.push(kv("net.ifu_vlan", v.clone(), "mono"));
            }
            for o in &s.other {
                rows.push(view! { <div class="kv"><dt class="mono">{o.name.clone()}</dt><dd class="mono">{o.value.clone()}</dd></div> }.into_any());
            }
            rows.push(kv("net.ifu_file", s.file.clone(), "mono muted"));
            view! { <dl class="kvs">{rows}</dl> }
        })
        .collect_view();
    view! {
        <div class="drawer-sec">
            <dl class="kvs">
                <div class="kv"><dt>{tr("col.status")}</dt><dd>{ui::status(tone, t(key).to_string())}</dd></div>
                {kv("net.col_speed", speed_text(i), "tnum")}
                {kv("net.col_mac", i.mac.clone().unwrap_or_else(|| "—".into()), "mono")}
                {i.driver.clone().map(|d| kv("net.driver", d, "mono"))}
                {kv("net.f_mtu", mtu, "mono")}
                {kv("net.backend", t(backend_key(i.backend)).to_string(), "")}
                {i.config_file.clone().map(|f| kv("net.config_file", f, "mono muted"))}
                {i.link_local_v4.clone().map(|a| kv("net.link_local", a, "mono"))}
                {(!i.dns.is_empty()).then(|| kv("net.dns_live", i.dns.join(", "), "mono"))}
            </dl>
            <h3 class="form-section">{tr("net.col_addresses")}</h3>
            {if i.addresses.is_empty() {
                view! { <p class="muted small">{tr("net.no_address")}</p> }.into_any()
            } else {
                view! { <ul class="plain-list net-addr-list">{addrs}</ul> }.into_any()
            }}
            {bond_state}
            {applied}
            {(!i.ifupdown.is_empty()).then(|| view! { <h3 class="form-section">{tr("net.sec_ifupdown")}</h3>{stanzas} })}
        </div>
    }
    .into_any()
}

fn phase_note() -> AnyView {
    view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("net.phase_note")}</p> }.into_any()
}
