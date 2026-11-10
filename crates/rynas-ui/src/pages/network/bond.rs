//! Bonds on the Network page (D-BOND): the bond part of the interface editor — member ports,
//! the mode with what it needs from the switch, the preferred port, the transmit hash — and
//! the members' state in the interface list. The IPv4 part, the staging and the timed test are
//! the same as for a port.

use leptos::prelude::*;
use rynas_api_types::network::{BondConfig, BondHash, BondLacpRate, BondMode, LinkKind, NetInterface, ReadOnlyCode};

use super::speed_text;
use crate::form::select_field;
use crate::i18n::{t, tf, tr};
use crate::icons::{self, Icon};
use crate::ui::{self, Tone};

/// The modes offered for a bond, most useful first.
const MODES: &[(&str, &str)] = &[
    ("active-backup", "net.bm.ab"),
    ("balance-alb", "net.bm.alb"),
    ("802.3ad", "net.bm.lacp"),
    ("balance-xor", "net.bm.xor"),
    ("balance-tlb", "net.bm.tlb"),
];

/// The same with the modes a migrated bond may run (not offered for new bonds).
const MODES_ALL: &[(&str, &str)] = &[
    ("active-backup", "net.bm.ab"),
    ("balance-alb", "net.bm.alb"),
    ("802.3ad", "net.bm.lacp"),
    ("balance-xor", "net.bm.xor"),
    ("balance-tlb", "net.bm.tlb"),
    ("balance-rr", "net.bm.rr"),
    ("broadcast", "net.bm.bc"),
];

const HASHES: &[(&str, &str)] = &[("layer2+3", "net.bh.l23"), ("layer3+4", "net.bh.l34"), ("layer2", "net.bh.l2")];

/// What the mode needs from the switch.
fn requirement_key(mode: &str) -> &'static str {
    match mode {
        "active-backup" => "net.bq.ab",
        "balance-alb" => "net.bq.alb",
        "802.3ad" => "net.bq.lacp",
        "balance-xor" => "net.bq.xor",
        "balance-tlb" => "net.bq.tlb",
        _ => "net.bq.legacy",
    }
}

/// The mode's name for the list.
pub fn mode_key(mode: &str) -> &'static str {
    MODES_ALL.iter().find(|(v, _)| *v == mode).map(|(_, k)| *k).unwrap_or("net.k.bond")
}

/// The bond part of a draft.
#[derive(Clone, Copy)]
pub struct BondDraft {
    pub members: RwSignal<Vec<String>>,
    pub mode: RwSignal<String>,
    pub primary: RwSignal<String>,
    pub hash: RwSignal<String>,
    /// What the form does not show (MAC, link monitoring, delays) is kept as it was.
    base: StoredValue<BondConfig>,
}

impl BondDraft {
    pub fn new(b: &BondConfig) -> Self {
        BondDraft {
            members: RwSignal::new(b.members.clone()),
            mode: RwSignal::new(b.mode.kernel_name().to_string()),
            primary: RwSignal::new(b.primary.clone().unwrap_or_default()),
            hash: RwSignal::new(b.hash_policy.unwrap_or(BondHash::Layer23).kernel_name().to_string()),
            base: StoredValue::new(b.clone()),
        }
    }

    pub fn config(&self) -> BondConfig {
        let base = self.base.get_value();
        let mode = BondMode::parse(&self.mode.get()).unwrap_or_default();
        let mut members = self.members.get();
        members.sort();
        let primary = Some(self.primary.get()).filter(|p| mode.has_primary() && members.contains(p));
        BondConfig {
            hash_policy: if mode.has_hash() { BondHash::parse(&self.hash.get()) } else { None },
            lacp_rate: if mode == BondMode::Lacp { base.lacp_rate } else { BondLacpRate::Slow },
            min_links: base.min_links.filter(|n| (*n as usize) <= members.len()),
            miimon_ms: if base.miimon_ms == 0 { 100 } else { base.miimon_ms },
            members,
            mode,
            primary,
            ..base
        }
    }
}

/// Problems of a bond draft, as (field, i18n key).
pub fn check(b: &BondConfig) -> Vec<(&'static str, &'static str)> {
    if b.members.is_empty() { vec![("members", "net.v_bond_members")] } else { Vec::new() }
}

/// Ports that may join `bond`: physical Ethernet ports rynas may edit, or members of it now.
pub fn candidates(ifaces: &[NetInterface], bond: &str) -> Vec<NetInterface> {
    ifaces
        .iter()
        .filter(|i| i.kind == LinkKind::Ethernet && i.physical)
        .filter(|i| {
            i.editable
                || i.read_only
                    .as_ref()
                    .is_some_and(|r| r.code == ReadOnlyCode::Enslaved && r.master.as_deref() == Some(bond))
        })
        .cloned()
        .collect()
}

/// The first free `bondN`.
pub fn next_name(ifaces: &[NetInterface]) -> String {
    (0..100)
        .map(|n| format!("bond{n}"))
        .find(|n| !ifaces.iter().any(|i| &i.name == n))
        .unwrap_or_else(|| "bond99".into())
}

/// The bond fields of the editor.
pub fn form(d: BondDraft, ports: Vec<NetInterface>, show_errors: Signal<bool>) -> AnyView {
    let legacy = matches!(d.mode.get_untracked().as_str(), "balance-rr" | "broadcast");
    let modes = if legacy { MODES_ALL } else { MODES };
    let has_primary = move || matches!(d.mode.get().as_str(), "active-backup" | "balance-alb" | "balance-tlb");
    let has_hash = move || matches!(d.mode.get().as_str(), "802.3ad" | "balance-xor" | "balance-tlb");
    let ports = StoredValue::new(ports);
    // speeds of the checked ports with link differ (fine with a preferred port in active-backup)
    let mixed = move || {
        let mut speeds: Vec<u32> = ports.with_value(|p| {
            p.iter()
                .filter(|i| d.members.with(|m| m.contains(&i.name)) && i.carrier == Some(true))
                .filter_map(|i| i.speed_mbps)
                .collect()
        });
        speeds.sort_unstable();
        speeds.dedup();
        speeds.len() > 1 && !(d.mode.get() == "active-backup" && !d.primary.get().is_empty())
    };
    let members_err = move || (show_errors.get() && d.members.with(Vec::is_empty)).then(|| t("net.v_bond_members"));
    let boxes = ports.with_value(|p| {
        p.iter()
            .map(|i| {
                let (n1, n2) = (i.name.clone(), i.name.clone());
                let state = match i.carrier {
                    Some(false) => t("net.st_no_cable").to_string(),
                    _ => speed_text(i),
                };
                view! {
                    <label class="check">
                        <input type="checkbox" prop:checked=move || d.members.with(|m| m.contains(&n1)) on:change=move |ev| {
                            let on = event_target_checked(&ev);
                            let n = n2.clone();
                            d.members.update(|m| {
                                m.retain(|x| x != &n);
                                if on {
                                    m.push(n);
                                }
                            });
                        } />
                        <span class="mono">{i.name.clone()}</span>
                        <span class="muted small">{state}</span>
                    </label>
                }
            })
            .collect_view()
    });
    let primary_opts = move || {
        let mut m = d.members.get();
        m.sort();
        m.into_iter()
            .map(|n| {
                let (n2, v) = (n.clone(), n.clone());
                view! { <option value=v selected=move || d.primary.with(|p| *p == n2)>{n}</option> }
            })
            .collect_view()
    };
    view! {
        <div class="field">
            <span class="field-label">{tr("net.f_members")}</span>
            {boxes}
            <span class="field-help">{tr("net.members_help")}</span>
            <span class="field-error" aria-live="polite">{move || members_err().map(|e| view! { <Icon icon=icons::CIRCLE_X size=12 />{e} })}</span>
        </div>
        {select_field("net.f_bond_mode", None, modes, d.mode.into(), Callback::new(move |v| d.mode.set(v)))}
        <p class="field-help net-bond-req">{move || t(requirement_key(&d.mode.get()))}</p>
        <Show when=move || d.mode.get() == "802.3ad">
            {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tr("net.lacp_warn")} }.into_any())}
        </Show>
        <div class="field" hidden=move || !has_primary()>
            <label class="field-label" for="net-bond-primary">{tr("net.f_primary")}</label>
            <select id="net-bond-primary" class="input" on:change=move |ev| d.primary.set(event_target_value(&ev))>
                <option value="" selected=move || d.primary.with(String::is_empty)>{tr("net.primary_none")}</option>
                {primary_opts}
            </select>
            <span class="field-help">{tr("net.primary_help")}</span>
        </div>
        <div hidden=move || !has_hash()>
            {select_field("net.f_hash", Some("net.hash_help"), HASHES, d.hash.into(), Callback::new(move |v| d.hash.set(v)))}
        </div>
        <Show when=mixed>
            {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tr("net.mixed_speeds")} }.into_any())}
        </Show>
    }
    .into_any()
}

/// The members' state under the bond's name in the list: "Members: enp3s0 (active), enp4s0
/// (no link)", or the configured members of a bond not created yet.
pub fn members_line(i: &NetInterface) -> Option<String> {
    if let Some(s) = &i.bond {
        let list = s
            .members
            .iter()
            .map(|m| {
                if !m.link_up {
                    format!("{} ({})", m.name, t("net.member_down"))
                } else if m.active {
                    format!("{} ({})", m.name, t("net.member_active"))
                } else {
                    m.name.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        return Some(tf("net.bond_members", &[("list", &list)]));
    }
    let b = i.pending.as_ref().or(i.applied.as_ref())?.bond.as_ref()?;
    Some(tf("net.bond_members", &[("list", &b.members.join(", "))]))
}

/// A 802.3ad bond without a working LACP group.
pub fn lacp_broken(i: &NetInterface) -> bool {
    i.bond.as_ref().is_some_and(|s| s.lacp_ok == Some(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::network::{BondMemberStatus, BondStatus, ReadOnlyReason};

    fn port(name: &str) -> NetInterface {
        NetInterface { name: name.into(), kind: LinkKind::Ethernet, physical: true, editable: true, ..Default::default() }
    }

    #[test]
    fn names_and_candidates() {
        let mut member = port("enp4s0");
        member.editable = false;
        member.read_only = Some(ReadOnlyReason { code: ReadOnlyCode::Enslaved, master: Some("bond0".into()) });
        let veth = NetInterface { name: "veth0".into(), kind: LinkKind::Virtual, editable: true, ..Default::default() };
        let bond = NetInterface { name: "bond0".into(), kind: LinkKind::Bond, ..Default::default() };
        let all = vec![port("enp3s0"), member, veth, bond];
        assert_eq!(candidates(&all, "bond0").iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), vec!["enp3s0", "enp4s0"]);
        assert_eq!(candidates(&all, "bond1").len(), 1);
        assert_eq!(next_name(&all), "bond1");
        assert_eq!(mode_key("802.3ad"), "net.bm.lacp");
        assert_eq!(requirement_key("balance-rr"), "net.bq.legacy");
    }

    #[test]
    fn members_in_the_list() {
        let mut i = NetInterface { name: "bond0".into(), kind: LinkKind::Bond, ..Default::default() };
        i.bond = Some(BondStatus {
            members: vec![
                BondMemberStatus { name: "enp3s0".into(), link_up: true, active: true, ..Default::default() },
                BondMemberStatus { name: "enp4s0".into(), link_up: false, ..Default::default() },
            ],
            lacp_ok: Some(false),
            ..Default::default()
        });
        assert_eq!(members_line(&i).unwrap(), "Members: enp3s0 (active), enp4s0 (no link)");
        assert!(lacp_broken(&i));
        assert!(check(&BondConfig::default()) == vec![("members", "net.v_bond_members")]);
    }
}
