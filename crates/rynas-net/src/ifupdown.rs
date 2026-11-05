//! ifupdown (`/etc/network/interfaces`): read-only parser, and the equivalent networkd
//! settings for the guided migration ("let rynas manage").
//!
//! Debian installed by d-i (and so synodeb's installed systems) configures the network with
//! ifupdown and predictable `enp*` names. rynas shows those stanzas as they are and changes
//! nothing until the administrator asks for the migration.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rynas_api_types::network::{IfupdownStanza, InterfaceConfig, Ipv4Method, NetOption};

use crate::validate::{Cidr, netmask_prefix};

/// The parsed configuration: stanzas per interface (inet and inet6), in file order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Interfaces {
    pub stanzas: Vec<(String, IfupdownStanza)>,
    /// Files read (the main file and its `source` includes).
    pub files: Vec<String>,
}

impl Interfaces {
    pub fn of(&self, iface: &str) -> Vec<IfupdownStanza> {
        self.stanzas
            .iter()
            .filter(|(n, _)| n == iface)
            .map(|(_, s)| s.clone())
            .collect()
    }

    /// Interfaces configured (anything but the loopback).
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for (n, s) in &self.stanzas {
            if s.method != "loopback" && n != "lo" && !v.contains(n) {
                v.push(n.clone());
            }
        }
        v
    }

    pub fn is_empty(&self) -> bool {
        self.names().is_empty()
    }
}

/// Joins `\` continuation lines and drops comments.
fn logical_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if cur.is_empty() && (line.starts_with('#') || line.is_empty()) {
            continue;
        }
        if let Some(s) = line.strip_suffix('\\') {
            cur.push_str(s);
            cur.push(' ');
            continue;
        }
        cur.push_str(line);
        out.push(std::mem::take(&mut cur));
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// Called for `source` (glob: true) and `source-directory` lines: `(path, text)` of each file.
pub type Include<'a> = dyn FnMut(&str, bool) -> Vec<(String, String)> + 'a;

/// Parses one file. `include` is called for `source` / `source-directory` lines and returns
/// the `(path, text)` of each included file.
pub fn parse_text(
    text: &str,
    file: &str,
    include: &mut Include<'_>,
    out: &mut Interfaces,
    depth: u32,
) {
    out.files.push(file.to_string());
    let mut auto: Vec<String> = Vec::new();
    let mut hotplug: Vec<String> = Vec::new();
    let mut current: Option<usize> = None;
    for line in logical_lines(text) {
        let mut words = line.split_whitespace();
        let Some(first) = words.next() else { continue };
        let rest: Vec<&str> = words.collect();
        match first {
            "auto" => {
                auto.extend(rest.iter().map(|s| s.to_string()));
                current = None;
            }
            "allow-hotplug" | "allow-auto" => {
                let list = if first == "allow-auto" {
                    &mut auto
                } else {
                    &mut hotplug
                };
                list.extend(rest.iter().map(|s| s.to_string()));
                current = None;
            }
            w if w.starts_with("allow-") => current = None,
            "source" | "source-directory" => {
                current = None;
                if depth < 4 {
                    for (p, t) in include(rest.first().copied().unwrap_or(""), first == "source") {
                        parse_text(&t, &p, include, out, depth + 1);
                    }
                }
            }
            "mapping" | "no-auto-down" | "no-scripts" | "rename" => current = None,
            "iface" if rest.len() >= 3 => {
                out.stanzas.push((
                    rest[0].to_string(),
                    IfupdownStanza {
                        family: rest[1].to_string(),
                        method: rest[2].to_string(),
                        file: file.to_string(),
                        ..Default::default()
                    },
                ));
                current = Some(out.stanzas.len() - 1);
            }
            opt => {
                let Some(i) = current else { continue };
                let value = rest.join(" ");
                let s = &mut out.stanzas[i].1;
                match opt {
                    "address" => s.address = Some(value),
                    "netmask" => s.other.push(NetOption {
                        name: "netmask".into(),
                        value,
                    }),
                    "gateway" => s.gateway = Some(value),
                    "dns-nameservers" => s.dns.extend(rest.iter().map(|x| x.to_string())),
                    "dns-search" => s.dns_search.extend(rest.iter().map(|x| x.to_string())),
                    "mtu" => s.mtu = value.parse().ok(),
                    "bond-slaves" | "bond_slaves" | "slaves" => {
                        s.bond_slaves.extend(rest.iter().map(|x| x.to_string()))
                    }
                    "bond-mode" | "bond_mode" => s.bond_mode = Some(value),
                    "bridge_ports" | "bridge-ports" => s
                        .bridge_ports
                        .extend(rest.iter().filter(|x| **x != "none").map(|x| x.to_string())),
                    "vlan-raw-device" | "vlan_raw_device" => s.vlan_raw_device = Some(value),
                    _ => s.other.push(NetOption {
                        name: opt.to_string(),
                        value,
                    }),
                }
            }
        }
    }
    // auto/hotplug may come before or after the stanza, in any file
    for (n, s) in out.stanzas.iter_mut() {
        if auto.contains(n) {
            s.auto = true;
        }
        if hotplug.contains(n) {
            s.hotplug = true;
        }
    }
    // address + netmask → CIDR
    for (_, s) in out.stanzas.iter_mut() {
        if let Some(a) = s.address.clone()
            && !a.contains('/')
        {
            let mask = s
                .other
                .iter()
                .find(|o| o.name == "netmask")
                .map(|o| o.value.clone());
            let prefix = match mask {
                Some(m) if m.contains('.') => netmask_prefix(&m),
                Some(m) => m.parse().ok(),
                None if s.family == "inet" => Some(24),
                None => None,
            };
            if let Some(p) = prefix {
                s.address = Some(format!("{a}/{p}"));
                s.other.retain(|o| o.name != "netmask");
            }
        }
    }
}

/// Reads `/etc/network/interfaces` below `root`, following `source` and `source-directory`.
/// `None` when the file does not exist.
pub fn load(root: &Path) -> Option<Interfaces> {
    let main = rynas_config::files::under_root(root, Path::new(crate::INTERFACES));
    let text = std::fs::read_to_string(&main).ok()?;
    let mut out = Interfaces::default();
    let mut include = |pattern: &str, glob: bool| -> Vec<(String, String)> {
        let abs = if pattern.starts_with('/') {
            PathBuf::from(pattern)
        } else {
            Path::new("/etc/network").join(pattern)
        };
        let (dir, name_glob) = if glob {
            match (abs.parent(), abs.file_name()) {
                (Some(d), Some(f)) => (d.to_path_buf(), f.to_string_lossy().into_owned()),
                _ => return Vec::new(),
            }
        } else {
            // source-directory: every file whose name is [A-Za-z0-9_-]+ (run-parts rules)
            (abs.clone(), String::new())
        };
        let real = rynas_config::files::under_root(root, &dir);
        let mut names: Vec<String> = std::fs::read_dir(&real)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().is_file())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
            .into_iter()
            .filter(|n| {
                if glob {
                    crate::networkd::glob_match(&name_glob, n)
                } else {
                    n.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                }
            })
            .filter_map(|n| {
                let text = std::fs::read_to_string(real.join(&n)).ok()?;
                Some((dir.join(&n).display().to_string(), text))
            })
            .collect()
    };
    parse_text(&text, crate::INTERFACES, &mut include, &mut out, 0);
    Some(out)
}

/// The networkd settings equivalent to the ifupdown stanzas (same addresses), or why the
/// migration is not possible in phase 1.
pub fn to_networkd(ifs: &Interfaces) -> Result<BTreeMap<String, InterfaceConfig>, String> {
    let mut out: BTreeMap<String, InterfaceConfig> = BTreeMap::new();
    for (name, s) in &ifs.stanzas {
        if s.method == "loopback" || name == "lo" {
            continue;
        }
        if !s.bond_slaves.is_empty()
            || s.bond_mode.is_some()
            || !s.bridge_ports.is_empty()
            || s.vlan_raw_device.is_some()
            || name.contains('.')
        {
            return Err(format!(
                "{name}: bonds, bridges and VLANs are migrated in a later version; keep ifupdown for now"
            ));
        }
        if !crate::networkd::valid_iface(name) {
            return Err(format!("{name}: interface name not supported"));
        }
        match (s.family.as_str(), s.method.as_str()) {
            ("inet6", "auto" | "dhcp" | "manual") => continue,
            ("inet6", m) => {
                return Err(format!(
                    "{name}: IPv6 method {m} is not migrated yet (IPv6 settings come in a later version)"
                ));
            }
            ("inet", _) => {}
            (f, _) => return Err(format!("{name}: address family {f} not supported")),
        }
        let c = match s.method.as_str() {
            "dhcp" => InterfaceConfig::default(),
            "static" => {
                let addr = s
                    .address
                    .clone()
                    .ok_or_else(|| format!("{name}: static stanza without address"))?;
                addr.parse::<Cidr>().map_err(|e| format!("{name}: {e}"))?;
                InterfaceConfig {
                    ipv4: Ipv4Method::Static,
                    addresses: vec![addr],
                    gateway: s.gateway.clone(),
                    ..Default::default()
                }
            }
            "manual" => InterfaceConfig {
                ipv4: Ipv4Method::Disabled,
                ..Default::default()
            },
            m => return Err(format!("{name}: method {m} is not migrated")),
        };
        let mut c = c;
        c.dns = s.dns.clone();
        c.mtu = s.mtu;
        if let Some(prev) = out.get_mut(name) {
            // a second inet stanza adds addresses (ifupdown allows it)
            prev.addresses.extend(c.addresses);
            if prev.gateway.is_none() {
                prev.gateway = c.gateway;
            }
        } else {
            out.insert(name.clone(), c);
        }
    }
    if out.is_empty() {
        return Err("no interface configured in /etc/network/interfaces".into());
    }
    Ok(out)
}

/// What `/etc/network/interfaces` looks like after the migration: only the loopback (the
/// original is kept as `interfaces.rynas-bak`).
pub const LOOPBACK_ONLY: &str = "# The network is configured by rynas (systemd-networkd, /etc/systemd/network/05-rynas-*).\n\
# The previous ifupdown configuration is kept in /etc/network/interfaces.rynas-bak.\n\
source /etc/network/interfaces.d/*\n\
\n\
auto lo\n\
iface lo inet loopback\n";

#[cfg(test)]
mod tests {
    use super::*;

    const DI: &str = "# This file describes the network interfaces available on your system\n\
source /etc/network/interfaces.d/*\n\
\n\
# The loopback network interface\n\
auto lo\n\
iface lo inet loopback\n\
\n\
# The primary network interface\n\
allow-hotplug enp6s0\n\
iface enp6s0 inet static\n\
\taddress 192.0.2.37/24\n\
\tgateway 192.0.2.1\n\
\t# dns-* options are implemented by the resolvconf package, if installed\n\
\tdns-nameservers 192.0.2.53 1.1.1.1\n\
\tdns-search example.lan\n\
iface enp6s0 inet6 auto\n\
\n\
allow-hotplug enp7s0\n\
iface enp7s0 inet dhcp\n";

    fn parse(text: &str) -> Interfaces {
        let mut out = Interfaces::default();
        parse_text(
            text,
            "/etc/network/interfaces",
            &mut |_, _| Vec::new(),
            &mut out,
            0,
        );
        out
    }

    #[test]
    fn parses_the_debian_installer_file() {
        let i = parse(DI);
        assert_eq!(i.names(), vec!["enp6s0", "enp7s0"]);
        let s = &i.of("enp6s0")[0];
        assert_eq!(s.method, "static");
        assert!(s.hotplug && !s.auto);
        assert_eq!(s.address.as_deref(), Some("192.0.2.37/24"));
        assert_eq!(s.gateway.as_deref(), Some("192.0.2.1"));
        assert_eq!(s.dns, vec!["192.0.2.53", "1.1.1.1"]);
        assert_eq!(s.dns_search, vec!["example.lan"]);
        assert_eq!(i.of("enp6s0")[1].family, "inet6");
        assert!(i.of("lo")[0].auto);
        let n = to_networkd(&i).unwrap();
        assert_eq!(n["enp6s0"].ipv4, Ipv4Method::Static);
        assert_eq!(n["enp6s0"].addresses, vec!["192.0.2.37/24"]);
        assert_eq!(n["enp6s0"].gateway.as_deref(), Some("192.0.2.1"));
        assert_eq!(n["enp6s0"].dns, vec!["192.0.2.53", "1.1.1.1"]);
        assert_eq!(n["enp7s0"], InterfaceConfig::default());
    }

    #[test]
    fn netmask_and_continuations() {
        let i = parse(
            "auto eth0\niface eth0 inet static\n address 10.1.2.3\n netmask 255.255.0.0\n mtu 9000\n \
             up ip route add 10.9.0.0/16 \\\n   via 10.1.0.1\n",
        );
        let s = &i.of("eth0")[0];
        assert_eq!(s.address.as_deref(), Some("10.1.2.3/16"));
        assert_eq!(s.mtu, Some(9000));
        assert!(s.auto);
        assert_eq!(s.other[0].name, "up");
        assert_eq!(s.other[0].value, "ip route add 10.9.0.0/16 via 10.1.0.1");
    }

    #[test]
    fn bonds_bridges_vlans_are_shown_but_not_migrated() {
        let i = parse(
            "auto bond0\niface bond0 inet manual\n bond-slaves enp6s0 enp7s0\n bond-mode 802.3ad\n\
             auto br0\niface br0 inet static\n address 192.0.2.5/24\n bridge_ports bond0\n\
             auto bond0.10\niface bond0.10 inet manual\n vlan-raw-device bond0\n",
        );
        assert_eq!(i.of("bond0")[0].bond_slaves, vec!["enp6s0", "enp7s0"]);
        assert_eq!(i.of("bond0")[0].bond_mode.as_deref(), Some("802.3ad"));
        assert_eq!(i.of("br0")[0].bridge_ports, vec!["bond0"]);
        assert_eq!(
            i.of("bond0.10")[0].vlan_raw_device.as_deref(),
            Some("bond0")
        );
        assert!(to_networkd(&i).unwrap_err().contains("later version"));
        let v6 = parse("iface eth0 inet6 static\n address 2001:db8::5/64\n");
        assert!(to_networkd(&v6).is_err());
        assert!(to_networkd(&parse("auto lo\niface lo inet loopback\n")).is_err());
    }

    #[test]
    fn follows_source_includes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("etc/network/interfaces.d")).unwrap();
        std::fs::write(root.join("etc/network/interfaces"), DI).unwrap();
        std::fs::write(
            root.join("etc/network/interfaces.d/lan10g"),
            "auto enp1s0\niface enp1s0 inet static\n address 198.51.100.1/24\n",
        )
        .unwrap();
        let i = load(root).unwrap();
        assert_eq!(i.names(), vec!["enp1s0", "enp6s0", "enp7s0"]);
        assert_eq!(i.of("enp1s0")[0].file, "/etc/network/interfaces.d/lan10g");
        assert_eq!(i.files.len(), 2);
        assert!(load(&root.join("nowhere")).is_none());
    }
}
