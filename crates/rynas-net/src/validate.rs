//! Validation of interface settings, before anything touches the system.
//!
//! Per interface: addresses are host addresses of a sane subnet, the gateway is inside one of
//! them and is not one of them, DNS servers are unicast addresses, the MTU is between 576 and
//! the driver maximum. Across interfaces: no two interfaces in the same subnet (the kernel
//! would route through either), no address twice, and at least one **management path**: a port
//! that keeps an IPv4 configuration (DHCP or static). Duplicate addresses on the wire (`arping
//! -D`) are checked by the transaction, which may take seconds.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::str::FromStr;

use rynas_api_types::network::{InterfaceConfig, Ipv4Method};

/// An IPv4 address with its prefix (`192.0.2.10/24`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cidr {
    pub addr: Ipv4Addr,
    pub prefix: u8,
}

impl FromStr for Cidr {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let (a, p) = s
            .split_once('/')
            .ok_or_else(|| format!("{s}: give the address with its prefix, e.g. 192.0.2.10/24"))?;
        let addr: Ipv4Addr = a.parse().map_err(|_| format!("{s}: not an IPv4 address"))?;
        let prefix: u8 = p
            .parse()
            .ok()
            .filter(|p| *p <= 32)
            .ok_or_else(|| format!("{s}: the prefix must be 0-32"))?;
        Ok(Cidr { addr, prefix })
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

impl Cidr {
    pub fn mask(&self) -> u32 {
        if self.prefix == 0 {
            0
        } else {
            u32::MAX << (32 - self.prefix)
        }
    }

    pub fn network(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.addr) & self.mask())
    }

    pub fn broadcast(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.addr) | !self.mask())
    }

    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & self.mask() == u32::from(self.addr) & self.mask()
    }

    /// Whether two subnets overlap.
    pub fn overlaps(&self, other: &Cidr) -> bool {
        let m = self.mask() & other.mask();
        u32::from(self.addr) & m == u32::from(other.addr) & m
    }
}

/// `255.255.255.0` → 24.
pub fn netmask_prefix(mask: &str) -> Option<u8> {
    let m = u32::from(mask.parse::<Ipv4Addr>().ok()?);
    let p = m.leading_ones();
    (m.checked_shl(p).unwrap_or(0) == 0).then_some(p as u8)
}

/// A unicast address a host may use (not loopback, multicast, broadcast, link-local,
/// unspecified or reserved).
fn unicast(ip: Ipv4Addr) -> bool {
    !(ip.is_loopback()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.octets()[0] == 0
        || ip.octets()[0] >= 240)
}

/// Smallest MTU accepted (IPv4 minimum datagram a host must handle; below it nothing works).
pub const MTU_MIN: u32 = 576;

/// Checks and normalizes one interface's settings. `max_mtu` is the driver's limit.
/// A refused interface setting: the field of `InterfaceConfig` it is about (its JSON name)
/// and an English message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    pub field: &'static str,
    pub message: String,
}

impl FieldError {
    pub(crate) fn at(field: &'static str, message: String) -> Self {
        Self { field, message }
    }
}

impl std::fmt::Display for FieldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub fn check_interface(
    name: &str,
    c: &InterfaceConfig,
    max_mtu: Option<u32>,
) -> Result<InterfaceConfig, FieldError> {
    // a bond's own settings are checked against the ports by `bond::check`
    let mut out = InterfaceConfig {
        ipv4: c.ipv4,
        bond: c.bond.clone(),
        ..Default::default()
    };
    match c.ipv4 {
        Ipv4Method::Static => {
            if c.addresses.is_empty() {
                return Err(FieldError::at(
                    "addresses",
                    format!("{name}: a static configuration needs an address"),
                ));
            }
            if c.addresses.len() > 16 {
                return Err(FieldError::at(
                    "addresses",
                    format!("{name}: at most 16 addresses"),
                ));
            }
            let mut seen = Vec::new();
            for a in &c.addresses {
                let cidr: Cidr = a
                    .trim()
                    .parse()
                    .map_err(|e| FieldError::at("addresses", format!("{name}: {e}")))?;
                if !unicast(cidr.addr) {
                    return Err(FieldError::at(
                        "addresses",
                        format!("{name}: {cidr} is not a usable host address"),
                    ));
                }
                if !(8..=32).contains(&cidr.prefix) {
                    return Err(FieldError::at(
                        "addresses",
                        format!("{name}: {cidr}: the prefix must be 8-32"),
                    ));
                }
                if cidr.prefix <= 30
                    && (cidr.addr == cidr.network() || cidr.addr == cidr.broadcast())
                {
                    return Err(FieldError::at(
                        "addresses",
                        format!("{name}: {cidr} is the network or broadcast address of its subnet"),
                    ));
                }
                if seen.iter().any(|s: &Cidr| s.addr == cidr.addr) {
                    return Err(FieldError::at(
                        "addresses",
                        format!("{name}: {} is listed twice", cidr.addr),
                    ));
                }
                seen.push(cidr);
            }
            out.addresses = seen.iter().map(Cidr::to_string).collect();
            if let Some(g) = c
                .gateway
                .as_deref()
                .map(str::trim)
                .filter(|g| !g.is_empty())
            {
                let gw: Ipv4Addr = g.parse().map_err(|_| {
                    FieldError::at(
                        "gateway",
                        format!("{name}: gateway {g} is not an IPv4 address"),
                    )
                })?;
                if !unicast(gw) {
                    return Err(FieldError::at(
                        "gateway",
                        format!("{name}: gateway {gw} is not a usable host address"),
                    ));
                }
                if seen.iter().any(|s| s.addr == gw) {
                    return Err(FieldError::at(
                        "gateway",
                        format!("{name}: the gateway {gw} is this interface's own address"),
                    ));
                }
                if !seen.iter().any(|s| s.contains(gw)) {
                    return Err(FieldError::at(
                        "gateway",
                        format!(
                            "{name}: the gateway {gw} is outside the subnets of the interface ({})",
                            out.addresses.join(", ")
                        ),
                    ));
                }
                out.gateway = Some(gw.to_string());
            }
        }
        Ipv4Method::Dhcp | Ipv4Method::Disabled => {
            if !c.addresses.is_empty() || c.gateway.as_deref().is_some_and(|g| !g.is_empty()) {
                return Err(FieldError::at(
                    "addresses",
                    format!("{name}: addresses and gateway are only for a static configuration"),
                ));
            }
        }
    }
    if c.dns.len() > 8 {
        return Err(FieldError::at(
            "dns",
            format!("{name}: at most 8 DNS servers"),
        ));
    }
    for d in &c.dns {
        let ip: Ipv4Addr = d.trim().parse().map_err(|_| {
            FieldError::at(
                "dns",
                format!("{name}: DNS server {d} is not an IPv4 address"),
            )
        })?;
        if ip.is_multicast() || ip.is_broadcast() || ip.is_unspecified() {
            return Err(FieldError::at(
                "dns",
                format!("{name}: DNS server {ip} is not a unicast address"),
            ));
        }
        if !out.dns.contains(&ip.to_string()) {
            out.dns.push(ip.to_string());
        }
    }
    if let Some(mtu) = c.mtu {
        let max = max_mtu.filter(|m| *m >= MTU_MIN).unwrap_or(9216);
        if !(MTU_MIN..=max).contains(&mtu) {
            return Err(FieldError::at(
                "mtu",
                format!("{name}: MTU {mtu} is out of range ({MTU_MIN}-{max} for this driver)"),
            ));
        }
        out.mtu = Some(mtu);
    }
    Ok(out)
}

/// What validation knows about the other interfaces of the box.
#[derive(Debug, Clone, Default)]
pub struct OtherPort {
    pub name: String,
    /// A port that counts as a management path (physical, or a test port).
    pub port: bool,
    /// Its static IPv4 subnets now (for overlap checks), when rynas does not configure it.
    pub subnets: Vec<Cidr>,
    /// It has a usable (non link-local) IPv4 address now.
    pub has_ipv4: bool,
}

/// Checks a whole candidate configuration: every interface, overlaps, management paths.
/// `config` is what rynas will manage after the change; `others` the interfaces it does not
/// manage. Returns the normalized configuration.
pub fn check_config(
    config: &BTreeMap<String, InterfaceConfig>,
    max_mtu: &dyn Fn(&str) -> Option<u32>,
    is_port: &dyn Fn(&str) -> bool,
    others: &[OtherPort],
) -> Result<BTreeMap<String, InterfaceConfig>, String> {
    let mut out = BTreeMap::new();
    for (name, c) in config {
        if !crate::networkd::valid_iface(name) {
            return Err(format!("{name}: not a valid interface name"));
        }
        out.insert(
            name.clone(),
            check_interface(name, c, max_mtu(name)).map_err(|e| e.message)?,
        );
    }
    // subnets across interfaces
    let mut subnets: Vec<(String, Cidr)> = Vec::new();
    for (name, c) in &out {
        for a in &c.addresses {
            subnets.push((name.clone(), a.parse().expect("normalized")));
        }
    }
    for o in others.iter().filter(|o| !out.contains_key(&o.name)) {
        subnets.extend(o.subnets.iter().map(|s| (o.name.clone(), *s)));
    }
    for (i, (na, a)) in subnets.iter().enumerate() {
        for (nb, b) in &subnets[i + 1..] {
            if a.addr == b.addr {
                return Err(format!("{} is used on both {na} and {nb}", a.addr));
            }
            if na != nb && a.overlaps(b) {
                return Err(format!(
                    "{na} ({a}) and {nb} ({b}) are in the same subnet: the kernel could answer \
                     through either; use one interface per subnet"
                ));
            }
        }
    }
    // management paths: a port (or a bond of ports) that keeps an IPv4 configuration
    let managed_paths = out
        .iter()
        .filter(|(n, c)| (is_port(n) || c.bond.is_some()) && c.ipv4 != Ipv4Method::Disabled)
        .count();
    let other_paths = others
        .iter()
        .filter(|o| o.port && o.has_ipv4 && !out.contains_key(&o.name))
        .count();
    if managed_paths + other_paths == 0 {
        return Err(
            "this would leave no port with an IPv4 address (no management path): keep DHCP or \
             a static address on at least one port"
                .into(),
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(addrs: &[&str], gw: Option<&str>) -> InterfaceConfig {
        InterfaceConfig {
            ipv4: Ipv4Method::Static,
            addresses: addrs.iter().map(|s| s.to_string()).collect(),
            gateway: gw.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn cidr_math() {
        let c: Cidr = "192.0.2.10/24".parse().unwrap();
        assert_eq!(c.network(), Ipv4Addr::new(192, 0, 2, 0));
        assert_eq!(c.broadcast(), Ipv4Addr::new(192, 0, 2, 255));
        assert!(c.contains(Ipv4Addr::new(192, 0, 2, 1)));
        assert!(!c.contains(Ipv4Addr::new(192, 0, 3, 1)));
        assert!(c.overlaps(&"192.0.0.0/16".parse().unwrap()));
        assert!(!c.overlaps(&"198.51.100.0/24".parse().unwrap()));
        assert_eq!(netmask_prefix("255.255.255.0"), Some(24));
        assert_eq!(netmask_prefix("255.255.252.0"), Some(22));
        assert_eq!(netmask_prefix("255.0.255.0"), None);
        assert!("192.0.2.1".parse::<Cidr>().is_err());
        assert!("192.0.2.1/33".parse::<Cidr>().is_err());
    }

    #[test]
    fn interface_rules() {
        let ok =
            check_interface("lan1", &st(&[" 192.0.2.10/24 "], Some("192.0.2.1")), None).unwrap();
        assert_eq!(ok.addresses, vec!["192.0.2.10/24"]);
        for (c, why) in [
            (st(&[], None), "needs an address"),
            (st(&["192.0.2.0/24"], None), "network or broadcast"),
            (st(&["192.0.2.255/24"], None), "network or broadcast"),
            (st(&["127.0.0.2/8"], None), "not a usable"),
            (st(&["169.254.1.1/16"], None), "not a usable"),
            (st(&["224.0.0.1/24"], None), "not a usable"),
            (
                st(&["192.0.2.10/24"], Some("198.51.100.1")),
                "outside the subnets",
            ),
            (st(&["192.0.2.10/24"], Some("192.0.2.10")), "own address"),
            (
                st(&["192.0.2.10/24", "192.0.2.10/25"], None),
                "listed twice",
            ),
            (st(&["10.1.2.3/4"], None), "prefix must be 8-32"),
        ] {
            let e = check_interface("lan1", &c, None).unwrap_err();
            assert!(e.message.contains(why), "{c:?}: {e}");
            assert_eq!(
                e.field,
                if why.contains("subnets") || why.contains("own") {
                    "gateway"
                } else {
                    "addresses"
                }
            );
        }
        // /31 and /32 have no network/broadcast addresses
        assert!(check_interface("lan1", &st(&["192.0.2.0/31"], None), None).is_ok());
        let dhcp_with_addr = InterfaceConfig {
            addresses: vec!["192.0.2.1/24".into()],
            ..Default::default()
        };
        assert!(check_interface("lan1", &dhcp_with_addr, None).is_err());
        let mut mtu = InterfaceConfig {
            mtu: Some(9000),
            ..Default::default()
        };
        assert!(check_interface("lan1", &mtu, Some(9000)).is_ok());
        assert!(
            check_interface("lan1", &mtu, Some(1500))
                .unwrap_err()
                .message
                .contains("out of range")
        );
        mtu.mtu = Some(500);
        assert!(check_interface("lan1", &mtu, None).is_err());
        let dns = InterfaceConfig {
            dns: vec!["1.1.1.1".into(), "1.1.1.1".into(), "9.9.9.9".into()],
            ..Default::default()
        };
        assert_eq!(
            check_interface("x", &dns, None).unwrap().dns,
            vec!["1.1.1.1", "9.9.9.9"]
        );
        let bad_dns = InterfaceConfig {
            dns: vec!["dns.example".into()],
            ..Default::default()
        };
        assert!(check_interface("x", &bad_dns, None).is_err());
    }

    #[test]
    fn config_rules_and_management_paths() {
        let port = |_: &str| true;
        let mtu = |_: &str| None;
        let mut c = BTreeMap::new();
        c.insert("lan1".to_string(), st(&["192.0.2.10/24"], None));
        c.insert("lan2".to_string(), st(&["192.0.2.11/24"], None));
        let e = check_config(&c, &mtu, &port, &[]).unwrap_err();
        assert!(e.contains("same subnet"), "{e}");
        c.insert("lan2".to_string(), st(&["198.51.100.2/24"], None));
        assert!(check_config(&c, &mtu, &port, &[]).is_ok());
        // a foreign interface in the same subnet
        let other = OtherPort {
            name: "lan10g".into(),
            port: true,
            subnets: vec!["198.51.100.9/24".parse().unwrap()],
            has_ipv4: true,
        };
        assert!(check_config(&c, &mtu, &port, std::slice::from_ref(&other)).is_err());
        // zero management paths
        let mut off = BTreeMap::new();
        let disabled = InterfaceConfig {
            ipv4: Ipv4Method::Disabled,
            ..Default::default()
        };
        off.insert("lan1".to_string(), disabled.clone());
        let e = check_config(&off, &mtu, &port, &[]).unwrap_err();
        assert!(e.contains("no management path"), "{e}");
        // ... unless another port still has an address
        let other = OtherPort {
            name: "lan2".into(),
            port: true,
            subnets: vec![],
            has_ipv4: true,
        };
        assert!(check_config(&off, &mtu, &port, &[other]).is_ok());
        // a non-port (a virtual link) is not a management path
        let not_port = |n: &str| n != "lan1";
        let mut only = BTreeMap::new();
        only.insert("lan1".to_string(), InterfaceConfig::default());
        assert!(check_config(&only, &mtu, &not_port, &[]).is_err());
        assert!(check_config(&only, &mtu, &port, &[]).is_ok());
        // invalid names never reach a file name
        let mut bad = BTreeMap::new();
        bad.insert("../etc".to_string(), InterfaceConfig::default());
        assert!(check_config(&bad, &mtu, &port, &[]).is_err());
    }
}
