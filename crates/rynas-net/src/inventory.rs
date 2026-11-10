//! Read-only inventory: links, addresses, routes, DNS, which backend manages each interface.
//!
//! Sources: `ip -j -d addr show` (links, kinds, MTU limits, addresses), `ip -j route show`
//! (main table, both families), sysfs (`carrier`, `speed`, `duplex`, `device/driver`),
//! networkd's runtime state (`/run/systemd/netif/links/<ifindex>`: `ADMIN_STATE`,
//! `NETWORK_FILE`, `DNS`), `/run/systemd/resolve/resolv.conf` or `/etc/resolv.conf`, ifupdown's
//! files and `systemctl is-active` of the network services.

use std::net::{IpAddr, Ipv4Addr};

use rynas_api_types::network::{
    DnsState, Duplex, IpAddress, IpFamily, LinkKind, NetBackend, NetRoute, NetServices,
};
use serde::Deserialize;

use crate::ifupdown::Interfaces;
use crate::os::{NetOs, QUICK};

/// One link as the kernel and networkd see it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Link {
    pub name: String,
    pub index: u32,
    pub kind: LinkKind,
    /// `bond`, `veth`, `dummy`... from `ip -d`.
    pub info_kind: Option<String>,
    pub physical: bool,
    pub mac: Option<String>,
    pub mtu: u32,
    pub max_mtu: Option<u32>,
    pub oper_state: String,
    pub carrier: Option<bool>,
    pub speed_mbps: Option<u32>,
    pub duplex: Option<Duplex>,
    pub driver: Option<String>,
    /// Enslaved to a bond or bridge.
    pub master: Option<String>,
    pub addresses: Vec<IpAddress>,
    /// networkd: `ADMIN_STATE` (`configured`, `unmanaged`, `failed`...).
    pub nd_admin: Option<String>,
    /// networkd: the `.network` file applied.
    pub nd_file: Option<String>,
    pub nd_dns: Vec<String>,
    /// networkd: the search domains of the link (`DOMAINS`).
    pub nd_domains: Vec<String>,
    /// A bond's details (`ip -d`).
    pub bond: Option<BondInfo>,
    /// A bond member's details (`ip -d`).
    pub slave: Option<SlaveInfo>,
}

impl Link {
    /// Global (non link-local) IPv4 addresses.
    pub fn routable_v4(&self) -> Vec<&IpAddress> {
        self.addresses
            .iter()
            .filter(|a| {
                a.family == IpFamily::Ipv4
                    && a.address
                        .parse::<Ipv4Addr>()
                        .is_ok_and(|ip| !ip.is_link_local() && !ip.is_loopback())
            })
            .collect()
    }

    /// `a.b.c.d/nn` of the configured (not leased) global IPv4 addresses.
    pub fn static_v4(&self) -> Vec<String> {
        self.routable_v4()
            .into_iter()
            .filter(|a| !a.dynamic)
            .map(|a| format!("{}/{}", a.address, a.prefix_len))
            .collect()
    }

    pub fn has_ip(&self, ip: &IpAddr) -> bool {
        self.addresses
            .iter()
            .any(|a| a.address.parse::<IpAddr>().is_ok_and(|x| x == *ip))
    }

    /// The port's own MAC address (a bond member shows the bond's MAC as its address).
    pub fn perm_mac(&self) -> Option<&str> {
        self.slave
            .as_ref()
            .and_then(|s| s.perm_hwaddr.as_deref())
            .or(self.mac.as_deref())
    }

    /// The `.network` file networkd applied is one of rynas's.
    pub fn rynas_file(&self) -> bool {
        self.nd_file
            .as_deref()
            .and_then(|f| f.rsplit('/').next())
            .is_some_and(crate::networkd::is_rynas_file)
    }

    /// networkd manages it (a `.network` file applies).
    pub fn networkd_managed(&self) -> bool {
        self.nd_file.as_deref().is_some_and(|f| !f.is_empty())
            && !matches!(
                self.nd_admin.as_deref(),
                Some("unmanaged" | "linger") | None
            )
    }
}

/// Everything read at one moment.
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    pub links: Vec<Link>,
    pub routes: Vec<NetRoute>,
    pub services: NetServices,
    pub dns: DnsState,
    pub ifupdown: Option<Interfaces>,
    pub read_at_ms: u64,
}

/// Test-only: interfaces matching these space-separated globs count as ports (containers have
/// no physical ports, only veth and dummy links).
pub const TEST_PORTS_ENV: &str = "RYNAS_NET_TEST_PORTS";

fn test_ports() -> Vec<String> {
    std::env::var(TEST_PORTS_ENV)
        .map(|v| v.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

impl Inventory {
    pub fn link(&self, name: &str) -> Option<&Link> {
        self.links.iter().find(|l| l.name == name)
    }

    /// The interface that holds `ip` (the local address of a session).
    pub fn iface_of_ip(&self, ip: &IpAddr) -> Option<&Link> {
        if ip.is_loopback() {
            return None;
        }
        // an IPv4-mapped IPv6 local address (dual-stack listener)
        let ip = match ip {
            IpAddr::V6(v6) => v6
                .to_ipv4_mapped()
                .map(IpAddr::V4)
                .unwrap_or(IpAddr::V6(*v6)),
            v4 => *v4,
        };
        self.links.iter().find(|l| l.has_ip(&ip))
    }

    /// A port: a physical Ethernet interface (or a test port), not enslaved.
    pub fn is_port(&self, name: &str) -> bool {
        self.link(name).is_some_and(is_port)
    }

    pub fn ports(&self) -> Vec<&Link> {
        self.links.iter().filter(|l| is_port(l)).collect()
    }

    /// The links a management path can run on: the ports and the bonds (not enslaved).
    pub fn paths(&self) -> Vec<&Link> {
        self.links
            .iter()
            .filter(|l| is_port(l) || (l.kind == LinkKind::Bond && l.master.is_none()))
            .collect()
    }

    /// Every physical port, also those enslaved to a bond (the factory network frees them).
    pub fn physical_ports(&self) -> Vec<&Link> {
        self.links.iter().filter(|l| is_physical_port(l)).collect()
    }

    /// The members of a bond now.
    pub fn members_of(&self, bond: &str) -> Vec<&Link> {
        self.links
            .iter()
            .filter(|l| l.master.as_deref() == Some(bond))
            .collect()
    }

    /// The gateway of the default IPv4 route through `iface`.
    pub fn default_gateway(&self, iface: &str) -> Option<String> {
        self.routes
            .iter()
            .find(|r| {
                r.family == IpFamily::Ipv4 && r.destination == "default" && r.interface == iface
            })
            .and_then(|r| r.gateway.clone())
    }

    /// Who configures `link`.
    pub fn backend_of(&self, link: &Link) -> NetBackend {
        if link.networkd_managed() {
            return NetBackend::Networkd;
        }
        if self
            .ifupdown
            .as_ref()
            .is_some_and(|i| i.names().contains(&link.name))
        {
            return NetBackend::Ifupdown;
        }
        if self.services.network_manager && link.kind != LinkKind::Loopback {
            return NetBackend::NetworkManager;
        }
        NetBackend::None
    }
}

pub fn is_port(l: &Link) -> bool {
    l.master.is_none() && is_physical_port(l)
}

/// A physical Ethernet interface (or a test port), enslaved or not.
pub fn is_physical_port(l: &Link) -> bool {
    if l.kind == LinkKind::Loopback {
        return false;
    }
    let tp = test_ports();
    if tp.iter().any(|p| crate::networkd::glob_match(p, &l.name)) {
        return true;
    }
    l.physical && l.kind == LinkKind::Ethernet
}

#[derive(Deserialize)]
struct IpLink {
    ifindex: u32,
    ifname: String,
    #[serde(default)]
    mtu: u32,
    #[serde(default)]
    operstate: String,
    #[serde(default)]
    link_type: String,
    #[serde(default)]
    address: Option<String>,
    #[serde(default)]
    max_mtu: Option<u32>,
    #[serde(default)]
    master: Option<String>,
    #[serde(default)]
    linkinfo: Option<IpLinkInfo>,
    #[serde(default)]
    addr_info: Vec<IpAddrInfo>,
}

#[derive(Deserialize)]
struct IpLinkInfo {
    #[serde(default)]
    info_kind: Option<String>,
    #[serde(default)]
    info_data: Option<serde_json::Value>,
    #[serde(default)]
    info_slave_kind: Option<String>,
    #[serde(default)]
    info_slave_data: Option<serde_json::Value>,
}

/// A bond as `ip -d` shows it (`linkinfo.info_data`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BondInfo {
    pub mode: Option<String>,
    pub active_slave: Option<String>,
    pub primary: Option<String>,
    pub miimon: Option<u32>,
    pub xmit_hash_policy: Option<String>,
    pub ad_lacp_rate: Option<String>,
    /// 802.3ad: the active aggregator and the partner's MAC.
    pub ad_aggregator: Option<u32>,
    pub ad_partner_mac: Option<String>,
}

/// A bond member as `ip -d` shows it (`linkinfo.info_slave_data`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SlaveInfo {
    /// `ACTIVE` or `BACKUP`.
    pub state: Option<String>,
    /// `UP` or `DOWN`.
    pub mii_status: Option<String>,
    pub link_failure_count: u32,
    /// The port's own MAC (the link's MAC may be the bond's).
    pub perm_hwaddr: Option<String>,
    pub ad_aggregator_id: Option<u32>,
}

fn json_str(v: &serde_json::Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(str::to_string).filter(|s| !s.is_empty())
}

fn json_u32(v: &serde_json::Value, k: &str) -> Option<u32> {
    v.get(k).and_then(|x| x.as_u64()).and_then(|n| u32::try_from(n).ok())
}

impl BondInfo {
    fn parse(v: &serde_json::Value) -> Self {
        let ad = v.get("ad_info");
        BondInfo {
            mode: json_str(v, "mode"),
            active_slave: json_str(v, "active_slave"),
            primary: json_str(v, "primary"),
            miimon: json_u32(v, "miimon"),
            xmit_hash_policy: json_str(v, "xmit_hash_policy"),
            ad_lacp_rate: json_str(v, "ad_lacp_rate"),
            ad_aggregator: ad.and_then(|a| json_u32(a, "aggregator")),
            ad_partner_mac: ad.and_then(|a| json_str(a, "partner_mac")),
        }
    }
}

impl SlaveInfo {
    fn parse(v: &serde_json::Value) -> Self {
        SlaveInfo {
            state: json_str(v, "state"),
            mii_status: json_str(v, "mii_status"),
            link_failure_count: json_u32(v, "link_failure_count").unwrap_or(0),
            perm_hwaddr: json_str(v, "perm_hwaddr"),
            ad_aggregator_id: json_u32(v, "ad_aggregator_id"),
        }
    }
}

#[derive(Deserialize)]
struct IpAddrInfo {
    #[serde(default)]
    family: String,
    #[serde(default)]
    local: String,
    #[serde(default)]
    prefixlen: u8,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    dynamic: bool,
}

#[derive(Deserialize)]
struct IpRoute {
    #[serde(default)]
    dst: String,
    #[serde(default)]
    gateway: Option<String>,
    #[serde(default)]
    dev: String,
    #[serde(default)]
    metric: Option<u32>,
    #[serde(default)]
    protocol: Option<String>,
}

/// Parses `ip -j -d addr show`.
pub fn parse_ip_addr(json: &str) -> Result<Vec<Link>, String> {
    let v: Vec<IpLink> = serde_json::from_str(json).map_err(|e| format!("ip -j addr: {e}"))?;
    Ok(v.into_iter()
        .map(|l| {
            let (info_kind, bond, slave) = match l.linkinfo {
                Some(i) => {
                    let bond = (i.info_kind.as_deref() == Some("bond"))
                        .then(|| i.info_data.as_ref().map(BondInfo::parse).unwrap_or_default());
                    let slave = (i.info_slave_kind.as_deref() == Some("bond")).then(|| {
                        i.info_slave_data.as_ref().map(SlaveInfo::parse).unwrap_or_default()
                    });
                    (i.info_kind, bond, slave)
                }
                None => (None, None, None),
            };
            let kind = match (l.link_type.as_str(), info_kind.as_deref()) {
                ("loopback", _) => LinkKind::Loopback,
                (_, Some("bond")) => LinkKind::Bond,
                (_, Some("bridge")) => LinkKind::Bridge,
                (_, Some("vlan")) => LinkKind::Vlan,
                (_, Some(_)) => LinkKind::Virtual,
                ("ether", None) => LinkKind::Ethernet,
                ("none", None) => LinkKind::Virtual,
                _ => LinkKind::Other,
            };
            Link {
                name: l.ifname,
                index: l.ifindex,
                kind,
                info_kind,
                physical: false,
                mac: l
                    .address
                    .filter(|m| m.len() == 17 && m != "00:00:00:00:00:00"),
                mtu: l.mtu,
                max_mtu: l.max_mtu.filter(|m| *m > 0),
                oper_state: l.operstate.to_ascii_lowercase(),
                master: l.master,
                bond,
                slave,
                addresses: l
                    .addr_info
                    .into_iter()
                    .filter(|a| !a.local.is_empty())
                    .map(|a| IpAddress {
                        address: a.local,
                        prefix_len: a.prefixlen,
                        family: if a.family == "inet6" {
                            IpFamily::Ipv6
                        } else {
                            IpFamily::Ipv4
                        },
                        scope: a.scope,
                        dynamic: a.dynamic,
                    })
                    .collect(),
                ..Default::default()
            }
        })
        .collect())
}

/// Parses `ip -j route show table main` (one family).
pub fn parse_ip_route(json: &str, family: IpFamily) -> Result<Vec<NetRoute>, String> {
    let v: Vec<IpRoute> = serde_json::from_str(json).map_err(|e| format!("ip -j route: {e}"))?;
    Ok(v.into_iter()
        .filter(|r| !r.dev.is_empty())
        .map(|r| NetRoute {
            family,
            destination: r.dst,
            gateway: r.gateway,
            interface: r.dev,
            metric: r.metric,
            protocol: r.protocol,
        })
        .collect())
}

/// `KEY=value` lines (networkd link state files, os-release style).
pub fn parse_kv(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        .collect()
}

/// `nameserver` and `search` of a resolv.conf.
pub fn parse_resolv(text: &str, source: &str) -> DnsState {
    let mut d = DnsState {
        source: source.into(),
        ..Default::default()
    };
    for l in text.lines() {
        let mut w = l.split_whitespace();
        match w.next() {
            Some("nameserver") => d.servers.extend(w.next().map(str::to_string)),
            Some("search") | Some("domain") => d.search.extend(w.map(str::to_string)),
            _ => {}
        }
    }
    d
}

const SERVICES: [&str; 5] = [
    "systemd-networkd.service",
    "systemd-resolved.service",
    "networking.service",
    "NetworkManager.service",
    "avahi-daemon.service",
];

/// `systemctl is-active` of the network services (one line per unit, in order).
pub fn services(os: &dyn NetOs) -> NetServices {
    let mut argv = vec!["systemctl", "is-active"];
    argv.extend(SERVICES);
    let out = os.run(&argv, QUICK).map(|o| o.stdout).unwrap_or_default();
    let states: Vec<bool> = out.lines().map(|l| l.trim() == "active").collect();
    let at = |i: usize| states.get(i).copied().unwrap_or(false);
    NetServices {
        networkd: at(0),
        resolved: at(1),
        ifupdown: at(2),
        network_manager: at(3),
        avahi: at(4),
    }
}

/// Reads everything.
pub fn read(os: &dyn NetOs) -> Result<Inventory, String> {
    let out = os.run_ok(&["ip", "-j", "-d", "addr", "show"], QUICK)?;
    let mut links = parse_ip_addr(&out.stdout)?;
    for l in &mut links {
        let sys = format!("/sys/class/net/{}", l.name);
        l.physical = os.runtime_exists(&format!("{sys}/device"));
        if l.kind == LinkKind::Ethernet
            && (os.runtime_exists(&format!("{sys}/wireless"))
                || os.runtime_exists(&format!("{sys}/phy80211")))
        {
            l.kind = LinkKind::Wireless;
        }
        if l.kind == LinkKind::Ethernet && !l.physical && l.info_kind.is_none() {
            // tap and other software links without a kind
            l.kind = LinkKind::Virtual;
        }
        l.driver = os.runtime_link_name(&format!("{sys}/device/driver"));
        // carrier/speed/duplex fail with EINVAL while the link is down
        l.carrier = os
            .read_runtime(&format!("{sys}/carrier"))
            .and_then(|v| v.parse::<u8>().ok())
            .map(|v| v == 1);
        l.speed_mbps = os
            .read_runtime(&format!("{sys}/speed"))
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|v| *v > 0 && *v < u32::MAX as i64)
            .map(|v| v as u32);
        l.duplex = match os.read_runtime(&format!("{sys}/duplex")).as_deref() {
            Some("full") => Some(Duplex::Full),
            Some("half") => Some(Duplex::Half),
            _ => None,
        };
        if let Some(state) = os.read_runtime(&format!("/run/systemd/netif/links/{}", l.index)) {
            for (k, v) in parse_kv(&state) {
                match k.as_str() {
                    "ADMIN_STATE" => l.nd_admin = Some(v),
                    "NETWORK_FILE" => l.nd_file = Some(v).filter(|v| !v.is_empty()),
                    "DNS" => l.nd_dns = v.split_whitespace().map(str::to_string).collect(),
                    "DOMAINS" => {
                        l.nd_domains = v.split_whitespace().map(str::to_string).collect()
                    }
                    _ => {}
                }
            }
        }
    }
    let mut routes = Vec::new();
    for (fam, argv) in [
        (
            IpFamily::Ipv4,
            ["ip", "-j", "-4", "route", "show", "table", "main"],
        ),
        (
            IpFamily::Ipv6,
            ["ip", "-j", "-6", "route", "show", "table", "main"],
        ),
    ] {
        if let Ok(o) = os.run_ok(&argv, QUICK) {
            routes.extend(parse_ip_route(&o.stdout, fam).unwrap_or_default());
        }
    }
    let dns = match os.read_runtime("/run/systemd/resolve/resolv.conf") {
        Some(t) => parse_resolv(&t, "/run/systemd/resolve/resolv.conf"),
        None => parse_resolv(
            &os.read_config(std::path::Path::new("/etc/resolv.conf"))
                .unwrap_or_default(),
            "/etc/resolv.conf",
        ),
    };
    let services = services(os);
    if !services.networkd {
        // networkd leaves its state files behind when it stops: they describe nothing now
        for l in &mut links {
            l.nd_admin = None;
            l.nd_file = None;
            l.nd_dns.clear();
            l.nd_domains.clear();
        }
    }
    Ok(Inventory {
        links,
        routes,
        services,
        dns,
        ifupdown: crate::ifupdown::load(os.root()),
        read_at_ms: os.now_ms(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const IP: &str = r#"[
 {"ifindex":1,"ifname":"lo","flags":["LOOPBACK","UP"],"mtu":65536,"operstate":"UNKNOWN","link_type":"loopback","address":"00:00:00:00:00:00","max_mtu":0,
  "addr_info":[{"family":"inet","local":"127.0.0.1","prefixlen":8,"scope":"host"}]},
 {"ifindex":2,"ifname":"lan1","mtu":1500,"operstate":"UP","link_type":"ether","address":"00:11:32:ab:cd:01","min_mtu":68,"max_mtu":9000,
  "addr_info":[{"family":"inet","local":"192.0.2.37","prefixlen":24,"scope":"global","dynamic":true},
               {"family":"inet","local":"169.254.103.1","prefixlen":16,"scope":"link"},
               {"family":"inet6","local":"fe80::211:32ff:feab:cd01","prefixlen":64,"scope":"link"}]},
 {"ifindex":3,"ifname":"bond0","mtu":1500,"operstate":"DOWN","link_type":"ether","address":"00:11:32:ab:cd:02","linkinfo":{"info_kind":"bond"},"addr_info":[]},
 {"ifindex":4,"ifname":"veth0","mtu":1500,"operstate":"UP","link_type":"ether","address":"aa:bb:cc:dd:ee:ff","master":"docker0","linkinfo":{"info_kind":"veth"},"addr_info":[]}
]"#;

    #[test]
    fn parses_links_and_addresses() {
        let l = parse_ip_addr(IP).unwrap();
        assert_eq!(l.len(), 4);
        assert_eq!(l[0].kind, LinkKind::Loopback);
        assert_eq!(l[0].mac, None);
        assert_eq!(l[0].max_mtu, None);
        assert_eq!(l[1].kind, LinkKind::Ethernet);
        assert_eq!(l[1].max_mtu, Some(9000));
        assert_eq!(l[1].oper_state, "up");
        assert_eq!(l[1].routable_v4().len(), 1);
        assert!(
            l[1].static_v4().is_empty(),
            "leased addresses are not static"
        );
        assert_eq!(l[2].kind, LinkKind::Bond);
        assert_eq!(l[3].kind, LinkKind::Virtual);
        assert_eq!(l[3].master.as_deref(), Some("docker0"));
        let inv = Inventory {
            links: l,
            ..Default::default()
        };
        assert_eq!(
            inv.iface_of_ip(&"192.0.2.37".parse().unwrap())
                .unwrap()
                .name,
            "lan1"
        );
        assert_eq!(
            inv.iface_of_ip(&"::ffff:192.0.2.37".parse().unwrap())
                .unwrap()
                .name,
            "lan1"
        );
        assert!(inv.iface_of_ip(&"127.0.0.1".parse().unwrap()).is_none());
    }

    /// `ip -j -d addr show` of a 802.3ad bond (iproute2 6.x field names) and one member.
    #[test]
    fn parses_bonds_and_members() {
        let json = r#"[
 {"ifindex":3,"ifname":"enp3s0","flags":["BROADCAST","MULTICAST","SLAVE","UP","LOWER_UP"],"mtu":1500,"master":"bond0","operstate":"UP","link_type":"ether","address":"24:5e:be:00:00:01","max_mtu":9000,
  "linkinfo":{"info_slave_kind":"bond","info_slave_data":{"state":"ACTIVE","mii_status":"UP","link_failure_count":2,"perm_hwaddr":"24:5e:be:00:00:03","queue_id":0,"prio":0,"ad_aggregator_id":1,"ad_actor_oper_port_state":61,"ad_actor_oper_port_state_str":["active","aggregating","in_sync","collecting","distributing"],"ad_partner_oper_port_state":61}},"addr_info":[]},
 {"ifindex":9,"ifname":"bond0","flags":["BROADCAST","MULTICAST","MASTER","UP","LOWER_UP"],"mtu":1500,"operstate":"UP","link_type":"ether","address":"24:5e:be:00:00:01","max_mtu":65535,
  "linkinfo":{"info_kind":"bond","info_data":{"mode":"802.3ad","miimon":100,"updelay":0,"downdelay":0,"peer_notify_delay":0,"use_carrier":1,"arp_interval":0,"arp_validate":null,"arp_all_targets":"any","primary_reselect":"always","fail_over_mac":"none","xmit_hash_policy":"layer2+3","resend_igmp":1,"num_peer_notif":1,"all_slaves_active":0,"min_links":0,"lp_interval":1,"packets_per_slave":1,"ad_lacp_active":"on","ad_lacp_rate":"slow","ad_select":"stable","ad_aggregator":1,"ad_num_ports":2,"ad_actor_key":9,"ad_partner_key":1,"ad_partner_mac":"00:11:22:33:44:55","ad_actor_sys_prio":65535,"ad_user_port_key":0,"ad_actor_system":"00:00:00:00:00:00","tlb_dynamic_lb":1,
  "ad_info":{"aggregator":1,"num_ports":2,"actor_key":9,"partner_key":1,"partner_mac":"00:11:22:33:44:55"}}},
  "addr_info":[{"family":"inet","local":"192.0.2.37","prefixlen":24,"scope":"global","dynamic":true}]}
]"#;
        let l = parse_ip_addr(json).unwrap();
        let s = l[0].slave.clone().unwrap();
        assert_eq!(s.perm_hwaddr.as_deref(), Some("24:5e:be:00:00:03"));
        assert_eq!((s.link_failure_count, s.ad_aggregator_id), (2, Some(1)));
        assert_eq!(l[0].perm_mac(), Some("24:5e:be:00:00:03"), "not the bond's MAC");
        assert_eq!(l[1].kind, LinkKind::Bond);
        let b = l[1].bond.clone().unwrap();
        assert_eq!(b.mode.as_deref(), Some("802.3ad"));
        assert_eq!(b.xmit_hash_policy.as_deref(), Some("layer2+3"));
        assert_eq!((b.ad_aggregator, b.ad_partner_mac.as_deref()), (Some(1), Some("00:11:22:33:44:55")));
        let inv = Inventory { links: l, ..Default::default() };
        let st = crate::bond::status(&inv, inv.link("bond0").unwrap()).unwrap();
        assert_eq!(st.lacp_ok, Some(true));
        assert_eq!(st.members[0].link_failures, 2);
        assert_eq!(inv.paths().len(), 1, "the bond, not its member");
    }

    #[test]
    fn parses_routes_resolv_and_state_files() {
        let r = parse_ip_route(
            r#"[{"dst":"default","gateway":"192.0.2.1","dev":"lan1","protocol":"dhcp","metric":1024,"flags":[]},
                {"dst":"192.0.2.0/24","dev":"lan1","protocol":"kernel","scope":"link","prefsrc":"192.0.2.37","flags":[]}]"#,
            IpFamily::Ipv4,
        )
        .unwrap();
        assert_eq!(r[0].gateway.as_deref(), Some("192.0.2.1"));
        let inv = Inventory {
            routes: r,
            ..Default::default()
        };
        assert_eq!(inv.default_gateway("lan1").as_deref(), Some("192.0.2.1"));
        assert_eq!(inv.default_gateway("lan2"), None);
        let d = parse_resolv(
            "# x\nnameserver 192.0.2.53\nnameserver 1.1.1.1\nsearch lan example.org\n",
            "/etc/resolv.conf",
        );
        assert_eq!(d.servers, vec!["192.0.2.53", "1.1.1.1"]);
        assert_eq!(d.search, vec!["lan", "example.org"]);
        let kv = parse_kv(
            "# This is private data. Do not parse.\nADMIN_STATE=configured\nNETWORK_FILE=/etc/systemd/network/05-rynas-lan1.network\nDNS=192.0.2.53 1.1.1.1\n",
        );
        assert_eq!(kv[0], ("ADMIN_STATE".into(), "configured".into()));
        assert_eq!(kv.len(), 3);
    }
}
