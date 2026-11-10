//! systemd-networkd files: the ones rynas writes (`/etc/systemd/network/05-rynas-<iface>.network`)
//! and a reader for foreign ones.
//!
//! networkd applies to a link the **first** `.network` file that matches, in lexical order of
//! the file names across `/etc`, `/run` and `/usr/lib`. rynas's files therefore start with `05-`
//! (the research note proposed `30-`): they must win over the files of the synodeb image
//! (`10-lan10g.network`, `20-lan.network`, `99-fallback.network`), which are kept untouched.
//!
//! Every rynas file keeps the rescue path on: `LinkLocalAddressing=yes` with a deterministic
//! `IPv4LLStartAddress=` derived from the MAC ([`link_local_for_mac`]), next to DHCP or the
//! static addresses.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use rynas_api_types::network::{InterfaceConfig, Ipv4Method};

use crate::NETWORKD_DIR;

/// Prefix of every `.network` file rynas owns.
pub const RYNAS_PREFIX: &str = "05-rynas-";

/// `/etc/systemd/network/05-rynas-<iface>.network`.
pub fn network_path(iface: &str) -> PathBuf {
    Path::new(NETWORKD_DIR).join(format!("{RYNAS_PREFIX}{iface}.network"))
}

/// Whether a file name is one of rynas's (`.network`, or a bond's `.netdev`).
pub fn is_rynas_file(name: &str) -> bool {
    name.starts_with(RYNAS_PREFIX) && (name.ends_with(".network") || name.ends_with(".netdev"))
}

/// The interface a rynas file configures.
pub fn iface_of_file(name: &str) -> Option<&str> {
    name.strip_prefix(RYNAS_PREFIX)?.strip_suffix(".network")
}

/// Kernel interface names rynas accepts (they reach file names and argv).
pub fn valid_iface(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 15
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'@'))
        && !name.starts_with(['-', '.'])
        && name != "lo"
}

/// Parses `aa:bb:cc:dd:ee:ff`.
pub fn parse_mac(mac: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = mac.split(':').collect();
    if parts.len() != 6 {
        return None;
    }
    let mut out = [0u8; 6];
    for (o, p) in out.iter_mut().zip(parts) {
        *o = u8::from_str_radix(p, 16).ok()?;
    }
    Some(out)
}

/// The deterministic IPv4 link-local start address of a port: `169.254.x.y` with
/// `x = 1 + (mac[3] ^ mac[4]) % 254` (169.254.0.0/24 and 169.254.255.0/24 are reserved by
/// RFC 3927) and `y = mac[5]`. Two ports of one box differ in their last byte, so they get
/// different addresses; networkd moves on to another address only on a conflict.
pub fn link_local_for_mac(mac: &str) -> Option<Ipv4Addr> {
    let m = parse_mac(mac)?;
    if m == [0; 6] {
        return None;
    }
    let x = 1 + (m[3] ^ m[4]) % 254;
    Some(Ipv4Addr::new(169, 254, x, m[5]))
}

/// The content (without the managed header) of the file for `iface`.
pub fn render_network(iface: &str, cfg: &InterfaceConfig, mac: Option<&str>) -> String {
    let mut s = String::new();
    s.push_str(
        "# rynas: network settings of this port; change them from the rynas web interface.\n",
    );
    s.push_str(&format!("[Match]\nName={iface}\n"));
    if let Some(mtu) = cfg.mtu {
        s.push_str(&format!("\n[Link]\nMTUBytes={mtu}\n"));
    }
    s.push_str("\n[Network]\n");
    match cfg.ipv4 {
        Ipv4Method::Dhcp => s.push_str("DHCP=ipv4\n"),
        Ipv4Method::Static | Ipv4Method::Disabled => {}
    }
    s.push_str("# rescue path: always reachable on a direct cable (link-local, IPv4 and IPv6)\n");
    s.push_str("LinkLocalAddressing=yes\n");
    if let Some(ll) = mac.and_then(link_local_for_mac) {
        s.push_str(&format!("IPv4LLStartAddress={ll}\n"));
    }
    if cfg.ipv4 == Ipv4Method::Static {
        for a in &cfg.addresses {
            s.push_str(&format!("Address={a}\n"));
        }
        if let Some(g) = &cfg.gateway {
            s.push_str(&format!("Gateway={g}\n"));
        }
    }
    for d in &cfg.dns {
        s.push_str(&format!("DNS={d}\n"));
    }
    if cfg.ipv4 == Ipv4Method::Dhcp {
        // a NAS keeps the name it was given; the lease must not rename it
        s.push_str("\n[DHCPv4]\nUseHostname=no\n");
    }
    s
}


/// The DHCPv4 client identifier the DHCP client before rynas sent, so the server keeps
/// giving the NAS the same address after the takeover (networkd's own default, a DUID from
/// the machine id with an IAID from the interface name, is a new client to the server).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DhcpIdentity {
    /// Hardware type 1 and the MAC address (`ClientIdentifier=mac`).
    Mac,
    /// RFC 4361: an IAID and a DUID (type in its first two bytes).
    Duid { duid: Vec<u8>, iaid: u32 },
}

/// Lines of a client configuration without comments and blank lines.
fn conf_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
}

/// What dhcpcd sent, from `dhcpcd.conf` and its DUID file. Only the global options count;
/// a configuration it cannot read for sure (an `iaid`, options per interface) gives `None`.
/// dhcpcd's IAID is the last four bytes of the hardware address.
pub fn dhcpcd_identity(conf: &str, duid_file: Option<&str>, mac: Option<&str>) -> Option<DhcpIdentity> {
    let mut duid = false;
    let mut clientid = false;
    for l in conf_lines(conf) {
        let mut words = l.split_whitespace();
        let key = words.next().unwrap_or("");
        let bare = words.next().is_none();
        match key {
            "interface" | "ssid" | "profile" | "iaid" | "anonymous" => return None,
            // `duid ll` and the like: another DUID than the file's; `clientid <x>` sends <x>
            "duid" | "clientid" if !bare => return None,
            "duid" => duid = true,
            "clientid" => clientid = true,
            _ => {}
        }
    }
    if duid {
        let bytes: Vec<u8> = duid_file?
            .trim()
            .split(':')
            .map(|b| u8::from_str_radix(b, 16).ok())
            .collect::<Option<_>>()?;
        if bytes.len() < 3 || bytes.len() > 130 {
            return None;
        }
        let m = parse_mac(mac?)?;
        Some(DhcpIdentity::Duid { duid: bytes, iaid: u32::from_be_bytes([m[2], m[3], m[4], m[5]]) })
    } else if clientid {
        Some(DhcpIdentity::Mac)
    } else {
        None
    }
}

/// What ISC dhclient sent: no client identifier unless told (the server then knows it by
/// its MAC, which `ClientIdentifier=mac` keeps), or the hardware address.
pub fn dhclient_identity(conf: Option<&str>) -> Option<DhcpIdentity> {
    let custom = conf.into_iter().flat_map(conf_lines).find(|l| l.contains("dhcp-client-identifier"));
    match custom {
        None => Some(DhcpIdentity::Mac),
        Some(l) if l.trim_end_matches(';').trim().ends_with("= hardware") => Some(DhcpIdentity::Mac),
        Some(_) => None,
    }
}

/// The drop-in next to the port's rynas file that keeps the previous client's identity.
pub fn dhcp_identity_path(iface: &str) -> PathBuf {
    Path::new(NETWORKD_DIR)
        .join(format!("{RYNAS_PREFIX}{iface}.network.d"))
        .join("10-dhcp-identity.conf")
}

/// Its content: the identifier and, when the port had a lease, that address asked for again.
pub fn render_dhcp_identity(id: &DhcpIdentity, client: &str, address: Option<Ipv4Addr>) -> String {
    let mut s = format!(
        "# rynas: the DHCP identity {client} used before rynas took over this port, so the\n# server keeps giving the same address.\n[DHCPv4]\n"
    );
    match id {
        DhcpIdentity::Mac => s.push_str("ClientIdentifier=mac\n"),
        DhcpIdentity::Duid { duid, iaid } => {
            let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(":");
            s.push_str("ClientIdentifier=duid\n");
            s.push_str(&format!("IAID={iaid}\n"));
            s.push_str(&format!("DUIDType={}\n", u16::from_be_bytes([duid[0], duid[1]])));
            s.push_str(&format!("DUIDRawData={}\n", hex(&duid[2..])));
        }
    }
    if let Some(a) = address {
        s.push_str(&format!("RequestAddress={a}\n"));
    }
    s
}

/// The identity networkd itself used for a lease, from its lease file
/// (`/run/systemd/netif/leases/<ifindex>`: `CLIENTID=` in hex, `ADDRESS=`): type 1 is the
/// MAC (`ClientIdentifier=mac`), type 255 an IAID and a DUID (RFC 4361). Other identifiers
/// give `None`.
pub fn lease_identity(lease: &str) -> Option<(DhcpIdentity, Option<Ipv4Addr>)> {
    let mut id = None;
    let mut addr = None;
    for l in lease.lines() {
        if let Some(v) = l.strip_prefix("CLIENTID=") {
            let v = v.trim();
            if v.len() % 2 != 0 {
                return None;
            }
            let bytes: Vec<u8> = (0..v.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&v[i..i + 2], 16).ok())
                .collect::<Option<_>>()?;
            id = Some(bytes);
        } else if let Some(v) = l.strip_prefix("ADDRESS=") {
            addr = v.trim().parse().ok();
        }
    }
    let id = id?;
    let ident = match id.first()? {
        1 if id.len() == 7 => DhcpIdentity::Mac,
        255 if id.len() >= 8 && id.len() <= 5 + 130 => DhcpIdentity::Duid {
            iaid: u32::from_be_bytes([id[1], id[2], id[3], id[4]]),
            duid: id[5..].to_vec(),
        },
        _ => return None,
    };
    Some((ident, addr))
}

/// A parsed INI-style systemd file: section → (key, value) in order.
#[derive(Debug, Default, Clone)]
pub struct UnitFile {
    pub sections: Vec<(String, Vec<(String, String)>)>,
}

impl UnitFile {
    pub fn parse(text: &str) -> Self {
        let mut f = UnitFile::default();
        let mut cont = String::new();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            // line continuation
            if let Some(stripped) = line.strip_suffix('\\') {
                cont.push_str(stripped);
                cont.push(' ');
                continue;
            }
            let full = if cont.is_empty() {
                line.to_string()
            } else {
                let v = format!("{cont}{line}");
                cont.clear();
                v
            };
            if full.starts_with('[') && full.ends_with(']') {
                f.sections
                    .push((full[1..full.len() - 1].trim().to_string(), Vec::new()));
            } else if let Some((k, v)) = full.split_once('=')
                && let Some(sec) = f.sections.last_mut()
            {
                sec.1.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        f
    }

    /// Every value of `key` in `section` (all sections of that name), in order.
    pub fn values(&self, section: &str, key: &str) -> Vec<&str> {
        self.sections
            .iter()
            .filter(|(s, _)| s == section)
            .flat_map(|(_, kv)| kv.iter())
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// The last value of `key` (later assignments win).
    pub fn last(&self, section: &str, key: &str) -> Option<&str> {
        self.values(section, key).last().copied()
    }

    /// Space-separated list values (`Name=eth0 eth1`), an empty assignment resets the list.
    pub fn list(&self, section: &str, key: &str) -> Vec<String> {
        let mut out = Vec::new();
        for v in self.values(section, key) {
            if v.is_empty() {
                out.clear();
            } else {
                out.extend(v.split_whitespace().map(str::to_string));
            }
        }
        out
    }
}

/// Shell-style glob of networkd's `Name=` (`*`, `?`, `[...]` and a leading `!`).
pub fn glob_match(pattern: &str, name: &str) -> bool {
    fn m(p: &[u8], n: &[u8]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => m(&p[1..], n) || (!n.is_empty() && m(p, &n[1..])),
            (Some(b'?'), Some(_)) => m(&p[1..], &n[1..]),
            (Some(b'['), Some(&c)) => {
                let Some(end) = p.iter().position(|&b| b == b']') else {
                    return false;
                };
                let set = &p[1..end];
                let (neg, set) = match set.first() {
                    Some(b'!' | b'^') => (true, &set[1..]),
                    _ => (false, set),
                };
                let mut hit = false;
                let mut i = 0;
                while i < set.len() {
                    if i + 2 < set.len() && set[i + 1] == b'-' {
                        hit |= set[i] <= c && c <= set[i + 2];
                        i += 3;
                    } else {
                        hit |= set[i] == c;
                        i += 1;
                    }
                }
                hit != neg && m(&p[end + 1..], &n[1..])
            }
            (Some(a), Some(b)) if a == b => m(&p[1..], &n[1..]),
            _ => false,
        }
    }
    match pattern.strip_prefix('!') {
        Some(p) => !m(p.as_bytes(), name.as_bytes()),
        None => m(pattern.as_bytes(), name.as_bytes()),
    }
}

/// What a `.network` file says, as far as rynas shows it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NetworkFile {
    pub names: Vec<String>,
    pub macs: Vec<String>,
    pub types: Vec<String>,
    pub dhcp: Option<String>,
    pub addresses: Vec<String>,
    pub gateways: Vec<String>,
    pub dns: Vec<String>,
    pub mtu: Option<u32>,
    pub link_local: Option<String>,
    /// `Bond=`: the link is a member of this bond.
    pub bond: Option<String>,
    /// `PrimarySlave=yes`.
    pub primary_slave: bool,
}

impl NetworkFile {
    pub fn parse(text: &str) -> Self {
        let u = UnitFile::parse(text);
        let mut addresses: Vec<String> = u.list("Network", "Address");
        for (s, kv) in &u.sections {
            if s == "Address" {
                for (k, v) in kv {
                    if k == "Address" {
                        addresses.push(v.clone());
                    }
                }
            }
        }
        let mut gateways = u.list("Network", "Gateway");
        for (s, kv) in &u.sections {
            if s == "Route" {
                let dst = kv.iter().find(|(k, _)| k == "Destination").map(|(_, v)| v);
                if dst.is_none_or(|d| d == "0.0.0.0/0" || d == "::/0") {
                    gateways.extend(
                        kv.iter()
                            .filter(|(k, _)| k == "Gateway")
                            .map(|(_, v)| v.clone()),
                    );
                }
            }
        }
        NetworkFile {
            names: u.list("Match", "Name"),
            macs: u.list("Match", "MACAddress"),
            types: u.list("Match", "Type"),
            dhcp: u.last("Network", "DHCP").map(str::to_string),
            addresses,
            gateways,
            dns: u.list("Network", "DNS"),
            mtu: u.last("Link", "MTUBytes").and_then(|v| v.parse().ok()),
            link_local: u.last("Network", "LinkLocalAddressing").map(str::to_string),
            bond: u.last("Network", "Bond").map(str::to_string).filter(|b| !b.is_empty()),
            primary_slave: matches!(
                u.last("Network", "PrimarySlave"),
                Some("yes" | "true" | "on" | "1")
            ),
        }
    }

    /// Whether the `[Match]` section selects this link (name, MAC, type; the other match keys
    /// are not evaluated and count as matching).
    pub fn matches(&self, name: &str, mac: Option<&str>, kind: Option<&str>) -> bool {
        let names_ok = self.names.is_empty() || {
            let (neg, pos): (Vec<&String>, Vec<&String>) =
                self.names.iter().partition(|p| p.starts_with('!'));
            (pos.is_empty() || pos.iter().any(|p| glob_match(p, name)))
                && neg.iter().all(|p| glob_match(p, name))
        };
        let mac_ok = self.macs.is_empty()
            || mac.is_some_and(|m| self.macs.iter().any(|x| x.eq_ignore_ascii_case(m)));
        let type_ok = self.types.is_empty()
            || kind.is_some_and(|k| self.types.iter().any(|t| glob_match(t, k)));
        names_ok && mac_ok && type_ok
    }

    /// The settings it gives an interface, in rynas's model (IPv4 only).
    pub fn to_config(&self) -> InterfaceConfig {
        let v4: Vec<String> = self
            .addresses
            .iter()
            .filter(|a| a.parse::<crate::validate::Cidr>().is_ok())
            .cloned()
            .collect();
        let dhcp = matches!(
            self.dhcp.as_deref(),
            Some("yes" | "ipv4" | "true" | "on" | "1")
        );
        InterfaceConfig {
            ipv4: if dhcp {
                Ipv4Method::Dhcp
            } else if v4.is_empty() {
                Ipv4Method::Disabled
            } else {
                Ipv4Method::Static
            },
            addresses: if dhcp { Vec::new() } else { v4 },
            gateway: self
                .gateways
                .iter()
                .find(|g| g.parse::<Ipv4Addr>().is_ok())
                .cloned(),
            dns: self
                .dns
                .iter()
                .filter(|d| d.parse::<Ipv4Addr>().is_ok())
                .cloned()
                .collect(),
            mtu: self.mtu,
            ..Default::default()
        }
    }
}

/// `.network` files of every search directory, by file name (the first directory wins for a
/// name, like networkd: `/etc` over `/run` over `/usr/lib`), sorted by name. Paths are the
/// real ones (below the root).
pub fn network_files(root: &Path) -> Vec<(String, PathBuf)> {
    let mut by_name: BTreeMap<String, PathBuf> = BTreeMap::new();
    for dir in [
        "/etc/systemd/network",
        "/run/systemd/network",
        "/usr/local/lib/systemd/network",
        "/usr/lib/systemd/network",
        "/lib/systemd/network",
    ] {
        let real = rynas_config::files::under_root(root, Path::new(dir));
        let Ok(rd) = std::fs::read_dir(&real) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".network") {
                continue;
            }
            by_name.entry(name).or_insert_with(|| e.path());
        }
    }
    by_name.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previous_dhcp_identity() {
        // Debian's dhcpcd.conf: DUID + IAID from the last four bytes of the MAC
        let conf = "hostname\nduid\npersistent\nvendorclassid\noption host_name\nslaac private\n";
        let duid = "00:01:00:01:32:85:aa:ac:02:00:5e:10:20:30\n";
        let id = dhcpcd_identity(conf, Some(duid), Some("02:00:5e:10:20:30")).unwrap();
        assert_eq!(
            id,
            DhcpIdentity::Duid {
                duid: vec![0, 1, 0, 1, 0x32, 0x85, 0xaa, 0xac, 2, 0, 0x5e, 0x10, 0x20, 0x30],
                iaid: 0x5e10_2030
            }
        );
        let text = render_dhcp_identity(&id, "dhcpcd", Some(Ipv4Addr::new(192, 0, 2, 20)));
        assert!(text.contains("[DHCPv4]\nClientIdentifier=duid\nIAID=1578115120\nDUIDType=1\n"), "{text}");
        assert!(text.contains("DUIDRawData=00:01:32:85:aa:ac:02:00:5e:10:20:30\nRequestAddress=192.0.2.20\n"), "{text}");
        // no DUID file or MAC: unknown, not guessed
        assert_eq!(dhcpcd_identity(conf, None, Some("02:00:5e:10:20:30")), None);
        assert_eq!(dhcpcd_identity(conf, Some(duid), None), None);
        // the MAC as client id; options per interface or a chosen IAID are not guessed
        assert_eq!(dhcpcd_identity("clientid\n", None, None), Some(DhcpIdentity::Mac));
        assert_eq!(dhcpcd_identity("duid\ninterface eth0\n", Some(duid), Some("02:00:5e:10:20:30")), None);
        assert_eq!(dhcpcd_identity("duid\niaid 1\n", Some(duid), Some("02:00:5e:10:20:30")), None);
        assert_eq!(dhcpcd_identity("# duid\n", Some(duid), Some("02:00:5e:10:20:30")), None);
        assert_eq!(dhcpcd_identity("clientid nas-1\n", None, None), None);
        assert_eq!(dhcpcd_identity("duid ll\n", Some(duid), Some("02:00:5e:10:20:30")), None);
        // dhclient: none sent (known by MAC) or the hardware address; anything else unknown
        assert_eq!(dhclient_identity(None), Some(DhcpIdentity::Mac));
        assert_eq!(dhclient_identity(Some("#send dhcp-client-identifier 1:0:a0:24:ab:fb:9c;\n")), Some(DhcpIdentity::Mac));
        assert_eq!(dhclient_identity(Some("send dhcp-client-identifier = hardware;\n")), Some(DhcpIdentity::Mac));
        assert_eq!(dhclient_identity(Some("send dhcp-client-identifier \"nas\";\n")), None);
        assert_eq!(render_dhcp_identity(&DhcpIdentity::Mac, "dhclient", None).lines().last(), Some("ClientIdentifier=mac"));
    }

    #[test]
    fn link_local_is_deterministic_and_valid() {
        let a = link_local_for_mac("00:11:32:ab:cd:01").unwrap();
        let b = link_local_for_mac("00:11:32:ab:cd:02").unwrap();
        assert_eq!(a, link_local_for_mac("00:11:32:AB:CD:01").unwrap());
        assert_ne!(a, b);
        for m in [
            "00:00:00:00:00:01",
            "ff:ff:ff:ff:ff:ff",
            "00:11:32:ff:00:ff",
        ] {
            let o = link_local_for_mac(m).unwrap().octets();
            assert_eq!((o[0], o[1]), (169, 254));
            assert!((1..=254).contains(&o[2]), "{m}: {o:?}");
        }
        assert_eq!(link_local_for_mac("00:00:00:00:00:00"), None);
        assert_eq!(link_local_for_mac("garbage"), None);
    }

    #[test]
    fn renders_static_and_dhcp_files() {
        let st = InterfaceConfig {
            ipv4: Ipv4Method::Static,
            addresses: vec!["192.0.2.10/24".into()],
            gateway: Some("192.0.2.1".into()),
            dns: vec!["192.0.2.53".into()],
            mtu: Some(9000),
            ..Default::default()
        };
        let text = render_network("lan1", &st, Some("00:11:32:ab:cd:01"));
        assert_eq!(
            text,
            "# rynas: network settings of this port; change them from the rynas web interface.\n\
             [Match]\nName=lan1\n\n[Link]\nMTUBytes=9000\n\n[Network]\n\
             # rescue path: always reachable on a direct cable (link-local, IPv4 and IPv6)\n\
             LinkLocalAddressing=yes\nIPv4LLStartAddress=169.254.103.1\n\
             Address=192.0.2.10/24\nGateway=192.0.2.1\nDNS=192.0.2.53\n"
        );
        let dhcp = InterfaceConfig::default();
        let text = render_network("enp6s0", &dhcp, None);
        assert_eq!(
            text,
            "# rynas: network settings of this port; change them from the rynas web interface.\n\
             [Match]\nName=enp6s0\n\n[Network]\nDHCP=ipv4\n\
             # rescue path: always reachable on a direct cable (link-local, IPv4 and IPv6)\n\
             LinkLocalAddressing=yes\n\n[DHCPv4]\nUseHostname=no\n"
        );
        // what it renders reads back as the same settings
        assert_eq!(
            NetworkFile::parse(&render_network("x", &st, None)).to_config(),
            st
        );
        assert_eq!(
            NetworkFile::parse(&render_network("x", &dhcp, None)).to_config(),
            dhcp
        );
    }

    #[test]
    fn reads_foreign_files_of_the_synodeb_image() {
        let f = NetworkFile::parse(
            "[Match]\nName=lan1 lan2\n\n[Network]\nDHCP=yes\nLinkLocalAddressing=ipv6\n",
        );
        assert!(f.matches("lan1", None, None) && f.matches("lan2", None, None));
        assert!(!f.matches("lan10g", None, None));
        assert_eq!(f.to_config().ipv4, Ipv4Method::Dhcp);
        let f = NetworkFile::parse(
            "[Match]\nName=lan10g\n[Network]\nAddress=192.0.2.1/24\n[Route]\nGateway=192.0.2.254\n[Link]\nMTUBytes=9000\n",
        );
        let c = f.to_config();
        assert_eq!(c.addresses, vec!["192.0.2.1/24"]);
        assert_eq!(c.gateway.as_deref(), Some("192.0.2.254"));
        assert_eq!(c.mtu, Some(9000));
        let fb = NetworkFile::parse("[Match]\nName=en* eth*\n[Network]\nDHCP=yes\n");
        assert!(fb.matches("enp6s0", None, None) && fb.matches("eth0", None, None));
        assert!(!fb.matches("wlan0", None, None));
        let neg = NetworkFile::parse("[Match]\nName=!veth*\nType=ether\n");
        assert!(
            neg.matches("eth0", None, Some("ether")) && !neg.matches("veth1", None, Some("ether"))
        );
        assert!(!neg.matches("eth0", None, Some("loopback")));
    }

    #[test]
    fn globs() {
        assert!(glob_match("en*", "enp6s0"));
        assert!(glob_match("lan[12]", "lan2"));
        assert!(!glob_match("lan[12]", "lan3"));
        assert!(glob_match("eth?", "eth0"));
        assert!(glob_match("lan[0-9]", "lan7"));
        assert!(!glob_match("!en*", "enp1s0"));
    }

    #[test]
    fn file_names() {
        assert_eq!(
            network_path("lan1"),
            Path::new("/etc/systemd/network/05-rynas-lan1.network")
        );
        assert!(is_rynas_file("05-rynas-lan1.network"));
        assert!(!is_rynas_file("10-lan10g.network"));
        assert_eq!(iface_of_file("05-rynas-enp6s0.network"), Some("enp6s0"));
        assert!(valid_iface("enp6s0") && valid_iface("lan1") && valid_iface("bond0.10"));
        assert!(
            !valid_iface("lo") && !valid_iface("../x") && !valid_iface("a b") && !valid_iface("")
        );
        assert!(!valid_iface("abcdefghijklmnop"));
    }
}
