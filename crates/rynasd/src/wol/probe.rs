//! Is a device on? (`docs/research/wake-on-lan.md` decision 3): the paired agent's contact,
//! then a TCP port, ICMP ping, the neighbor table. And where is it: its IPv4 address learned
//! from the agent's facts or the neighbor table by MAC.

use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use rynas_api_types::pcagent::AgentFacts;
use rynas_api_types::wol::{WolProbe, WolStatus};
use serde::Deserialize;

/// The agent's last request this recent counts as "on": a running agent long-polls its plan
/// at least every 55 s (`pc_agent::MAX_WAIT`), every request is noted in memory
/// ([`crate::pc_agent::PcAgent::last_contact`]).
pub const AGENT_FRESH_MS: u64 = 90_000;
/// Timeout of one TCP port check.
pub const TCP_TIMEOUT: Duration = Duration::from_millis(500);

/// One entry of the neighbor table (`ip -4 -j neigh show`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Neighbor {
    pub ip: Ipv4Addr,
    pub dev: String,
    pub mac: String,
    pub reachable: bool,
}

#[derive(Deserialize)]
struct IpNeigh {
    #[serde(default)]
    dst: String,
    #[serde(default)]
    dev: String,
    #[serde(default)]
    lladdr: Option<String>,
    #[serde(default)]
    state: Vec<String>,
}

/// Parses `ip -4 -j neigh show`: entries with an address and a MAC, not failed.
pub fn parse_neigh(json: &str) -> Vec<Neighbor> {
    let list: Vec<IpNeigh> = serde_json::from_str(json).unwrap_or_default();
    list.into_iter()
        .filter(|n| !n.state.iter().any(|s| s == "FAILED" || s == "INCOMPLETE"))
        .filter_map(|n| {
            Some(Neighbor {
                ip: n.dst.parse().ok()?,
                mac: rynas_api_types::wol::normalize_mac(n.lladdr.as_deref()?)?,
                reachable: n.state.iter().any(|s| s == "REACHABLE"),
                dev: n.dev,
            })
        })
        .collect()
}

/// The IPv4 address of a device from its agent's cards: the card of one of its MACs first,
/// then any wired card that is up with an address.
pub fn ip_from_facts(facts: &AgentFacts, macs: &[String]) -> Option<Ipv4Addr> {
    let first_v4 = |n: &rynas_api_types::pcagent::AgentNic| {
        n.ipv4.iter().find_map(|c| c.split('/').next().and_then(|a| a.parse::<Ipv4Addr>().ok()))
    };
    macs.iter()
        .find_map(|m| facts.nics.iter().find(|n| &n.mac == m).and_then(first_v4))
        .or_else(|| facts.nics.iter().filter(|n| !n.wireless && n.up).find_map(first_v4))
}

/// The address the neighbor table holds for one of `macs` (reachable entries first).
pub fn ip_from_neighbors(neigh: &[Neighbor], macs: &[String]) -> Option<Ipv4Addr> {
    let of = |want_reachable: bool| {
        neigh.iter().find(|n| macs.contains(&n.mac) && (n.reachable || !want_reachable)).map(|n| n.ip)
    };
    of(true).or_else(|| of(false))
}

/// What the checks found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Seen {
    /// A paired agent is linked.
    pub agent: bool,
    /// Milliseconds since the agent's last contact.
    pub agent_age_ms: Option<u64>,
    /// An address to check is known.
    pub ip_known: bool,
    pub port: bool,
    pub ping: bool,
    /// The neighbor table holds a MAC of the device as reachable.
    pub neighbor: bool,
    /// The device itself answered a port or ping check within [`ANSWERS_USUALLY_MS`].
    pub answers_usually: bool,
}

/// A device that answered a port or ping check this recently "usually answers".
pub const ANSWERS_USUALLY_MS: u64 = 24 * 3_600_000;
/// An agent contact older than this does not keep a device that usually answers, and no
/// longer does, Online (it went to sleep or off since).
pub const AGENT_SILENT_MS: u64 = 20_000;

/// The status from the checks (strongest proof first). The port and ping answers are those
/// of the device itself ([`answer_is_device`]).
pub fn decide(s: &Seen) -> (WolStatus, Option<WolProbe>) {
    // a device that usually answers and stopped: the agent's older contact predates its sleep
    let went_quiet = s.answers_usually && !s.port && !s.ping && s.agent_age_ms.is_some_and(|a| a > AGENT_SILENT_MS);
    if s.agent && !went_quiet && s.agent_age_ms.is_some_and(|a| a <= AGENT_FRESH_MS) {
        return (WolStatus::Online, Some(WolProbe::Agent));
    }
    if s.port {
        return (WolStatus::Online, Some(WolProbe::Port));
    }
    if s.ping {
        return (WolStatus::Online, Some(WolProbe::Ping));
    }
    if s.neighbor {
        return (WolStatus::Online, Some(WolProbe::Neighbor));
    }
    if s.ip_known || s.agent {
        (WolStatus::Offline, None)
    } else {
        (WolStatus::Unknown, None)
    }
}

/// A port or ping answer from `ip` is the device's: always for an address the NAS reaches
/// through a router (`on_subnet` false: no neighbor entry to compare), else only when the
/// neighbor table, read after the answer, holds `ip` with one of the device's MACs (an old
/// address now used by another host answers too).
pub fn answer_is_device(ip: Ipv4Addr, on_subnet: bool, macs: &[String], neigh: &[Neighbor]) -> bool {
    !on_subnet || neigh.iter().any(|n| n.ip == ip && macs.contains(&n.mac))
}

/// One TCP port answers (connected or refused both prove the host is up).
pub fn tcp_answers(ip: Ipv4Addr, port: u16) -> bool {
    match TcpStream::connect_timeout(&SocketAddr::from((ip, port)), TCP_TIMEOUT) {
        Ok(_) => true,
        Err(e) => e.kind() == std::io::ErrorKind::ConnectionRefused,
    }
}

/// Any of `ports` answers (checked at once).
pub fn any_port(ip: Ipv4Addr, ports: &[u16]) -> bool {
    if ports.is_empty() {
        return false;
    }
    std::thread::scope(|s| {
        let hs: Vec<_> = ports.iter().map(|p| s.spawn(move || tcp_answers(ip, *p))).collect();
        hs.into_iter().any(|h| h.join().unwrap_or(false))
    })
}

/// One ICMP echo within a second (`ping -n -c1 -W1`).
pub fn ping(ip: Ipv4Addr) -> bool {
    std::process::Command::new("ping")
        .args(["-n", "-q", "-c", "1", "-W", "1", &ip.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// A host name or IPv4 literal as an IPv4 address (the resolver; blocking).
pub fn resolve(host: &str) -> Option<Ipv4Addr> {
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Some(ip);
    }
    use std::net::ToSocketAddrs;
    (host, 0).to_socket_addrs().ok()?.find_map(|a| match a {
        SocketAddr::V4(v4) => Some(*v4.ip()),
        SocketAddr::V6(_) => None,
    })
}

/// The reverse DNS name of an address, when the resolver has one (blocking).
pub fn reverse(ip: Ipv4Addr) -> Option<String> {
    let sa = libc::sockaddr_in {
        sin_family: libc::AF_INET as u16,
        sin_port: 0,
        sin_addr: libc::in_addr { s_addr: u32::from(ip).to_be() },
        sin_zero: [0; 8],
    };
    let mut host = [0 as libc::c_char; 256];
    // SAFETY: valid sockaddr_in and output buffer of the given length.
    let r = unsafe {
        libc::getnameinfo(
            (&sa as *const libc::sockaddr_in).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as u32,
            host.as_mut_ptr(),
            host.len() as u32,
            std::ptr::null_mut(),
            0,
            libc::NI_NAMEREQD,
        )
    };
    if r != 0 {
        return None;
    }
    // SAFETY: getnameinfo wrote a NUL-terminated string into `host`.
    let s = unsafe { std::ffi::CStr::from_ptr(host.as_ptr()) }.to_string_lossy().into_owned();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::pcagent::AgentNic;

    #[test]
    fn statuses() {
        let none = Seen::default();
        assert_eq!(decide(&none), (WolStatus::Unknown, None));
        assert_eq!(decide(&Seen { ip_known: true, ..none }), (WolStatus::Offline, None));
        assert_eq!(decide(&Seen { agent: true, ..none }), (WolStatus::Offline, None));
        assert_eq!(decide(&Seen { ip_known: true, ping: true, ..none }), (WolStatus::Online, Some(WolProbe::Ping)));
        assert_eq!(decide(&Seen { ip_known: true, port: true, ping: true, ..none }), (WolStatus::Online, Some(WolProbe::Port)));
        // the agent's requests within 90 s: on, whatever the network says (a long poll
        // lasts up to 55 s: no flapping between two polls)
        let agent = Seen { agent: true, agent_age_ms: Some(60_000), ip_known: true, ..none };
        assert_eq!(decide(&agent), (WolStatus::Online, Some(WolProbe::Agent)));
        assert_eq!(decide(&Seen { agent_age_ms: Some(89_000), ..agent }), (WolStatus::Online, Some(WolProbe::Agent)));
        assert_eq!(decide(&Seen { ping: true, ..agent }), (WolStatus::Online, Some(WolProbe::Agent)));
        // silent for longer: off, unless the network answers
        let quiet = Seen { agent_age_ms: Some(120_000), ..agent };
        assert_eq!(decide(&quiet), (WolStatus::Offline, None));
        assert_eq!(decide(&Seen { ping: true, ..quiet }), (WolStatus::Online, Some(WolProbe::Ping)));
        assert_eq!(decide(&Seen { neighbor: true, ..none }), (WolStatus::Online, Some(WolProbe::Neighbor)));
    }

    #[test]
    fn a_device_that_usually_answers_and_stops_is_off() {
        let base = Seen { agent: true, ip_known: true, answers_usually: true, ..Default::default() };
        // the agent spoke 40 s ago, the PC usually answers ping and no longer does: asleep
        assert_eq!(decide(&Seen { agent_age_ms: Some(40_000), ..base }), (WolStatus::Offline, None));
        // just now: still on (the ping may lag behind)
        assert_eq!(decide(&Seen { agent_age_ms: Some(5_000), ..base }).0, WolStatus::Online);
        // it answers: on by the agent as before
        assert_eq!(decide(&Seen { agent_age_ms: Some(40_000), ping: true, ..base }).1, Some(WolProbe::Agent));
        // a device that never answers ping (blocked): the agent's contact keeps it on
        assert_eq!(decide(&Seen { agent_age_ms: Some(40_000), answers_usually: false, ..base }).1, Some(WolProbe::Agent));
    }

    #[test]
    fn answers_checked_by_mac() {
        let macs = vec!["a8:bb:cc:dd:ee:f0".to_string()];
        let ip = Ipv4Addr::new(192, 0, 2, 40);
        let entry = |mac: &str| vec![Neighbor { ip, dev: "eth0".into(), mac: mac.into(), reachable: true }];
        // the device's MAC at that address: its answer
        assert!(answer_is_device(ip, true, &macs, &entry("a8:bb:cc:dd:ee:f0")));
        // another host took the old address
        assert!(!answer_is_device(ip, true, &macs, &entry("a8:00:00:00:00:99")));
        // no entry (yet): not proven
        assert!(!answer_is_device(ip, true, &macs, &[]));
        // behind a router: nothing to compare
        assert!(answer_is_device(ip, false, &macs, &[]));
    }

    #[test]
    fn neighbors_and_facts() {
        let j = r#"[
          {"dst":"192.0.2.20","dev":"bond0","lladdr":"AA:BB:CC:DD:EE:F0","state":["STALE"]},
          {"dst":"192.0.2.21","dev":"bond0","lladdr":"aa:bb:cc:dd:ee:f0","state":["REACHABLE"]},
          {"dst":"192.0.2.22","dev":"bond0","state":["FAILED"]},
          {"dst":"192.0.2.23","dev":"bond0","lladdr":"aa:bb:cc:dd:ee:f1","state":["INCOMPLETE"]}
        ]"#;
        let n = parse_neigh(j);
        assert_eq!(n.len(), 2);
        let macs = vec!["aa:bb:cc:dd:ee:f0".to_string()];
        assert_eq!(ip_from_neighbors(&n, &macs), Some(Ipv4Addr::new(192, 0, 2, 21)));
        assert_eq!(ip_from_neighbors(&n, &["aa:bb:cc:dd:ee:f1".into()]), None);
        let facts = AgentFacts {
            nics: vec![
                AgentNic { name: "wlan0".into(), mac: "aa:00:00:00:00:02".into(), ipv4: vec!["10.0.0.9/24".into()], wireless: true, up: true, ..Default::default() },
                AgentNic { name: "enp3s0".into(), mac: "aa:bb:cc:dd:ee:f0".into(), ipv4: vec!["192.0.2.30/24".into()], up: true, ..Default::default() },
            ],
            ..Default::default()
        };
        assert_eq!(ip_from_facts(&facts, &macs), Some(Ipv4Addr::new(192, 0, 2, 30)));
        assert_eq!(ip_from_facts(&facts, &["aa:00:00:00:00:02".into()]), Some(Ipv4Addr::new(10, 0, 0, 9)));
        assert_eq!(ip_from_facts(&facts, &["ee:00:00:00:00:00".into()]), Some(Ipv4Addr::new(192, 0, 2, 30)));
    }
}
