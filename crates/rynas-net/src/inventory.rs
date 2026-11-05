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
    if l.master.is_some() || l.kind == LinkKind::Loopback {
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
            let info_kind = l.linkinfo.and_then(|i| i.info_kind);
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
