//! The PC's facts for the NAS (`AgentFacts`, docs/research/wake-on-lan.md §3.5): physical
//! network cards with their MAC, IPv4 and Wake-on-LAN state, and the sleep modes this PC
//! has. Sent at the service's start, every 6 hours when they changed, and within 5 minutes of
//! a change of the cards' IPv4 addresses; an older NAS (404, or 401 from its web front) is
//! asked again at the next 6-hour check. Failures are logged, never fatal.

use std::path::Path;

use rynas_api_types::pcagent::{AGENT_FACTS, AgentFacts, AgentNic, SleepMode};

use crate::power::Exec;

/// How often the facts are collected again (and sent when they changed).
pub const EVERY_MS: u64 = 6 * 3_600_000;
/// A send that failed (no answer, an error other than 404) is tried again after this.
pub const RETRY_MS: u64 = 10 * 60_000;
/// How often the cards' IPv4 addresses are looked at (a change is sent at once).
pub const ADDR_EVERY_MS: u64 = 5 * 60_000;

/// Name prefixes of virtual cards (a safety net: they normally have no `device` link).
const VIRTUAL: &[&str] = &["lo", "veth", "docker", "br-", "virbr", "vnet", "tun", "tap", "wg", "tailscale", "zt", "vmnet", "vboxnet"];

/// One card from sysfs, before the IPv4 and `ethtool` parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SysNic {
    pub name: String,
    pub mac: String,
    pub wireless: bool,
    pub up: bool,
}

fn read_trim(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

/// The physical cards under `<sys>/class/net`: a `device` link, Ethernet-like (type 1), not a
/// bridge, bond, tunnel or a known virtual name. The MAC is the card's own (a bond port's
/// permanent address, not the bond's), lower case. Sorted by name.
pub fn sys_nics(sys: &Path) -> Vec<SysNic> {
    let base = sys.join("class/net");
    let Ok(dir) = std::fs::read_dir(&base) else { return Vec::new() };
    let mut out = Vec::new();
    for e in dir.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let p = base.join(&name);
        if name.chars().any(char::is_control)
            || VIRTUAL.iter().any(|v| name.starts_with(v))
            || !p.join("device").exists()
            || p.join("bridge").exists()
            || p.join("bonding").exists()
            || p.join("tun_flags").exists()
            || read_trim(&p.join("type")).is_some_and(|t| t != "1")
        {
            continue;
        }
        let mac = read_trim(&p.join("bonding_slave/perm_hwaddr"))
            .filter(|m| !m.is_empty())
            .or_else(|| read_trim(&p.join("address")))
            .unwrap_or_default()
            .to_ascii_lowercase();
        if mac.is_empty() || mac == "00:00:00:00:00:00" {
            continue;
        }
        let wireless = p.join("wireless").exists() || p.join("phy80211").exists();
        let up = match read_trim(&p.join("carrier")) {
            Some(c) => c == "1",
            None => read_trim(&p.join("operstate")).as_deref() == Some("up"),
        };
        out.push(SysNic { name, mac, wireless, up });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// `ethtool <if>`: (magic packet supported, enabled); `None` where the line is missing.
pub fn parse_ethtool(out: &str) -> (Option<bool>, Option<bool>) {
    let mut supported = None;
    let mut enabled = None;
    for line in out.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("Supports Wake-on:") {
            supported = Some(v.trim().contains('g'));
        } else if let Some(v) = line.strip_prefix("Wake-on:") {
            enabled = Some(v.trim().contains('g'));
        }
    }
    (supported, enabled)
}

/// The IPv4 addresses with prefix of each interface (`getifaddrs`).
pub fn ipv4_by_name() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills a list freed below with freeifaddrs; every pointer is checked
    // before use and the sockaddrs are read as their family says
    unsafe {
        if libc::getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut cur = head;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || i32::from((*ifa.ifa_addr).sa_family) != libc::AF_INET || ifa.ifa_name.is_null() {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
            let addr = &*(ifa.ifa_addr as *const libc::sockaddr_in);
            let ip = std::net::Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
            let prefix = if ifa.ifa_netmask.is_null() {
                32
            } else {
                let m = &*(ifa.ifa_netmask as *const libc::sockaddr_in);
                u32::from_be(m.sin_addr.s_addr).count_ones()
            };
            out.push((name, format!("{ip}/{prefix}")));
        }
        libc::freeifaddrs(head);
    }
    out
}

/// `busctl` answer to a logind `Can…` call (`s "yes"`): yes or challenge.
pub fn parse_can(out: &str) -> bool {
    out.contains("\"yes\"") || out.contains("\"challenge\"")
}

/// What `Sleep` can do here: suspend and hibernate as logind says (else `/sys/power/state`
/// when `busctl` is missing), shutting down always.
pub fn sleep_modes(exec: &dyn Exec, sys: &Path) -> Vec<SleepMode> {
    let can = |method: &str| {
        exec.read(
            "busctl",
            &["call", "org.freedesktop.login1", "/org/freedesktop/login1", "org.freedesktop.login1.Manager", method],
        )
    };
    let power_state = read_trim(&sys.join("power/state")).unwrap_or_default();
    let mut out = Vec::new();
    for (method, word, mode) in [("CanSuspend", "mem", SleepMode::Suspend), ("CanHibernate", "disk", SleepMode::Hibernate)] {
        let ok = match can(method) {
            Some((true, o)) => parse_can(&o),
            _ => power_state.split_whitespace().any(|w| w == word),
        };
        if ok {
            out.push(mode);
        }
    }
    out.push(SleepMode::Poweroff);
    normalize_sleep(out)
}

/// Sleep modes in a fixed order (suspend, hibernate, power off), once each, never `Unknown`.
pub fn normalize_sleep(modes: Vec<SleepMode>) -> Vec<SleepMode> {
    [SleepMode::Suspend, SleepMode::Hibernate, SleepMode::Poweroff].into_iter().filter(|m| modes.contains(m)).collect()
}

/// The IPv4 addresses of the reported cards differ from `ipv4` (order does not matter).
pub fn addresses_changed(facts: &AgentFacts, ipv4: &[(String, String)]) -> bool {
    facts.nics.iter().any(|n| {
        let mut now: Vec<&String> = ipv4.iter().filter(|(i, _)| *i == n.name).map(|(_, a)| a).collect();
        let mut was: Vec<&String> = n.ipv4.iter().collect();
        now.sort();
        now.dedup();
        was.sort();
        was.dedup();
        now != was
    })
}

/// All the facts of this PC (`sys` is `/sys`; `kept` the cards with Wake-on-LAN kept on).
pub fn collect(exec: &dyn Exec, sys: &Path, ipv4: &[(String, String)], kept: &[String], root_btrfs: bool) -> AgentFacts {
    let nics = sys_nics(sys)
        .into_iter()
        .map(|n| {
            // the `ethtool` program when it is there, else the kernel directly
            let (wol_supported, wol_enabled) = match exec.read("ethtool", &[&n.name]).map(|(_, out)| parse_ethtool(&out)) {
                Some((Some(s), e)) => (Some(s), e),
                _ => match crate::wolioctl::get(&n.name) {
                    Some((s, e)) => (Some(s), Some(e)),
                    None => (None, None),
                },
            };
            AgentNic {
                ipv4: ipv4.iter().filter(|(i, _)| *i == n.name).map(|(_, a)| a.clone()).collect(),
                wol_kept: kept.contains(&n.name),
                name: n.name,
                mac: n.mac,
                wireless: n.wireless,
                up: n.up,
                wol_supported,
                wol_enabled,
            }
        })
        .collect();
    AgentFacts {
        agent_version: env!("CARGO_PKG_VERSION").into(),
        root_btrfs: Some(root_btrfs),
        nics,
        sleep: sleep_modes(exec, sys),
    }
}

/// When the facts go to the NAS.
#[derive(Debug, Default)]
pub struct Sender {
    /// The facts the NAS has (sent and accepted).
    last: Option<AgentFacts>,
    /// The next collection (ms; 0: at once).
    next_ms: u64,
    /// The next cheap look at the cards' IPv4 addresses (ms).
    addr_next_ms: u64,
}

/// What a collection leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendDecision {
    Skip,
    Send,
}

/// The facts go out when the NAS has none from this run, or they changed.
pub fn should_send(last: Option<&AgentFacts>, new: &AgentFacts) -> SendDecision {
    if last == Some(new) { SendDecision::Skip } else { SendDecision::Send }
}

impl Sender {
    /// A collection is due.
    pub fn due(&self, now_ms: u64) -> bool {
        now_ms >= self.next_ms
    }

    /// The next check comes at once (an `EnableWol` changed the cards).
    pub fn soon(&mut self) {
        self.next_ms = 0;
    }

    /// A look at the addresses is due (every [`ADDR_EVERY_MS`], once facts were sent).
    pub fn addresses_due(&self, now_ms: u64) -> bool {
        self.last.is_some() && now_ms >= self.addr_next_ms
    }

    /// The reported cards' IPv4 addresses changed: the facts go out at once.
    pub fn check_addresses(&mut self, now_ms: u64, ipv4: &[(String, String)]) {
        self.addr_next_ms = now_ms + ADDR_EVERY_MS;
        if self.last.as_ref().is_some_and(|l| addresses_changed(l, ipv4)) {
            self.next_ms = 0;
        }
    }

    /// Sends `facts` through `post` when they changed; schedules the next check.
    pub fn tick(&mut self, now_ms: u64, facts: AgentFacts, post: impl FnOnce(&AgentFacts) -> Result<(), String>) {
        if should_send(self.last.as_ref(), &facts) == SendDecision::Skip {
            self.next_ms = now_ms + EVERY_MS;
            return;
        }
        match post(&facts) {
            Ok(()) => {
                self.last = Some(facts);
                self.next_ms = now_ms + EVERY_MS;
            }
            // an older NAS: 404, or 401 from its web front's fallback for an unknown path (the
            // service sends only right after a poll with the same token succeeded, so a 401
            // here is no revoked token); also a NAS in the middle of its own update: tried
            // again at the next regular check, not only after the agent restarts
            Err(e) if e.starts_with("HTTP 404") || e.starts_with("HTTP 401") => {
                eprintln!("the NAS does not take the PC's facts ({e}): tried again in 6 hours");
                self.next_ms = now_ms + EVERY_MS;
            }
            Err(e) => {
                eprintln!("PC facts to the NAS: {e}");
                self.next_ms = now_ms + RETRY_MS;
            }
        }
    }
}

/// `POST` [`AGENT_FACTS`] with the device's token.
pub fn post(client: &crate::http::Client, facts: &AgentFacts) -> Result<(), String> {
    client.post_json::<_, serde_json::Value>(AGENT_FACTS, facts).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::tests::Fake;

    fn card(sys: &Path, name: &str, files: &[(&str, &str)], dirs: &[&str]) {
        let p = sys.join("class/net").join(name);
        std::fs::create_dir_all(&p).unwrap();
        for (f, v) in files {
            if let Some(parent) = p.join(f).parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p.join(f), v).unwrap();
        }
        for d in dirs {
            std::fs::create_dir_all(p.join(d)).unwrap();
        }
    }

    #[test]
    fn ethtool_output() {
        let out = "Settings for enp3s0:\n\tSupported ports: [ TP ]\n\tSupports Wake-on: pumbg\n\tWake-on: d\n\tLink detected: yes\n";
        assert_eq!(parse_ethtool(out), (Some(true), Some(false)));
        assert_eq!(parse_ethtool("\tSupports Wake-on: d\n\tWake-on: g\n"), (Some(false), Some(true)));
        assert_eq!(parse_ethtool("Settings for wlan0:\n\tLink detected: yes\n"), (None, None));
    }

    #[test]
    fn physical_cards_only() {
        let d = tempfile::tempdir().unwrap();
        let s = d.path();
        card(s, "enp3s0", &[("address", "AA:BB:CC:00:11:22\n"), ("type", "1"), ("carrier", "1")], &["device"]);
        card(s, "wlp2s0", &[("address", "aa:bb:cc:00:11:33"), ("type", "1"), ("operstate", "down")], &["device", "phy80211"]);
        card(s, "enp4s0", &[("address", "02:00:00:00:00:01"), ("bonding_slave/perm_hwaddr", "aa:bb:cc:00:11:44"), ("carrier", "0")], &["device"]);
        card(s, "lo", &[("address", "00:00:00:00:00:00"), ("type", "772")], &[]);
        card(s, "br0", &[("address", "aa:bb:cc:00:11:55")], &["bridge"]);
        card(s, "bond0", &[("address", "02:00:00:00:00:01")], &["bonding"]);
        card(s, "veth12", &[("address", "aa:bb:cc:00:11:66")], &["device"]);
        card(s, "tailscale0", &[("address", "aa:bb:cc:00:11:77"), ("type", "65534")], &["device"]);
        card(s, "usbtun", &[("address", "aa:bb:cc:00:11:88")], &["device", "tun_flags"]);
        card(s, "ib0", &[("address", "aa:bb:cc:00:11:99"), ("type", "32")], &["device"]);
        card(s, "bad\u{1b}x", &[("address", "aa:bb:cc:00:11:aa"), ("type", "1")], &["device"]);
        let nics = sys_nics(s);
        assert_eq!(
            nics,
            [
                SysNic { name: "enp3s0".into(), mac: "aa:bb:cc:00:11:22".into(), wireless: false, up: true },
                SysNic { name: "enp4s0".into(), mac: "aa:bb:cc:00:11:44".into(), wireless: false, up: false },
                SysNic { name: "wlp2s0".into(), mac: "aa:bb:cc:00:11:33".into(), wireless: true, up: false },
            ]
        );

        let mut f = Fake::default();
        f.outputs.insert("ethtool enp3s0".into(), (true, "\tSupports Wake-on: pumbg\n\tWake-on: g\n".into()));
        f.outputs.insert("ethtool wlp2s0".into(), (false, "Settings for wlp2s0:\n".into()));
        std::fs::create_dir_all(s.join("power")).unwrap();
        std::fs::write(s.join("power/state"), "freeze mem disk\n").unwrap();
        let ipv4 = [("enp3s0".to_string(), "192.0.2.20/24".to_string()), ("lo".into(), "127.0.0.1/8".into())];
        let facts = collect(&f, s, &ipv4, &["enp3s0".into()], true);
        assert_eq!(facts.nics.len(), 3);
        let e = &facts.nics[0];
        assert_eq!((e.ipv4.clone(), e.wol_supported, e.wol_enabled, e.wol_kept), (vec!["192.0.2.20/24".into()], Some(true), Some(true), true));
        assert_eq!((facts.nics[1].wol_supported, facts.nics[2].wol_supported), (None, None));
        // no busctl: /sys/power/state
        assert_eq!(facts.sleep, [SleepMode::Suspend, SleepMode::Hibernate, SleepMode::Poweroff]);
        assert_eq!(facts.root_btrfs, Some(true));
    }

    #[test]
    fn logind_sleep_modes() {
        let d = tempfile::tempdir().unwrap();
        let mut f = Fake::default();
        let call = "busctl call org.freedesktop.login1 /org/freedesktop/login1 org.freedesktop.login1.Manager";
        f.outputs.insert(format!("{call} CanSuspend"), (true, "s \"yes\"\n".into()));
        f.outputs.insert(format!("{call} CanHibernate"), (true, "s \"na\"\n".into()));
        assert_eq!(sleep_modes(&f, d.path()), [SleepMode::Suspend, SleepMode::Poweroff]);
        assert!(parse_can("s \"challenge\""));
        assert!(!parse_can("s \"no\""));
        // nothing known: shutting down only
        assert_eq!(sleep_modes(&Fake::default(), d.path()), [SleepMode::Poweroff]);
    }

    #[test]
    fn facts_go_out_when_changed() {
        let nic = AgentNic { name: "enp3s0".into(), mac: "aa:bb:cc:00:11:22".into(), ipv4: vec!["192.0.2.20/24".into()], ..Default::default() };
        let a = AgentFacts { agent_version: "x".into(), nics: vec![nic], ..Default::default() };
        let mut b = a.clone();
        b.sleep.push(SleepMode::Poweroff);
        assert_eq!(should_send(None, &a), SendDecision::Send);
        assert_eq!(should_send(Some(&a), &a), SendDecision::Skip);
        assert_eq!(should_send(Some(&a), &b), SendDecision::Send);

        let mut s = Sender::default();
        assert!(s.due(0));
        assert!(!s.addresses_due(0));
        let mut sent = 0;
        s.tick(1000, a.clone(), |_| {
            sent += 1;
            Ok(())
        });
        assert!(!s.due(1000 + EVERY_MS - 1) && s.due(1000 + EVERY_MS));
        // unchanged: not sent again
        s.tick(1000 + EVERY_MS, a.clone(), |_| {
            sent += 1;
            Ok(())
        });
        assert_eq!(sent, 1);
        // the addresses: same (another card's do not count) = nothing; changed = at once
        assert!(s.addresses_due(2000));
        let same = [("enp3s0".to_string(), "192.0.2.20/24".to_string()), ("lo".into(), "127.0.0.1/8".into())];
        s.check_addresses(2000, &same);
        assert!(!s.due(2000) && !s.addresses_due(2000 + ADDR_EVERY_MS - 1) && s.addresses_due(2000 + ADDR_EVERY_MS));
        s.check_addresses(2000 + ADDR_EVERY_MS, &[("enp3s0".to_string(), "192.0.2.21/24".to_string())]);
        assert!(s.due(2000 + ADDR_EVERY_MS));
        // a failed send: again in 10 min
        s.tick(2000, b.clone(), |_| Err("HTTP 502: down".into()));
        assert!(s.due(2000 + RETRY_MS) && !s.due(2000 + RETRY_MS - 1));
        // an EnableWol: at once
        s.soon();
        assert!(s.due(2001));
        // an older NAS (404, or 401 from its web front), or one updating: at the next regular
        // check, no address checks meanwhile
        let mut s = Sender::default();
        s.tick(3000, b.clone(), |_| Err("HTTP 404: not found".into()));
        assert!(!s.due(3000 + EVERY_MS - 1) && s.due(3000 + EVERY_MS) && !s.addresses_due(u64::MAX));
        let mut s = Sender::default();
        s.tick(3000, b, |_| Err("HTTP 401: unauthenticated".into()));
        assert!(!s.due(3000 + EVERY_MS - 1) && s.due(3000 + EVERY_MS));
        // sleep modes: fixed order, once each, never unknown
        assert_eq!(
            normalize_sleep(vec![SleepMode::Poweroff, SleepMode::Unknown, SleepMode::Suspend, SleepMode::Poweroff]),
            [SleepMode::Suspend, SleepMode::Poweroff]
        );
    }

    #[test]
    fn interfaces_have_addresses() {
        // read-only: loopback has 127.0.0.1/8 on any Linux
        assert!(ipv4_by_name().iter().any(|(n, a)| n == "lo" && a == "127.0.0.1/8"));
    }
}
