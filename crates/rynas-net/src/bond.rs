//! Bonds (link aggregation, decision D-BOND; design in `docs/research/network.md` §3.7).
//!
//! A bond is an interface of rynas's model like a port (`InterfaceConfig` with `bond`): it
//! carries the IPv4 settings, MTU and DNS, its members carry nothing. On disk it is three kinds
//! of rynas files, read back by [`attach`] as the configuration in force:
//!
//! - `05-rynas-bondN.netdev`: `Kind=bond`, the bond's MAC (networkd would otherwise derive
//!   one from the name: a new DHCP client, a new address) and `[Bond]` with `Mode=` always
//!   written (networkd's default would be balance-rr) and `MIIMonitorSec=`;
//! - `05-rynas-bondN.network`: the addresses, like a port's file (link-local from the bond's
//!   MAC);
//! - `05-rynas-<member>.network`: `Bond=`, no link-local, `PrimarySlave=yes` on the primary.
//!
//! networkd never deletes a netdev nor changes the mode of one that exists, so a transaction
//! that removes a bond or changes its kind or mode deletes the link first (`ip link del`)
//! and releases ports that leave a bond (`ip link set nomaster`): [`teardown`] works that out
//! from the files before and after, so the rollback (the same function in reverse) frees the
//! members and puts the ports back the same way.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rynas_api_types::network::{
    BondConfig, BondHash, BondLacpRate, BondMemberStatus, BondMode, BondStatus, InterfaceConfig,
    Ipv4Method,
};

use crate::NETWORKD_DIR;
use crate::inventory::{Inventory, Link};
use crate::networkd::{self, NetworkFile, RYNAS_PREFIX, UnitFile};
use crate::store::FileState;
use crate::validate::FieldError;

/// How long a LACP partner gets to answer after a change (slow LACPDUs come every 30 s).
pub const LACP_WAIT_S: u32 = 35;

/// `bond0` … `bond99`.
pub fn valid_name(name: &str) -> bool {
    name.strip_prefix("bond").is_some_and(|n| {
        (1..=2).contains(&n.len())
            && n.bytes().all(|b| b.is_ascii_digit())
            && (n == "0" || !n.starts_with('0'))
    })
}

/// `/etc/systemd/network/05-rynas-<bond>.netdev`.
pub fn netdev_path(name: &str) -> PathBuf {
    Path::new(NETWORKD_DIR).join(format!("{RYNAS_PREFIX}{name}.netdev"))
}

/// The `.netdev` of a bond (without the managed header).
pub fn render_netdev(name: &str, b: &BondConfig) -> String {
    let mut s = String::from(
        "# rynas: bond (link aggregation); change it from the rynas web interface.\n",
    );
    s.push_str(&format!("[NetDev]\nName={name}\nKind=bond\n"));
    if let Some(m) = &b.mac {
        s.push_str(&format!("MACAddress={m}\n"));
    }
    s.push_str("\n[Bond]\n");
    s.push_str(&format!("Mode={}\n", b.mode.kernel_name()));
    s.push_str(&format!("MIIMonitorSec={}ms\n", b.miimon_ms));
    if let Some(h) = b.hash_policy {
        s.push_str(&format!("TransmitHashPolicy={}\n", h.kernel_name()));
    }
    if b.mode == BondMode::Lacp {
        s.push_str(&format!(
            "LACPTransmitRate={}\n",
            match b.lacp_rate {
                BondLacpRate::Slow => "slow",
                BondLacpRate::Fast => "fast",
            }
        ));
    }
    if b.primary.is_some() {
        // back to the primary as soon as it has link again (also when the speeds differ)
        s.push_str("PrimaryReselectPolicy=always\n");
    }
    if let Some(n) = b.min_links {
        s.push_str(&format!("MinLinks={n}\n"));
    }
    if let Some(n) = b.updelay_ms {
        s.push_str(&format!("UpDelaySec={n}ms\n"));
    }
    if let Some(n) = b.downdelay_ms {
        s.push_str(&format!("DownDelaySec={n}ms\n"));
    }
    s
}

/// The `.network` of a member port (without the managed header).
pub fn render_member(port: &str, bond: &str, b: &BondConfig) -> String {
    let mut s = format!(
        "# rynas: member of {bond}; its addresses are the bond's.\n[Match]\nName={port}\n\n[Network]\nBond={bond}\n"
    );
    // the rescue link-local address lives on the bond (same MAC)
    s.push_str("LinkLocalAddressing=no\n");
    if b.primary.as_deref() == Some(port) && b.mode.has_primary() {
        s.push_str("PrimarySlave=yes\n");
    }
    s
}

/// The `.network` of the bond itself: the addresses, as for a port.
pub fn render_network(name: &str, c: &InterfaceConfig) -> String {
    let mac = c.bond.as_ref().and_then(|b| b.mac.as_deref());
    let text = networkd::render_network(name, c, mac);
    match text.split_once('\n') {
        Some((_, rest)) => format!(
            "# rynas: network settings of this bond; change them from the rynas web interface.\n{rest}"
        ),
        None => text,
    }
}

/// What a `.netdev` file says, as far as rynas needs it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Netdev {
    pub name: String,
    pub kind: String,
    /// `[Bond] Mode=`.
    pub mode: Option<String>,
    /// `[VLAN] Id=`.
    pub vlan_id: Option<String>,
    /// A bond's settings (members empty: they are in the members' files).
    pub bond: Option<BondConfig>,
}

fn ms(v: &str) -> Option<u32> {
    let v = v.trim();
    if let Some(n) = v.strip_suffix("ms") {
        return n.trim().parse().ok();
    }
    if let Some(n) = v.strip_suffix('s') {
        return n.trim().parse::<f64>().ok().map(|s| (s * 1000.0).round() as u32);
    }
    // a bare number is seconds for systemd's time spans
    v.parse::<f64>().ok().map(|s| (s * 1000.0).round() as u32)
}

pub fn parse_netdev(text: &str) -> Option<Netdev> {
    let u = UnitFile::parse(text);
    let name = u.last("NetDev", "Name")?.to_string();
    let kind = u.last("NetDev", "Kind")?.to_string();
    let mode = u.last("Bond", "Mode").map(str::to_string);
    let bond = (kind == "bond").then(|| BondConfig {
        members: Vec::new(),
        // networkd's default
        mode: mode.as_deref().and_then(BondMode::parse).unwrap_or(BondMode::BalanceRr),
        hash_policy: u.last("Bond", "TransmitHashPolicy").and_then(BondHash::parse),
        lacp_rate: match u.last("Bond", "LACPTransmitRate") {
            Some("fast") => BondLacpRate::Fast,
            _ => BondLacpRate::Slow,
        },
        primary: None,
        miimon_ms: u.last("Bond", "MIIMonitorSec").and_then(ms).unwrap_or(0),
        mac: u
            .last("NetDev", "MACAddress")
            .map(|m| m.to_ascii_lowercase()),
        min_links: u.last("Bond", "MinLinks").and_then(|v| v.parse().ok()),
        updelay_ms: u.last("Bond", "UpDelaySec").and_then(ms),
        downdelay_ms: u.last("Bond", "DownDelaySec").and_then(ms),
    });
    Some(Netdev {
        name,
        kind,
        mode,
        vlan_id: u.last("VLAN", "Id").map(str::to_string),
        bond,
    })
}

/// A member file read back: (port, bond, primary).
pub type MemberFile = (String, String, bool);

/// Puts the bonds of rynas's `.netdev` files and their members' files into the configuration
/// read from the `.network` files (a bond without its own `.network` has no IPv4 address).
pub fn attach(
    config: &mut BTreeMap<String, InterfaceConfig>,
    netdevs: Vec<Netdev>,
    members: &[MemberFile],
) {
    for nd in netdevs {
        let Some(mut b) = nd.bond else { continue };
        let mut mine: Vec<&MemberFile> = members.iter().filter(|m| m.1 == nd.name).collect();
        mine.sort();
        b.members = mine.iter().map(|m| m.0.clone()).collect();
        b.primary = mine.iter().find(|m| m.2).map(|m| m.0.clone());
        let c = config.entry(nd.name).or_insert_with(|| InterfaceConfig {
            ipv4: Ipv4Method::Disabled,
            ..Default::default()
        });
        c.bond = Some(b);
    }
}

/// The runtime steps a revision needs besides its files, worked out from the files on disk
/// (`read`) and the files the revision writes (`writes`, also the removals).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Teardown {
    /// Netdevs to delete before the reload: removed, or their kind or mode changes.
    pub delete: Vec<String>,
    /// Ports that leave a bond that stays (`ip link set dev <port> nomaster`).
    pub release: Vec<String>,
    /// Members of a deleted bond that comes back: their file does not change, so the reload
    /// would not enslave them to the new link.
    pub reconfigure: Vec<String>,
}

pub fn teardown(read: &dyn Fn(&Path) -> Option<String>, writes: &[FileState]) -> Teardown {
    let ext = |f: &FileState, e: &str| f.path.extension().is_some_and(|x| x == e);
    let mut t = Teardown::default();
    for f in writes.iter().filter(|f| ext(f, "netdev")) {
        let Some(old) = read(&f.path).and_then(|s| parse_netdev(&s)) else {
            continue;
        };
        let new = f.content.as_deref().and_then(parse_netdev);
        let gone = new.is_none_or(|n| {
            n.name != old.name || n.kind != old.kind || n.mode != old.mode || n.vlan_id != old.vlan_id
        });
        if gone && !t.delete.contains(&old.name) {
            t.delete.push(old.name);
        }
    }
    let plain = |names: &[String]| -> Vec<String> {
        names
            .iter()
            .filter(|n| !n.contains(['*', '?', '[', '!']))
            .cloned()
            .collect()
    };
    for f in writes.iter().filter(|f| ext(f, "network")) {
        let old = read(&f.path).map(|s| NetworkFile::parse(&s));
        let new = f.content.as_deref().map(NetworkFile::parse);
        if let Some(o) = &old
            && let Some(b) = &o.bond
        {
            let stays = new.as_ref().and_then(|n| n.bond.as_ref()) == Some(b);
            if !stays && !t.delete.contains(b) {
                for p in plain(&o.names) {
                    if !t.release.contains(&p) {
                        t.release.push(p);
                    }
                }
            }
        }
        if let Some(n) = &new
            && let Some(b) = &n.bond
            && t.delete.contains(b)
        {
            for p in plain(&n.names) {
                if !t.reconfigure.contains(&p) {
                    t.reconfigure.push(p);
                }
            }
        }
    }
    t
}

/// What validation knows about a port that should become a member.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PortFacts {
    /// A physical Ethernet port (enslaved or not).
    pub physical: bool,
    /// Enslaved to this link now.
    pub master: Option<String>,
    pub carrier: Option<bool>,
    pub max_mtu: Option<u32>,
    pub speed_mbps: Option<u32>,
    /// Its own MAC address.
    pub mac: Option<String>,
    /// It holds a routable IPv4 address now (the management path or the lease).
    pub has_ipv4: bool,
}

impl PortFacts {
    pub fn of(l: &Link) -> Self {
        PortFacts {
            physical: crate::inventory::is_physical_port(l),
            master: l.master.clone(),
            carrier: l.carrier,
            max_mtu: l.max_mtu,
            speed_mbps: l.speed_mbps,
            mac: l.perm_mac().map(str::to_ascii_lowercase),
            has_ipv4: !l.routable_v4().is_empty(),
        }
    }
}

fn err(field: &'static str, message: String) -> FieldError {
    FieldError::at(field, message)
}

/// The default MAC of a bond: the member that holds an address now (the DHCP server and the
/// other hosts keep seeing it), else the primary, else the first member.
pub fn default_mac(b: &BondConfig, facts: &dyn Fn(&str) -> Option<PortFacts>) -> Option<String> {
    let mac_of = |p: &str| facts(p).and_then(|f| f.mac);
    b.members
        .iter()
        .find(|m| facts(m).is_some_and(|f| f.has_ipv4))
        .and_then(|m| mac_of(m))
        .or_else(|| b.primary.as_deref().and_then(mac_of))
        .or_else(|| b.members.first().and_then(|m| mac_of(m)))
}

/// Checks and normalizes a bond. `taken`: port → the other bond it is a member of.
/// `adopting`: the bond comes from ifupdown (it already exists; carrier is not required).
pub fn check(
    name: &str,
    b: &BondConfig,
    mtu: Option<u32>,
    facts: &dyn Fn(&str) -> Option<PortFacts>,
    taken: &BTreeMap<String, String>,
    adopting: bool,
) -> Result<BondConfig, FieldError> {
    if !valid_name(name) {
        return Err(err(
            "name",
            format!("{name}: a bond is named bond0 to bond99"),
        ));
    }
    let members: BTreeSet<String> = b.members.iter().map(|m| m.trim().to_string()).collect();
    if members.is_empty() {
        return Err(err("bond.members", format!("{name}: choose at least one port")));
    }
    if members.len() > 16 {
        return Err(err("bond.members", format!("{name}: at most 16 ports")));
    }
    let mut out = BondConfig {
        members: members.iter().cloned().collect(),
        mode: b.mode,
        ..Default::default()
    };
    let mut any_carrier = false;
    let mut max_mtu: Option<u32> = None;
    for m in &members {
        let f = facts(m).ok_or_else(|| err("bond.members", format!("{name}: no port {m}")))?;
        if !f.physical {
            return Err(err(
                "bond.members",
                format!("{name}: {m} is not a physical Ethernet port"),
            ));
        }
        if let Some(other) = f.master.as_deref().filter(|o| *o != name) {
            return Err(err(
                "bond.members",
                format!("{name}: {m} is already a member of {other}"),
            ));
        }
        if let Some(other) = taken.get(m) {
            return Err(err(
                "bond.members",
                format!("{name}: {m} is already a member of {other}"),
            ));
        }
        any_carrier |= f.carrier == Some(true);
        if let Some(x) = f.max_mtu {
            max_mtu = Some(max_mtu.map_or(x, |y| y.min(x)));
        }
    }
    if !any_carrier && !adopting {
        return Err(err(
            "bond.members",
            format!("{name}: no member has a cable connected (no carrier): the bond could not be checked"),
        ));
    }
    if let (Some(mtu), Some(max)) = (mtu, max_mtu)
        && mtu > max
    {
        return Err(err(
            "mtu",
            format!("{name}: MTU {mtu} is larger than a member accepts ({max})"),
        ));
    }
    out.hash_policy = match (b.mode.has_hash(), b.hash_policy) {
        (true, Some(h)) => Some(h),
        (true, None) if b.mode == BondMode::Lacp => Some(BondHash::Layer23),
        (true, None) => None,
        (false, None) => None,
        (false, Some(_)) => {
            return Err(err(
                "bond.hash_policy",
                format!("{name}: a transmit hash policy is only for 802.3ad, balance-xor and balance-tlb"),
            ));
        }
    };
    if b.lacp_rate == BondLacpRate::Fast && b.mode != BondMode::Lacp {
        return Err(err(
            "bond.lacp_rate",
            format!("{name}: the LACP rate is only for 802.3ad"),
        ));
    }
    out.lacp_rate = b.lacp_rate;
    if let Some(p) = b.primary.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        if !b.mode.has_primary() {
            return Err(err(
                "bond.primary",
                format!("{name}: a primary port is only for active-backup, balance-alb and balance-tlb"),
            ));
        }
        if !members.contains(p) {
            return Err(err(
                "bond.primary",
                format!("{name}: the primary port {p} is not a member"),
            ));
        }
        out.primary = Some(p.to_string());
    }
    if !(1..=10_000).contains(&b.miimon_ms) {
        return Err(err(
            "bond.miimon_ms",
            format!("{name}: the link monitoring interval must be 1-10000 ms"),
        ));
    }
    out.miimon_ms = b.miimon_ms;
    if let Some(n) = b.min_links {
        if n as usize > members.len() {
            return Err(err(
                "bond.min_links",
                format!("{name}: at least {n} links up is more than the bond has members"),
            ));
        }
        out.min_links = (n > 0).then_some(n);
    }
    for (field, v) in [("bond.updelay_ms", b.updelay_ms), ("bond.downdelay_ms", b.downdelay_ms)] {
        if v.is_some_and(|v| v > 60_000) {
            return Err(err(field, format!("{name}: a link delay is at most 60000 ms")));
        }
    }
    out.updelay_ms = b.updelay_ms.filter(|v| *v > 0);
    out.downdelay_ms = b.downdelay_ms.filter(|v| *v > 0);
    out.mac = match b.mac.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => {
            let bytes = networkd::parse_mac(m).ok_or_else(|| {
                err("bond.mac", format!("{name}: {m} is not a MAC address"))
            })?;
            if bytes == [0; 6] || bytes[0] & 1 == 1 {
                return Err(err(
                    "bond.mac",
                    format!("{name}: {m} is not a unicast MAC address"),
                ));
            }
            Some(m.to_ascii_lowercase())
        }
        None => default_mac(&out, facts),
    };
    Ok(out)
}

/// Warnings about a valid bond (shown, logged; they do not stop the change).
pub fn warnings(name: &str, b: &BondConfig, facts: &dyn Fn(&str) -> Option<PortFacts>) -> Vec<String> {
    let mut w = Vec::new();
    let speeds: BTreeSet<u32> = b
        .members
        .iter()
        .filter_map(|m| facts(m))
        .filter(|f| f.carrier == Some(true))
        .filter_map(|f| f.speed_mbps)
        .collect();
    let primary_ab = b.mode == BondMode::ActiveBackup && b.primary.is_some();
    if speeds.len() > 1 && !primary_ab {
        w.push(format!(
            "{name}: the members run at different speeds; with active-backup choose the faster port as primary"
        ));
    }
    match b.mode {
        BondMode::Lacp => w.push(format!(
            "{name}: 802.3ad needs a LACP group on the switch ports first; without it the change is rolled back"
        )),
        BondMode::BalanceXor => w.push(format!(
            "{name}: balance-xor needs a static link aggregation group on the switch ports"
        )),
        _ => {}
    }
    w
}

/// Whether a 802.3ad bond has a partner and every member with link is in its aggregator.
fn lacp_state(b: &crate::inventory::BondInfo, members: &[&Link]) -> Result<(), &'static str> {
    let partner = b.ad_partner_mac.as_deref().unwrap_or("00:00:00:00:00:00");
    if partner == "00:00:00:00:00:00" {
        return Err("no LACP partner answers on the members");
    }
    let up: Vec<&&Link> = members
        .iter()
        .filter(|m| m.slave.as_ref().is_some_and(|s| s.mii_status.as_deref() == Some("UP")))
        .collect();
    if up.is_empty() {
        return Err("no member has link");
    }
    if up
        .iter()
        .any(|m| m.slave.as_ref().and_then(|s| s.ad_aggregator_id) != b.ad_aggregator)
    {
        return Err("the members ended up in different aggregators");
    }
    Ok(())
}

/// A bond's state now, for the API.
pub fn status(inv: &Inventory, l: &Link) -> Option<BondStatus> {
    let b = l.bond.as_ref()?;
    let members = inv.members_of(&l.name);
    let lacp = b.mode.as_deref() == Some("802.3ad");
    Some(BondStatus {
        mode: b.mode.clone(),
        active_member: b.active_slave.clone(),
        members: members
            .iter()
            .map(|m| {
                let s = m.slave.clone().unwrap_or_default();
                BondMemberStatus {
                    name: m.name.clone(),
                    link_up: match s.mii_status.as_deref() {
                        Some(v) => v == "UP",
                        None => m.carrier == Some(true),
                    },
                    speed_mbps: m.speed_mbps,
                    link_failures: s.link_failure_count,
                    active: b.active_slave.as_deref() == Some(m.name.as_str()),
                    aggregator_id: s.ad_aggregator_id,
                    mac: m.perm_mac().map(str::to_string),
                }
            })
            .collect(),
        aggregator_id: if lacp { b.ad_aggregator } else { None },
        partner_mac: if lacp { b.ad_partner_mac.clone() } else { None },
        lacp_ok: lacp.then(|| lacp_state(b, &members).is_ok()),
    })
}

/// Differences between the bonds `want` configures and the system; `before` is the
/// configuration the system comes from (its bonds must be gone unless `want` keeps them).
/// `ifupdown`: the revision hands the network to ifupdown, which makes its own bonds.
pub fn verify(
    want: &BTreeMap<String, InterfaceConfig>,
    before: &BTreeMap<String, InterfaceConfig>,
    ifupdown: bool,
    inv: &Inventory,
) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, b) in want.iter().filter_map(|(n, c)| Some((n, c.bond.as_ref()?))) {
        let Some(l) = inv.link(name) else {
            problems.push(format!("{name}: the bond is missing"));
            continue;
        };
        let Some(info) = l.bond.as_ref() else {
            problems.push(format!("{name}: the interface is not a bond"));
            continue;
        };
        if info.mode.as_deref() != Some(b.mode.kernel_name()) {
            problems.push(format!(
                "{name}: the mode is {}, expected {}",
                info.mode.as_deref().unwrap_or("unknown"),
                b.mode.kernel_name()
            ));
        }
        for m in &b.members {
            if let Some(ml) = inv.link(m)
                && ml.master.as_deref() != Some(name.as_str())
            {
                problems.push(format!("{name}: {m} is not a member"));
            }
        }
        if let (Some(want), Some(have)) = (b.mac.as_deref(), l.mac.as_deref())
            && !want.eq_ignore_ascii_case(have)
        {
            problems.push(format!("{name}: the MAC address is {have}, expected {want}"));
        }
        if b.mode == BondMode::Lacp
            && let Err(why) = lacp_state(info, &inv.members_of(name))
        {
            problems.push(format!(
                "{name}: the switch is not configured for LACP ({why})"
            ));
        }
    }
    if !ifupdown {
        for name in before
            .iter()
            .filter(|(n, c)| c.bond.is_some() && !want.get(*n).is_some_and(|w| w.bond.is_some()))
            .map(|(n, _)| n)
        {
            if inv.link(name).is_some_and(|l| l.bond.is_some()) {
                problems.push(format!("{name}: the old bond is still there"));
            }
        }
    }
    problems
}

/// The configuration has a 802.3ad bond (its partner needs time to answer).
pub fn has_lacp(config: &BTreeMap<String, InterfaceConfig>) -> bool {
    config
        .values()
        .any(|c| c.bond.as_ref().is_some_and(|b| b.mode == BondMode::Lacp))
}

/// Members of every bond in `config`: port → bond.
pub fn members(config: &BTreeMap<String, InterfaceConfig>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (n, c) in config {
        if let Some(b) = &c.bond {
            for m in &b.members {
                out.insert(m.clone(), n.clone());
            }
        }
    }
    out
}

/// What the bond watch remembers of a bond between two looks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Seen {
    pub active: Option<String>,
    /// member → (link up, failure count).
    pub members: BTreeMap<String, (bool, u32)>,
    pub lacp_ok: Option<bool>,
}

impl Seen {
    pub fn of(s: &BondStatus) -> Self {
        Seen {
            active: s.active_member.clone(),
            members: s
                .members
                .iter()
                .map(|m| (m.name.clone(), (m.link_up, m.link_failures)))
                .collect(),
            lacp_ok: s.lacp_ok,
        }
    }
}

/// What changed for the worse (or the active member) since the last look: `(code, fields)`.
pub fn changes(name: &str, before: &Seen, now: &Seen) -> Vec<(&'static str, Vec<(&'static str, String)>)> {
    let mut out = Vec::new();
    for (m, (up, failures)) in &now.members {
        let (was_up, was_failures) = before.members.get(m).copied().unwrap_or((true, *failures));
        if (was_up && !up) || *failures > was_failures {
            out.push((
                "net.bond_member_down",
                vec![
                    ("bond", name.to_string()),
                    ("member", m.clone()),
                    ("failures", failures.to_string()),
                ],
            ));
        }
    }
    if now.active.is_some() && before.active.is_some() && now.active != before.active {
        out.push((
            "net.bond_active_changed",
            vec![
                ("bond", name.to_string()),
                ("from", before.active.clone().unwrap_or_default()),
                ("to", now.active.clone().unwrap_or_default()),
            ],
        ));
    }
    if now.lacp_ok == Some(false) && before.lacp_ok != Some(false) {
        out.push(("net.bond_lacp_lost", vec![("bond", name.to_string())]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ab(members: &[&str]) -> BondConfig {
        BondConfig {
            members: members.iter().map(|m| m.to_string()).collect(),
            miimon_ms: 100,
            ..Default::default()
        }
    }

    fn facts(p: &str) -> Option<PortFacts> {
        match p {
            "enp3s0" => Some(PortFacts {
                physical: true,
                carrier: Some(true),
                max_mtu: Some(9000),
                speed_mbps: Some(1000),
                mac: Some("24:5e:be:00:00:01".into()),
                has_ipv4: true,
                ..Default::default()
            }),
            "enp4s0" => Some(PortFacts {
                physical: true,
                carrier: Some(false),
                max_mtu: Some(9216),
                mac: Some("24:5e:be:00:00:02".into()),
                ..Default::default()
            }),
            "enp5s0" => Some(PortFacts {
                physical: true,
                carrier: Some(true),
                speed_mbps: Some(10000),
                master: Some("bond1".into()),
                ..Default::default()
            }),
            "veth0" => Some(PortFacts::default()),
            _ => None,
        }
    }

    #[test]
    fn names() {
        assert!(valid_name("bond0") && valid_name("bond12"));
        assert!(!valid_name("bond") && !valid_name("bond007") && !valid_name("bond01"));
        assert!(!valid_name("bond123") && !valid_name("team0") && !valid_name("bond0.10"));
    }

    #[test]
    fn renders_and_reads_back() {
        let mut b = ab(&["enp3s0", "enp4s0"]);
        b.primary = Some("enp3s0".into());
        b.mac = Some("24:5e:be:00:00:01".into());
        let nd = render_netdev("bond0", &b);
        assert_eq!(
            nd,
            "# rynas: bond (link aggregation); change it from the rynas web interface.\n\
             [NetDev]\nName=bond0\nKind=bond\nMACAddress=24:5e:be:00:00:01\n\n\
             [Bond]\nMode=active-backup\nMIIMonitorSec=100ms\nPrimaryReselectPolicy=always\n"
        );
        assert_eq!(
            render_member("enp3s0", "bond0", &b),
            "# rynas: member of bond0; its addresses are the bond's.\n[Match]\nName=enp3s0\n\n\
             [Network]\nBond=bond0\nLinkLocalAddressing=no\nPrimarySlave=yes\n"
        );
        assert!(!render_member("enp4s0", "bond0", &b).contains("PrimarySlave"));
        let c = InterfaceConfig {
            bond: Some(b.clone()),
            ..Default::default()
        };
        let net = render_network("bond0", &c);
        assert!(net.starts_with("# rynas: network settings of this bond"));
        assert!(net.contains("Name=bond0\n") && net.contains("DHCP=ipv4\n"));
        // the rescue address of the bond comes from its MAC (the old port's address)
        assert!(net.contains("LinkLocalAddressing=yes\nIPv4LLStartAddress=169.254.1.1\n"));
        // read back
        let mut config = BTreeMap::new();
        config.insert("bond0".to_string(), NetworkFile::parse(&net).to_config());
        attach(
            &mut config,
            vec![parse_netdev(&nd).unwrap()],
            &[
                ("enp4s0".into(), "bond0".into(), false),
                ("enp3s0".into(), "bond0".into(), true),
            ],
        );
        assert_eq!(config["bond0"], c);
        // 802.3ad: hash, rate; no primary
        let lacp = BondConfig {
            mode: BondMode::Lacp,
            hash_policy: Some(BondHash::Layer34),
            lacp_rate: BondLacpRate::Fast,
            min_links: Some(1),
            updelay_ms: Some(200),
            ..ab(&["enp3s0"])
        };
        let nd = render_netdev("bond1", &lacp);
        assert!(nd.contains("Mode=802.3ad\nMIIMonitorSec=100ms\nTransmitHashPolicy=layer3+4\nLACPTransmitRate=fast\nMinLinks=1\nUpDelaySec=200ms\n"));
        let back = parse_netdev(&nd).unwrap().bond.unwrap();
        assert_eq!(BondConfig { members: lacp.members.clone(), ..back }, lacp);
        // networkd's defaults when a foreign file leaves them out
        let bare = parse_netdev("[NetDev]\nName=bond9\nKind=bond\n").unwrap().bond.unwrap();
        assert_eq!((bare.mode, bare.miimon_ms), (BondMode::BalanceRr, 0));
        assert_eq!(ms("1s"), Some(1000));
        assert_eq!(ms("0.2"), Some(200));
    }

    #[test]
    fn validation() {
        let none = BTreeMap::new();
        let b = check("bond0", &ab(&["enp4s0", "enp3s0"]), None, &facts, &none, false).unwrap();
        assert_eq!(b.members, vec!["enp3s0", "enp4s0"]);
        // the MAC of the port that holds the address
        assert_eq!(b.mac.as_deref(), Some("24:5e:be:00:00:01"));
        assert_eq!(b.hash_policy, None);
        let field = |b: BondConfig, mtu| check("bond0", &b, mtu, &facts, &none, false).unwrap_err().field;
        assert_eq!(field(ab(&[]), None), "bond.members");
        assert_eq!(field(ab(&["enp9s0"]), None), "bond.members");
        assert_eq!(field(ab(&["veth0"]), None), "bond.members");
        assert_eq!(field(ab(&["enp5s0"]), None), "bond.members");
        // no carrier at all
        assert_eq!(field(ab(&["enp4s0"]), None), "bond.members");
        assert!(check("bond0", &ab(&["enp4s0"]), None, &facts, &none, true).is_ok());
        // in another bond of the configuration
        let taken: BTreeMap<String, String> = [("enp4s0".to_string(), "bond2".to_string())].into();
        assert!(check("bond0", &ab(&["enp3s0", "enp4s0"]), None, &facts, &taken, false).is_err());
        assert_eq!(field(ab(&["enp3s0", "enp4s0"]), Some(9216)), "mtu");
        assert!(check("bond0", &ab(&["enp3s0", "enp4s0"]), Some(9000), &facts, &none, false).is_ok());
        assert_eq!(
            field(BondConfig { primary: Some("enp9s0".into()), ..ab(&["enp3s0"]) }, None),
            "bond.primary"
        );
        assert_eq!(
            field(
                BondConfig { mode: BondMode::Lacp, primary: Some("enp3s0".into()), ..ab(&["enp3s0"]) },
                None
            ),
            "bond.primary"
        );
        assert_eq!(
            field(BondConfig { hash_policy: Some(BondHash::Layer34), ..ab(&["enp3s0"]) }, None),
            "bond.hash_policy"
        );
        assert_eq!(
            field(BondConfig { lacp_rate: BondLacpRate::Fast, ..ab(&["enp3s0"]) }, None),
            "bond.lacp_rate"
        );
        assert_eq!(field(BondConfig { mac: Some("01:00:5e:00:00:01".into()), ..ab(&["enp3s0"]) }, None), "bond.mac");
        assert_eq!(field(BondConfig { miimon_ms: 0, ..ab(&["enp3s0"]) }, None), "bond.miimon_ms");
        assert_eq!(field(BondConfig { min_links: Some(3), ..ab(&["enp3s0"]) }, None), "bond.min_links");
        assert_eq!(check("bond100", &ab(&["enp3s0"]), None, &facts, &none, false).unwrap_err().field, "name");
        // 802.3ad gets layer2+3 unless told otherwise
        let l = check("bond0", &BondConfig { mode: BondMode::Lacp, ..ab(&["enp3s0"]) }, None, &facts, &none, false).unwrap();
        assert_eq!(l.hash_policy, Some(BondHash::Layer23));
        // warnings
        let f2 = |p: &str| facts(p).map(|mut f| {
            f.carrier = Some(true);
            f.speed_mbps = Some(if p == "enp3s0" { 1000 } else { 2500 });
            f
        });
        let w = warnings("bond0", &ab(&["enp3s0", "enp4s0"]), &f2);
        assert!(w[0].contains("different speeds"), "{w:?}");
        let quiet = BondConfig { primary: Some("enp4s0".into()), ..ab(&["enp3s0", "enp4s0"]) };
        assert!(warnings("bond0", &quiet, &f2).is_empty());
        let w = warnings("bond0", &BondConfig { mode: BondMode::Lacp, ..ab(&["enp3s0"]) }, &facts);
        assert!(w[0].contains("LACP group"));
    }

    #[test]
    fn teardown_from_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = |n: &str| dir.path().join(n);
        let b = ab(&["enp3s0", "enp4s0"]);
        let nd = render_netdev("bond0", &b);
        std::fs::write(p("05-rynas-bond0.netdev"), &nd).unwrap();
        std::fs::write(p("05-rynas-enp3s0.network"), render_member("enp3s0", "bond0", &b)).unwrap();
        std::fs::write(p("05-rynas-enp4s0.network"), render_member("enp4s0", "bond0", &b)).unwrap();
        let read = |x: &Path| std::fs::read_to_string(x).ok();
        let member = |n: &str, bond: &BondConfig| FileState::present(p(&format!("05-rynas-{n}.network")), render_member(n, "bond0", bond));
        // same files: nothing to do
        let same = vec![
            FileState::present(p("05-rynas-bond0.netdev"), nd.clone()),
            member("enp3s0", &b),
            member("enp4s0", &b),
        ];
        assert_eq!(teardown(&read, &same), Teardown::default());
        // a mode change: delete, then enslave the unchanged members again
        let lacp = BondConfig { mode: BondMode::Lacp, hash_policy: Some(BondHash::Layer23), ..b.clone() };
        let changed = vec![
            FileState::present(p("05-rynas-bond0.netdev"), render_netdev("bond0", &lacp)),
            member("enp3s0", &lacp),
            member("enp4s0", &lacp),
        ];
        let t = teardown(&read, &changed);
        assert_eq!(t.delete, vec!["bond0"]);
        assert_eq!(t.reconfigure, vec!["enp3s0", "enp4s0"]);
        assert!(t.release.is_empty());
        // a member leaves a bond that stays
        let one = ab(&["enp3s0"]);
        let leave = vec![
            FileState::present(p("05-rynas-bond0.netdev"), render_netdev("bond0", &one)),
            member("enp3s0", &one),
            FileState::present(p("05-rynas-enp4s0.network"), "[Match]\nName=enp4s0\n".into()),
        ];
        let t = teardown(&read, &leave);
        assert_eq!((t.delete.len(), t.release.clone()), (0, vec!["enp4s0".to_string()]));
        // the bond goes away: delete only (members are freed by it)
        let gone = vec![
            FileState::absent(p("05-rynas-bond0.netdev")),
            FileState::present(p("05-rynas-enp3s0.network"), "[Match]\nName=enp3s0\n\n[Network]\nDHCP=ipv4\n".into()),
            FileState::absent(p("05-rynas-enp4s0.network")),
        ];
        let t = teardown(&read, &gone);
        assert_eq!(t, Teardown { delete: vec!["bond0".into()], ..Default::default() });
    }

    #[test]
    fn watch_changes() {
        let s = |active: &str, up: bool, failures: u32, lacp: Option<bool>| Seen {
            active: Some(active.into()),
            members: [("enp3s0".to_string(), (up, failures)), ("enp4s0".to_string(), (true, 0))].into(),
            lacp_ok: lacp,
        };
        assert!(changes("bond0", &s("enp3s0", true, 0, None), &s("enp3s0", true, 0, None)).is_empty());
        let c = changes("bond0", &s("enp3s0", true, 0, None), &s("enp4s0", false, 1, None));
        let codes: Vec<&str> = c.iter().map(|x| x.0).collect();
        assert_eq!(codes, vec!["net.bond_member_down", "net.bond_active_changed"]);
        // a flap between two looks still counts (the failure counter)
        let c = changes("bond0", &s("enp3s0", true, 0, None), &s("enp3s0", true, 2, None));
        assert_eq!(c[0].0, "net.bond_member_down");
        let c = changes("bond0", &s("enp3s0", true, 0, Some(true)), &s("enp3s0", true, 0, Some(false)));
        assert_eq!(c[0].0, "net.bond_lacp_lost");
        assert!(changes("bond0", &s("enp3s0", true, 0, Some(false)), &s("enp3s0", true, 0, Some(false))).is_empty());
    }
}
