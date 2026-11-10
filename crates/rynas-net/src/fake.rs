//! A scripted kernel and service manager for the tests: links with addresses and MTU,
//! default routes, services, and the effect of the commands the engine runs
//! (`networkctl reconfigure` applies the matching `.network` file, `systemctl restart
//! networking` applies `/etc/network/interfaces`, `ip addr del`, `ip link set mtu`...).
//!
//! Like the real networkd it does **not** reset an MTU when `MTUBytes=` disappears from a file
//! (the behaviour behind netplan's LP#1768802), so the engine's repair step is exercised.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rynas_tasks::CmdOutput;

use crate::networkd::{NetworkFile, network_files};
use crate::bond;
use crate::os::NetOs;

#[derive(Debug, Clone, Default)]
pub struct FakeLink {
    pub index: u32,
    pub mac: String,
    pub mtu: u32,
    pub max_mtu: u32,
    pub carrier: bool,
    pub physical: bool,
    /// `(address, prefix, dynamic)`.
    pub addrs: Vec<(String, u8, bool)>,
    /// `.network` file networkd applied.
    pub nd_file: Option<String>,
    /// DNS servers networkd learned for the link (shown while it manages the link).
    pub nd_dns: Vec<String>,
    /// A bond (made from a `.netdev`): its mode. Like networkd, never changed in place.
    pub bond_mode: Option<String>,
    /// Enslaved to this bond.
    pub master: Option<String>,
    /// Link failures the bond counted on this member.
    pub failures: u32,
}

#[derive(Debug, Default)]
pub struct FakeState {
    pub links: BTreeMap<String, FakeLink>,
    /// iface → gateway of its default route.
    pub default_routes: BTreeMap<String, String>,
    /// unit (without `.service`) → active.
    pub active: BTreeMap<String, bool>,
    pub enabled: BTreeMap<String, bool>,
    /// Every command run, joined with spaces.
    pub log: Vec<String>,
    pub runtime: BTreeMap<String, String>,
    /// Addresses another host already uses (`arping -D` sees them).
    pub taken: Vec<String>,
    /// Hosts that answer pings.
    pub reachable: Vec<String>,
    /// DNS servers that answer queries.
    pub dns_up: Vec<String>,
    /// DNS servers of the DHCP leases: iface → servers.
    pub lease_dns: BTreeMap<String, Vec<String>>,
    /// DHCP leases networkd gets: iface → (address/prefix, gateway).
    pub leases: BTreeMap<String, (String, String)>,
    /// networkd ignores reconfigure requests (a broken daemon).
    pub broken_networkd: bool,
    /// `--on-active` of the armed rollback timer.
    pub timer: Option<String>,
    pub now_ms: u64,
    /// The switch's MAC when it has a LACP group on the ports (802.3ad bonds find a partner);
    /// `None`: no partner answers, each member ends up in its own aggregator.
    pub lacp_partner: Option<String>,
}

pub struct FakeOs {
    dir: tempfile::TempDir,
    pub state: Mutex<FakeState>,
}

fn out(code: i32, stdout: impl Into<String>) -> CmdOutput {
    CmdOutput {
        code: Some(code),
        stdout: stdout.into(),
        stderr: String::new(),
    }
}

impl Default for FakeOs {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeOs {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("temporary directory");
        std::fs::create_dir_all(dir.path().join("etc/systemd/network")).unwrap();
        let s = FakeState {
            now_ms: 1_800_000_000_000,
            ..Default::default()
        };
        Self {
            dir,
            state: Mutex::new(s),
        }
    }

    /// A port with carrier and the default MTU.
    pub fn add_port(&self, name: &str, mac: &str) {
        let mut s = self.state.lock().unwrap();
        let index = s.links.len() as u32 + 2;
        s.links.insert(
            name.into(),
            FakeLink {
                index,
                mac: mac.into(),
                mtu: 1500,
                max_mtu: 9000,
                carrier: true,
                physical: true,
                ..Default::default()
            },
        );
    }

    pub fn set_active(&self, unit: &str, on: bool) {
        let mut s = self.state.lock().unwrap();
        s.active.insert(unit.into(), on);
        s.enabled.insert(unit.into(), on);
    }

    pub fn add_addr(&self, iface: &str, cidr: &str, dynamic: bool) {
        let (a, p) = cidr.split_once('/').unwrap();
        let mut s = self.state.lock().unwrap();
        s.links
            .get_mut(iface)
            .unwrap()
            .addrs
            .push((a.into(), p.parse().unwrap(), dynamic));
    }

    pub fn add_runtime_file(&self, path: &str, content: &str) {
        self.state
            .lock()
            .unwrap()
            .runtime
            .insert(path.into(), content.into());
    }

    pub fn write_config(&self, abs: &str, content: &str) {
        let p = self.path(Path::new(abs));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    /// Global IPv4 addresses of a link, `a.b.c.d/nn`.
    pub fn addrs(&self, iface: &str) -> Vec<String> {
        let s = self.state.lock().unwrap();
        s.links[iface]
            .addrs
            .iter()
            .filter(|(a, _, _)| !a.starts_with("169.254."))
            .map(|(a, p, _)| format!("{a}/{p}"))
            .collect()
    }

    pub fn mtu(&self, iface: &str) -> u32 {
        self.state.lock().unwrap().links[iface].mtu
    }

    pub fn log(&self) -> Vec<String> {
        self.state.lock().unwrap().log.clone()
    }

    pub fn ran(&self, prefix: &str) -> bool {
        self.log().iter().any(|l| l.starts_with(prefix))
    }

    fn link_json(s: &FakeState) -> String {
        let mut v = vec![serde_json::json!({
            "ifindex": 1, "ifname": "lo", "mtu": 65536, "operstate": "UNKNOWN",
            "link_type": "loopback", "address": "00:00:00:00:00:00",
            "addr_info": [{"family": "inet", "local": "127.0.0.1", "prefixlen": 8, "scope": "host"}]
        })];
        for (name, l) in &s.links {
            let addrs: Vec<_> = l
                .addrs
                .iter()
                .map(|(a, p, d)| {
                    let scope = if a.starts_with("169.254.") { "link" } else { "global" };
                    serde_json::json!({"family": "inet", "local": a, "prefixlen": p, "scope": scope, "dynamic": d})
                })
                .collect();
            let carrier = Self::carrier(s, name);
            let mut j = serde_json::json!({
                "ifindex": l.index, "ifname": name, "mtu": l.mtu,
                "operstate": if carrier { "UP" } else { "DOWN" },
                "link_type": "ether", "address": l.mac, "max_mtu": l.max_mtu,
                "addr_info": addrs
            });
            let lacp = |b: &str| s.links.get(b).and_then(|x| x.bond_mode.as_deref()) == Some("802.3ad");
            if let Some(mode) = &l.bond_mode {
                let slaves: Vec<(&String, &FakeLink)> = s
                    .links
                    .iter()
                    .filter(|(_, x)| x.master.as_deref() == Some(name.as_str()))
                    .collect();
                let active = slaves.iter().find(|(_, x)| x.carrier).map(|(n, _)| n.to_string());
                let mut data = serde_json::json!({"mode": mode, "miimon": 100});
                if mode == "802.3ad" {
                    data["ad_info"] = serde_json::json!({
                        "aggregator": 1,
                        "num_ports": slaves.len(),
                        "partner_mac": s.lacp_partner.clone().unwrap_or("00:00:00:00:00:00".into()),
                    });
                } else if let Some(a) = active.filter(|_| mode != "balance-xor") {
                    data["active_slave"] = a.into();
                }
                j["linkinfo"] = serde_json::json!({"info_kind": "bond", "info_data": data});
            }
            if let Some(m) = &l.master {
                j["master"] = m.clone().into();
                let mut data = serde_json::json!({
                    "state": "ACTIVE",
                    "mii_status": if l.carrier { "UP" } else { "DOWN" },
                    "link_failure_count": l.failures,
                    "perm_hwaddr": l.mac,
                });
                if lacp(m) {
                    // without a partner every port forms its own aggregator
                    let id = if s.lacp_partner.is_some() { 1 } else { l.index };
                    data["ad_aggregator_id"] = id.into();
                }
                j["linkinfo"] = serde_json::json!({"info_slave_kind": "bond", "info_slave_data": data});
            }
            v.push(j);
        }
        serde_json::to_string(&v).unwrap()
    }

    /// A bond has carrier when one of its members has.
    fn carrier(s: &FakeState, name: &str) -> bool {
        let Some(l) = s.links.get(name) else {
            return false;
        };
        if l.bond_mode.is_some() {
            return s
                .links
                .values()
                .any(|x| x.master.as_deref() == Some(name) && x.carrier);
        }
        l.carrier
    }

    /// networkd's reload: creates the netdevs of the `.netdev` files that do not exist (an
    /// existing one is left as it is), then applies the `.network` files to every link.
    fn networkd_reload(&self, s: &mut FakeState) {
        if s.broken_networkd || !s.active.get("systemd-networkd").copied().unwrap_or(false) {
            return;
        }
        let dir = self.path(Path::new(crate::NETWORKD_DIR));
        let mut names: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        names.sort();
        for p in names.iter().filter(|p| p.extension().is_some_and(|e| e == "netdev")) {
            let Some(nd) = std::fs::read_to_string(p).ok().and_then(|t| bond::parse_netdev(&t)) else {
                continue;
            };
            if nd.kind != "bond" || s.links.contains_key(&nd.name) {
                continue;
            }
            let index = s.links.values().map(|l| l.index).max().unwrap_or(1) + 1;
            let b = nd.bond.unwrap_or_default();
            s.links.insert(
                nd.name.clone(),
                FakeLink {
                    index,
                    mac: b.mac.unwrap_or_else(|| format!("02:00:00:00:00:{index:02x}")),
                    mtu: 1500,
                    max_mtu: 65535,
                    carrier: true,
                    bond_mode: Some(b.mode.kernel_name().into()),
                    ..Default::default()
                },
            );
        }
        let all: Vec<String> = s.links.keys().cloned().collect();
        for n in all {
            self.networkd_apply(s, &n);
        }
    }

    fn route_json(s: &FakeState) -> String {
        let mut v = Vec::new();
        for (iface, gw) in &s.default_routes {
            v.push(serde_json::json!({"dst": "default", "gateway": gw, "dev": iface, "protocol": "static"}));
        }
        serde_json::to_string(&v).unwrap()
    }

    /// networkd applying its files to one link.
    fn networkd_apply(&self, s: &mut FakeState, iface: &str) {
        if s.broken_networkd || !s.active.get("systemd-networkd").copied().unwrap_or(false) {
            return;
        }
        let Some(l) = s.links.get(iface).cloned() else {
            return;
        };
        let file = network_files(self.root()).into_iter().find_map(|(_, p)| {
            let nf = NetworkFile::parse(&std::fs::read_to_string(&p).ok()?);
            nf.matches(iface, Some(&l.mac), Some("ether"))
                .then_some((p, nf))
        });
        let bond_exists = file
            .as_ref()
            .and_then(|(_, nf)| nf.bond.as_ref())
            .is_some_and(|b| s.links.contains_key(b));
        let link = s.links.get_mut(iface).unwrap();
        let Some((path, nf)) = file else {
            link.nd_file = None;
            return;
        };
        if let Some(b) = &nf.bond {
            // a member: enslaved once its bond exists, no addresses of its own
            if bond_exists {
                link.master = Some(b.clone());
            }
            link.addrs.clear();
            link.nd_dns.clear();
            link.nd_file = path
                .strip_prefix(self.root())
                .map(|r| format!("/{}", r.display()))
                .ok();
            s.default_routes.remove(iface);
            return;
        }
        let c = nf.to_config();
        link.addrs.retain(|(a, _, _)| a.starts_with("169.254."));
        for a in &c.addresses {
            let (ip, p) = a.split_once('/').unwrap();
            link.addrs.push((ip.into(), p.parse().unwrap(), false));
        }
        let mut gw = c.gateway.clone();
        // static servers first, then those of the lease (networkd's order)
        link.nd_dns = c.dns.clone();
        if c.ipv4 == rynas_api_types::network::Ipv4Method::Dhcp {
            link.nd_dns
                .extend(s.lease_dns.get(iface).cloned().unwrap_or_default());
        }
        if c.ipv4 == rynas_api_types::network::Ipv4Method::Dhcp
            && let Some((a, g)) = s.leases.get(iface)
        {
            let (ip, p) = a.split_once('/').unwrap();
            link.addrs.push((ip.into(), p.parse().unwrap(), true));
            gw = Some(g.clone());
        }
        if let Some(m) = c.mtu {
            link.mtu = m;
        }
        let rel = path
            .strip_prefix(self.root())
            .map(|r| format!("/{}", r.display()))
            .unwrap_or_default();
        link.nd_file = Some(rel);
        match gw {
            Some(g) => {
                s.default_routes.insert(iface.into(), g);
            }
            None => {
                s.default_routes.remove(iface);
            }
        }
    }

    /// ifup -a with the current `/etc/network/interfaces`.
    fn ifupdown_apply(&self, s: &mut FakeState) {
        let Some(ifs) = crate::ifupdown::load(self.root()) else {
            return;
        };
        for (name, st) in &ifs.stanzas {
            if st.family != "inet" || st.method != "static" {
                continue;
            }
            let Some(link) = s.links.get_mut(name) else {
                continue;
            };
            if let Some(a) = &st.address {
                let (ip, p) = a.split_once('/').unwrap();
                if !link.addrs.iter().any(|(x, _, _)| x == ip) {
                    link.addrs.push((ip.into(), p.parse().unwrap(), false));
                }
            }
            if let Some(g) = &st.gateway {
                s.default_routes.insert(name.clone(), g.clone());
            }
        }
    }

    fn unit(u: &str) -> String {
        u.trim_end_matches(".service").to_string()
    }
}

impl NetOs for FakeOs {
    fn run(&self, argv: &[&str], _timeout: Duration) -> Result<CmdOutput, String> {
        let mut s = self.state.lock().unwrap();
        s.log.push(argv.join(" "));
        let a: Vec<&str> = argv.to_vec();
        match a.as_slice() {
            ["ip", "-j", "-d", "addr", "show"] => Ok(out(0, Self::link_json(&s))),
            ["ip", "-j", "-4", "route", ..] => Ok(out(0, Self::route_json(&s))),
            ["ip", "-j", "-6", "route", ..] => Ok(out(0, "[]")),
            ["ip", "link", "set", "dev", dev, "mtu", m] => {
                if let Some(l) = s.links.get_mut(*dev) {
                    l.mtu = m.parse().unwrap_or(l.mtu);
                }
                Ok(out(0, ""))
            }
            ["ip", "addr", "del", cidr, "dev", dev] => {
                let ip = cidr.split('/').next().unwrap_or("");
                if let Some(l) = s.links.get_mut(*dev) {
                    l.addrs.retain(|(x, _, _)| x != ip);
                }
                Ok(out(0, ""))
            }
            ["ip", "addr", "add", cidr, "dev", dev] => {
                let (a, p) = cidr.split_once('/').unwrap_or((cidr, "32"));
                if let Some(l) = s.links.get_mut(*dev)
                    && !l.addrs.iter().any(|(x, _, _)| x == a)
                {
                    l.addrs.push((a.into(), p.parse().unwrap_or(32), false));
                }
                Ok(out(0, ""))
            }
            ["ip", "route", "replace", "default", "via", gw, "dev", dev] => {
                s.default_routes.insert(dev.to_string(), gw.to_string());
                Ok(out(0, ""))
            }
            ["ip", "addr", "flush", "dev", dev, "scope", "global"] => {
                if let Some(l) = s.links.get_mut(*dev) {
                    l.addrs.retain(|(x, _, _)| x.starts_with("169.254."));
                }
                s.default_routes.remove(*dev);
                Ok(out(0, ""))
            }
            ["networkctl", "reload"] => {
                self.networkd_reload(&mut s);
                Ok(out(0, ""))
            }
            ["ip", "link", "set", "dev", dev, "nomaster"] => {
                if let Some(l) = s.links.get_mut(*dev) {
                    l.master = None;
                }
                Ok(out(0, ""))
            }
            ["ip", "link", "del", "dev", dev] => {
                if s.links.remove(*dev).is_none() {
                    return Ok(out(1, ""));
                }
                s.default_routes.remove(*dev);
                for l in s.links.values_mut() {
                    if l.master.as_deref() == Some(*dev) {
                        l.master = None;
                    }
                }
                Ok(out(0, ""))
            }
            ["networkctl", "reconfigure", ifaces @ ..] => {
                for i in ifaces {
                    self.networkd_apply(&mut s, i);
                }
                Ok(out(0, ""))
            }
            ["systemctl", "is-active", units @ ..] => {
                let lines: Vec<&str> = units
                    .iter()
                    .map(|u| {
                        if s.active.get(&Self::unit(u)).copied().unwrap_or(false) {
                            "active"
                        } else {
                            "inactive"
                        }
                    })
                    .collect();
                Ok(out(3, lines.join("\n")))
            }
            ["systemctl", "is-enabled", unit] => {
                let on = s.enabled.get(&Self::unit(unit)).copied().unwrap_or(false);
                Ok(out(
                    if on { 0 } else { 1 },
                    if on { "enabled" } else { "disabled" },
                ))
            }
            ["systemctl", verb, rest @ ..] => {
                let units: Vec<String> = rest
                    .iter()
                    .filter(|u| !u.starts_with("--"))
                    .map(|u| Self::unit(u))
                    .collect();
                let now = rest.contains(&"--now");
                for u in &units {
                    match *verb {
                        "start" | "restart" => {
                            s.active.insert(u.clone(), true);
                        }
                        "stop" => {
                            s.active.insert(u.clone(), false);
                        }
                        "enable" => {
                            s.enabled.insert(u.clone(), true);
                            if now {
                                s.active.insert(u.clone(), true);
                            }
                        }
                        "disable" => {
                            s.enabled.insert(u.clone(), false);
                            if now {
                                s.active.insert(u.clone(), false);
                            }
                        }
                        _ => {}
                    }
                    if u == "rynas-net-rollback.timer" && *verb == "stop" {
                        s.timer = None;
                    }
                }
                if matches!(*verb, "start" | "restart" | "enable")
                    && units.iter().any(|u| u == "systemd-networkd")
                    && s.active.get("systemd-networkd").copied().unwrap_or(false)
                {
                    self.networkd_reload(&mut s);
                }
                if matches!(*verb, "start" | "restart") && units.iter().any(|u| u == "networking") {
                    self.ifupdown_apply(&mut s);
                }
                Ok(out(0, ""))
            }
            ["systemd-run", rest @ ..] => {
                s.timer = rest
                    .iter()
                    .find_map(|x| x.strip_prefix("--on-active="))
                    .map(str::to_string);
                Ok(out(0, ""))
            }
            ["arping", rest @ ..] => {
                let addr = rest.last().copied().unwrap_or("");
                Ok(out(
                    if s.taken.iter().any(|t| t == addr) {
                        1
                    } else {
                        0
                    },
                    "",
                ))
            }
            ["ping", rest @ ..] => {
                let host = rest.last().copied().unwrap_or("");
                Ok(out(
                    if s.reachable.iter().any(|t| t == host) {
                        0
                    } else {
                        1
                    },
                    "",
                ))
            }
            _ => Ok(out(0, "")),
        }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn read_runtime(&self, abs: &str) -> Option<String> {
        let s = self.state.lock().unwrap();
        if let Some(v) = s.runtime.get(abs) {
            return Some(v.trim().to_string());
        }
        if let Some(rest) = abs.strip_prefix("/sys/class/net/") {
            let (name, attr) = rest.split_once('/')?;
            s.links.get(name)?;
            let carrier = Self::carrier(&s, name);
            return match attr {
                "carrier" => Some(if carrier { "1" } else { "0" }.into()),
                "speed" => carrier.then(|| "1000".into()),
                "duplex" => carrier.then(|| "full".into()),
                _ => None,
            };
        }
        if let Some(idx) = abs.strip_prefix("/run/systemd/netif/links/") {
            let (_, l) = s.links.iter().find(|(_, l)| l.index.to_string() == idx)?;
            return Some(match &l.nd_file {
                Some(f) => format!(
                    "ADMIN_STATE=configured\nNETWORK_FILE={f}\nDNS={}\n",
                    l.nd_dns.join(" ")
                ),
                None => "ADMIN_STATE=unmanaged\n".into(),
            });
        }
        if abs == "/proc/sys/kernel/hostname" {
            return Some("nas".into());
        }
        None
    }

    fn runtime_exists(&self, abs: &str) -> bool {
        let s = self.state.lock().unwrap();
        if s.runtime.contains_key(abs) {
            return true;
        }
        if let Some(rest) = abs.strip_prefix("/sys/class/net/")
            && let Some(name) = rest.strip_suffix("/device")
        {
            return s.links.get(name).is_some_and(|l| l.physical);
        }
        false
    }

    fn runtime_link_name(&self, abs: &str) -> Option<String> {
        abs.ends_with("/device/driver").then(|| "r8169".into())
    }

    fn runtime_list(&self, _abs: &str) -> Vec<String> {
        Vec::new()
    }

    fn sleep(&self, d: Duration) {
        self.state.lock().unwrap().now_ms += d.as_millis() as u64;
    }

    fn now_ms(&self) -> u64 {
        self.state.lock().unwrap().now_ms
    }

    fn dns_answers(&self, server: &str) -> bool {
        let s = self.state.lock().unwrap();
        s.dns_up.iter().any(|d| d == server)
    }
}

/// A path below the fake root.
pub fn real(os: &FakeOs, abs: &str) -> PathBuf {
    os.path(Path::new(abs))
}
