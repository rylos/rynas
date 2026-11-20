//! Magic packets: the payload, the NAS interfaces a wake goes out on, and the sockets
//! (`docs/research/wake-on-lan.md` §2, decision 2).
//!
//! Per interface a raw Ethernet frame (EtherType `0x0842`, `AF_PACKET`, destination
//! `ff:ff:ff:ff:ff:ff`, source the interface's MAC) and a UDP datagram to the interface's subnet
//! broadcast on the device's port (`SO_BROADCAST` + `SO_BINDTODEVICE`): `255.255.255.255`
//! would leave by the default route's interface only.

use std::net::Ipv4Addr;

use serde::Deserialize;

/// EtherType of Wake-on-LAN frames.
pub const ETHERTYPE_WOL: u16 = 0x0842;

/// One NAS interface as `ip -j -d addr show` reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NasIface {
    pub name: String,
    pub index: u32,
    /// `aa:bb:cc:dd:ee:ff`, lower case (none for tunnels).
    pub mac: Option<String>,
    /// A bond port's own MAC (its `address` is the bond's).
    pub perm_mac: Option<String>,
    pub up: bool,
    pub broadcast: bool,
    pub loopback: bool,
    /// `bond`, `bridge`, `veth`, `tun`, `wireguard`… (`linkinfo.info_kind`).
    pub kind: Option<String>,
    /// Enslaved to a bond or a bridge: never sent on directly (its master is).
    pub master: Option<String>,
    /// IPv4 addresses and prefix lengths.
    pub ipv4: Vec<(Ipv4Addr, u8)>,
}

impl NasIface {
    /// `a.b.c.d/nn` of each IPv4 address.
    pub fn cidrs(&self) -> Vec<String> {
        self.ipv4.iter().map(|(a, p)| format!("{a}/{p}")).collect()
    }

    /// The interface has an IPv4 subnet holding `ip`.
    pub fn holds(&self, ip: Ipv4Addr) -> bool {
        self.ipv4.iter().any(|(a, p)| same_subnet(*a, ip, *p))
    }
}

#[derive(Deserialize)]
struct IpLink {
    #[serde(default)]
    ifindex: u32,
    #[serde(default)]
    ifname: String,
    #[serde(default)]
    flags: Vec<String>,
    #[serde(default)]
    operstate: Option<String>,
    #[serde(default)]
    master: Option<String>,
    #[serde(default)]
    address: Option<String>,
    #[serde(default)]
    link_type: Option<String>,
    #[serde(default)]
    linkinfo: Option<LinkInfo>,
    #[serde(default)]
    addr_info: Vec<AddrInfo>,
}

#[derive(Deserialize)]
struct LinkInfo {
    #[serde(default)]
    info_kind: Option<String>,
    #[serde(default)]
    info_slave_data: Option<SlaveData>,
}

#[derive(Deserialize)]
struct SlaveData {
    #[serde(default)]
    perm_hwaddr: Option<String>,
}

#[derive(Deserialize)]
struct AddrInfo {
    #[serde(default)]
    family: String,
    #[serde(default)]
    local: String,
    #[serde(default)]
    prefixlen: u8,
}

/// Parses `ip -j -d addr show`.
pub fn parse_ip_addr(json: &str) -> Vec<NasIface> {
    let links: Vec<IpLink> = serde_json::from_str(json).unwrap_or_default();
    links
        .into_iter()
        .filter(|l| !l.ifname.is_empty())
        .map(|l| {
            let has = |f: &str| l.flags.iter().any(|x| x == f);
            let ether = l.link_type.as_deref().is_none_or(|t| t == "ether");
            NasIface {
                up: has("UP") && l.operstate.as_deref() != Some("DOWN"),
                broadcast: has("BROADCAST"),
                loopback: has("LOOPBACK"),
                mac: l.address.as_deref().filter(|_| ether).and_then(rynas_api_types::wol::normalize_mac),
                perm_mac: l
                    .linkinfo
                    .as_ref()
                    .and_then(|i| i.info_slave_data.as_ref())
                    .and_then(|s| s.perm_hwaddr.as_deref())
                    .and_then(rynas_api_types::wol::normalize_mac),
                kind: l.linkinfo.as_ref().and_then(|i| i.info_kind.clone()),
                master: l.master,
                ipv4: l
                    .addr_info
                    .iter()
                    .filter(|a| a.family == "inet")
                    .filter_map(|a| a.local.parse().ok().map(|ip| (ip, a.prefixlen.min(32))))
                    .collect(),
                name: l.ifname,
                index: l.ifindex,
            }
        })
        .collect()
}

/// Interfaces a wake never goes out on by itself: loopback, ports of a bond or bridge (their
/// master is used), VPN and container links.
pub fn eligible(i: &NasIface) -> bool {
    let excluded_name = [
        "tailscale", "docker", "veth", "br-", "virbr", "podman", "cni-", "lxcbr", "incusbr", "wg", "tun", "tap", "zt",
    ]
        .iter()
        .any(|p| i.name.starts_with(p));
    let excluded_kind = i
        .kind
        .as_deref()
        .is_some_and(|k| matches!(k, "veth" | "tun" | "wireguard" | "vxlan" | "ipip" | "gre" | "sit" | "dummy"));
    !i.loopback && i.master.is_none() && !excluded_name && !excluded_kind && i.mac.is_some()
}

/// The prefix-length mask.
fn mask(prefix: u8) -> u32 {
    if prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(prefix.min(32))) }
}

pub fn same_subnet(a: Ipv4Addr, b: Ipv4Addr, prefix: u8) -> bool {
    let m = mask(prefix);
    u32::from(a) & m == u32::from(b) & m
}

/// The subnet broadcast of `ip/prefix` (`/31` and `/32`: the limited broadcast).
pub fn subnet_broadcast(ip: Ipv4Addr, prefix: u8) -> Ipv4Addr {
    if prefix >= 31 {
        return Ipv4Addr::BROADCAST;
    }
    Ipv4Addr::from(u32::from(ip) | !mask(prefix))
}

/// The interfaces a wake goes out on: the fixed one when set (whatever its state, so a
/// wrong choice shows as a send error); else the up, eligible interface whose IPv4 subnet
/// holds `ip`; else every up, eligible, broadcast-capable interface.
pub fn choose<'a>(ifaces: &'a [NasIface], fixed: Option<&str>, ip: Option<Ipv4Addr>) -> Vec<&'a NasIface> {
    if let Some(f) = fixed {
        return ifaces.iter().filter(|i| i.name == f).collect();
    }
    let usable = |i: &&NasIface| i.up && eligible(i);
    if let Some(ip) = ip
        && let Some(i) = ifaces.iter().filter(usable).find(|i| i.holds(ip))
    {
        return vec![i];
    }
    ifaces.iter().filter(usable).filter(|i| i.broadcast).collect()
}

/// The interface a wake would use, `None` meaning all (the overview's `send_iface`).
pub fn send_iface(ifaces: &[NasIface], fixed: Option<&str>, ip: Option<Ipv4Addr>) -> Option<String> {
    if let Some(f) = fixed {
        return Some(f.to_string());
    }
    let ip = ip?;
    ifaces.iter().filter(|i| i.up && eligible(i)).find(|i| i.holds(ip)).map(|i| i.name.clone())
}

/// Parses `aa:bb:cc:dd:ee:ff` (any case, `:` or `-`).
pub fn mac_bytes(mac: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = mac.split([':', '-']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut out = [0u8; 6];
    for (o, p) in out.iter_mut().zip(parts) {
        if p.len() != 2 {
            return None;
        }
        *o = u8::from_str_radix(p, 16).ok()?;
    }
    Some(out)
}

/// A SecureOn password: 6 bytes as a MAC (`aa:bb:cc:dd:ee:ff`) or 4 as `a.b.c.d`.
pub fn secureon_bytes(s: &str) -> Option<Vec<u8>> {
    if let Some(b) = mac_bytes(s) {
        return Some(b.to_vec());
    }
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    parts
        .iter()
        .map(|p| if p.is_empty() || p.len() > 3 { None } else { p.parse::<u8>().ok() })
        .collect()
}

/// The magic packet: 6 × `FF`, 16 × the MAC, the SecureOn password when set.
pub fn magic_payload(mac: [u8; 6], secureon: Option<&[u8]>) -> Vec<u8> {
    let mut p = Vec::with_capacity(6 + 16 * 6 + 6);
    p.extend_from_slice(&[0xff; 6]);
    for _ in 0..16 {
        p.extend_from_slice(&mac);
    }
    if let Some(s) = secureon {
        p.extend_from_slice(s);
    }
    p
}

/// The raw Ethernet frame of a payload: broadcast destination, `src`, EtherType `0x0842`.
pub fn ether_frame(src: [u8; 6], payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(14 + payload.len());
    f.extend_from_slice(&[0xff; 6]);
    f.extend_from_slice(&src);
    f.extend_from_slice(&ETHERTYPE_WOL.to_be_bytes());
    f.extend_from_slice(payload);
    f
}

/// The interface address whose subnet holds `ip` (else its first): the UDP broadcast's.
pub fn broadcast_for(iface: &NasIface, ip: Option<Ipv4Addr>) -> Option<Ipv4Addr> {
    let (a, p) = ip
        .and_then(|ip| iface.ipv4.iter().find(|(a, p)| same_subnet(*a, ip, *p)))
        .or(iface.ipv4.first())?;
    Some(subnet_broadcast(*a, *p))
}

/// Sends one magic packet's payload on one interface: a raw frame and a UDP broadcast to the
/// subnet of the device's address `ip` (else the interface's first subnet). `Ok` when at
/// least one of the two went out.
pub fn send_on(iface: &NasIface, payload: &[u8], port: u16, ip: Option<Ipv4Addr>) -> Result<(), String> {
    let raw = match iface.mac.as_deref().and_then(mac_bytes) {
        Some(src) => send_raw(iface, &ether_frame(src, payload)),
        None => Err("no MAC address".to_string()),
    };
    let udp = match broadcast_for(iface, ip) {
        Some(to) => send_udp(&iface.name, to, port, payload),
        None => Err("no IPv4 address".to_string()),
    };
    match (raw, udp) {
        (Err(a), Err(b)) => Err(format!("raw frame: {a}; UDP: {b}")),
        _ => Ok(()),
    }
}

fn last_os_error() -> String {
    std::io::Error::last_os_error().to_string()
}

fn send_raw(iface: &NasIface, frame: &[u8]) -> Result<(), String> {
    let proto = ETHERTYPE_WOL.to_be();
    // SAFETY: plain socket calls on a descriptor owned here and closed below.
    unsafe {
        let fd = libc::socket(libc::AF_PACKET, libc::SOCK_RAW | libc::SOCK_CLOEXEC, i32::from(proto));
        if fd < 0 {
            return Err(last_os_error());
        }
        let mut addr: libc::sockaddr_ll = std::mem::zeroed();
        addr.sll_family = libc::AF_PACKET as u16;
        addr.sll_protocol = proto;
        addr.sll_ifindex = iface.index as i32;
        addr.sll_halen = 6;
        addr.sll_addr[..6].copy_from_slice(&[0xff; 6]);
        let n = libc::sendto(
            fd,
            frame.as_ptr().cast(),
            frame.len(),
            0,
            (&addr as *const libc::sockaddr_ll).cast(),
            std::mem::size_of::<libc::sockaddr_ll>() as u32,
        );
        let err = (n < 0).then(last_os_error);
        libc::close(fd);
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

fn send_udp(iface: &str, to: Ipv4Addr, port: u16, payload: &[u8]) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    let s = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(|e| e.to_string())?;
    s.set_broadcast(true).map_err(|e| e.to_string())?;
    let name = std::ffi::CString::new(iface).map_err(|e| e.to_string())?;
    // SAFETY: setsockopt with a NUL-terminated name that outlives the call.
    let r = unsafe {
        libc::setsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            name.as_ptr().cast(),
            name.as_bytes_with_nul().len() as u32,
        )
    };
    if r != 0 {
        return Err(last_os_error());
    }
    s.send_to(payload, (to, port)).map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: [u8; 6] = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xf0];

    #[test]
    fn packet_bytes() {
        let p = magic_payload(MAC, None);
        assert_eq!(p.len(), 102);
        assert_eq!(&p[..6], &[0xff; 6]);
        for i in 0..16 {
            assert_eq!(&p[6 + i * 6..12 + i * 6], &MAC);
        }
        let s = secureon_bytes("01:02:03:04:05:06").unwrap();
        let p = magic_payload(MAC, Some(&s));
        assert_eq!(p.len(), 108);
        assert_eq!(&p[102..], &[1, 2, 3, 4, 5, 6]);
        let s = secureon_bytes("203.0.113.1").unwrap();
        assert_eq!(magic_payload(MAC, Some(&s))[102..], [203, 0, 113, 1]);
        assert!(secureon_bytes("1.2.3").is_none() && secureon_bytes("1.2.3.256").is_none());
        let f = ether_frame([2, 0, 0, 0, 0, 1], &p);
        assert_eq!(&f[..6], &[0xff; 6]);
        assert_eq!(&f[6..12], &[2, 0, 0, 0, 0, 1]);
        assert_eq!(&f[12..14], &[0x08, 0x42]);
        assert_eq!(f.len(), 14 + p.len());
    }

    #[test]
    fn broadcasts() {
        let ip: Ipv4Addr = "198.51.100.10".parse().unwrap();
        assert_eq!(subnet_broadcast(ip, 24), Ipv4Addr::new(198, 51, 100, 255));
        assert_eq!(subnet_broadcast(ip, 23), Ipv4Addr::new(198, 51, 101, 255));
        assert_eq!(subnet_broadcast("10.0.0.5".parse().unwrap(), 8), Ipv4Addr::new(10, 255, 255, 255));
        assert_eq!(subnet_broadcast(ip, 32), Ipv4Addr::BROADCAST);
        assert!(same_subnet(ip, "198.51.101.7".parse().unwrap(), 23));
        assert!(!same_subnet(ip, "198.51.102.7".parse().unwrap(), 23));
        // the UDP broadcast goes to the subnet of the device's address
        let i = NasIface {
            name: "eth0".into(),
            ipv4: vec![(Ipv4Addr::new(192, 0, 2, 5), 24), (Ipv4Addr::new(203, 0, 113, 5), 24)],
            ..Default::default()
        };
        assert_eq!(broadcast_for(&i, Some(Ipv4Addr::new(203, 0, 113, 9))), Some(Ipv4Addr::new(203, 0, 113, 255)));
        assert_eq!(broadcast_for(&i, Some(Ipv4Addr::new(10, 0, 0, 9))), Some(Ipv4Addr::new(192, 0, 2, 255)));
        assert_eq!(broadcast_for(&i, None), Some(Ipv4Addr::new(192, 0, 2, 255)));
    }

    fn iface(name: &str, ip: Option<&str>, f: impl FnOnce(&mut NasIface)) -> NasIface {
        let mut i = NasIface {
            name: name.into(),
            index: 2,
            mac: Some("02:00:00:00:00:01".into()),
            up: true,
            broadcast: true,
            ipv4: ip.map(|c| {
                let (a, p) = c.split_once('/').unwrap();
                vec![(a.parse().unwrap(), p.parse().unwrap())]
            })
            .unwrap_or_default(),
            ..Default::default()
        };
        f(&mut i);
        i
    }

    #[test]
    fn interface_choice() {
        let list = vec![
            iface("lo", Some("127.0.0.1/8"), |i| {
                i.loopback = true;
                i.mac = None
            }),
            iface("enp1s0", None, |i| i.master = Some("bond0".into())),
            iface("enp2s0", None, |i| i.master = Some("bond0".into())),
            iface("bond0", Some("192.0.2.5/24"), |i| i.kind = Some("bond".into())),
            iface("enp3s0", Some("10.0.0.5/24"), |_| {}),
            iface("enp4s0", Some("172.16.0.5/24"), |i| i.up = false),
            iface("tailscale0", Some("100.64.0.1/32"), |i| {
                i.mac = None;
                i.broadcast = false
            }),
            iface("docker0", Some("172.17.0.1/16"), |i| i.kind = Some("bridge".into())),
            iface("br-1a2b", Some("172.18.0.1/16"), |i| i.kind = Some("bridge".into())),
            iface("veth12", None, |i| i.kind = Some("veth".into())),
        ];
        let names = |v: Vec<&NasIface>| v.iter().map(|i| i.name.clone()).collect::<Vec<_>>();
        // the subnet's interface: the bond, never its ports
        assert_eq!(names(choose(&list, None, Some("192.0.2.20".parse().unwrap()))), ["bond0"]);
        assert_eq!(names(choose(&list, None, Some("10.0.0.9".parse().unwrap()))), ["enp3s0"]);
        // a down interface's subnet: every usable one
        assert_eq!(names(choose(&list, None, Some("172.16.0.9".parse().unwrap()))), ["bond0", "enp3s0"]);
        // no IP: every usable one (no loopback, tailscale, docker, veth, bond port)
        assert_eq!(names(choose(&list, None, None)), ["bond0", "enp3s0"]);
        // a fixed interface wins, whatever its state
        assert_eq!(names(choose(&list, Some("enp4s0"), None)), ["enp4s0"]);
        assert_eq!(send_iface(&list, None, Some("10.0.0.9".parse().unwrap())).as_deref(), Some("enp3s0"));
        assert_eq!(send_iface(&list, None, Some("8.8.8.8".parse().unwrap())), None);
    }

    #[test]
    fn ip_addr_json() {
        let j = r#"[
          {"ifindex":1,"ifname":"lo","flags":["LOOPBACK","UP","LOWER_UP"],"operstate":"UNKNOWN","link_type":"loopback","address":"00:00:00:00:00:00","addr_info":[{"family":"inet","local":"127.0.0.1","prefixlen":8}]},
          {"ifindex":2,"ifname":"enp1s0","flags":["BROADCAST","MULTICAST","SLAVE","UP","LOWER_UP"],"master":"bond0","operstate":"UP","link_type":"ether","address":"02:11:22:33:44:55","linkinfo":{"info_slave_kind":"bond","info_slave_data":{"perm_hwaddr":"A8:A1:59:00:00:01"}},"addr_info":[]},
          {"ifindex":4,"ifname":"bond0","flags":["BROADCAST","MULTICAST","MASTER","UP","LOWER_UP"],"operstate":"UP","link_type":"ether","address":"02:11:22:33:44:55","linkinfo":{"info_kind":"bond"},"addr_info":[{"family":"inet","local":"192.0.2.5","prefixlen":24},{"family":"inet6","local":"fe80::1","prefixlen":64}]}
        ]"#;
        let l = parse_ip_addr(j);
        assert_eq!(l.len(), 3);
        assert!(l[0].loopback && l[0].mac.is_none());
        assert_eq!(l[1].master.as_deref(), Some("bond0"));
        assert_eq!(l[1].perm_mac.as_deref(), Some("a8:a1:59:00:00:01"));
        assert_eq!(l[2].kind.as_deref(), Some("bond"));
        assert_eq!(l[2].cidrs(), ["192.0.2.5/24"]);
        assert!(l[2].up && l[2].broadcast);
    }
}
