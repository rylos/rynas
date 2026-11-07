//! DNS rebind protection detection (`docs/research/web-access.md` §3.2): dnsmasq/OpenWrt
//! (`rebind_protection`), Fritz!Box, pfSense/OPNsense and many ISP routers drop public answers
//! that point to private addresses. The own-domain LAN mode then fails silently: the public
//! resolver answers the NAS's LAN address, the LAN resolver answers nothing. Comparing the two
//! tells that apart from a record that is missing or wrong.

use std::net::IpAddr;

use crate::wire::Answer;

/// What the comparison says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Both resolvers answer the expected address.
    Ok,
    /// The public resolver answers the (private) LAN address, the LAN resolver drops it:
    /// rebind protection.
    RebindBlocked,
    /// The LAN resolver answers other addresses than the public one (a local override, an
    /// old entry in the router or in AdGuard/Pi-hole).
    LanOverride(Vec<IpAddr>),
    /// The record is not published (yet): the public resolver has no address.
    NotPublished,
    /// The public record points elsewhere.
    Wrong(Vec<IpAddr>),
    /// The public resolver could not be asked (outbound DNS blocked?): only the LAN side is
    /// known; `true` when it answers the expected address.
    PublicUnreachable(bool),
    /// The LAN resolver could not be asked.
    LanUnreachable,
}

/// RFC 1918, shared CGNAT space, IPv6 ULA and link-local: what rebind protection filters.
pub fn is_private(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private() || v4.is_link_local() || (o[0] == 100 && (o[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            let s = v6.segments()[0];
            (s & 0xfe00) == 0xfc00 || (s & 0xffc0) == 0xfe80
        }
    }
}

fn hits(a: &Answer, expected: &[IpAddr]) -> bool {
    a.addrs().iter().any(|x| expected.contains(x))
}

/// Compares the public and the LAN resolvers' answers for a name that should resolve to one
/// of `expected`.
pub fn verdict(
    expected: &[IpAddr],
    public: &Result<Answer, String>,
    lan: &Result<Answer, String>,
) -> Verdict {
    let public = match public {
        Err(_) => {
            return Verdict::PublicUnreachable(lan.as_ref().is_ok_and(|l| hits(l, expected)));
        }
        Ok(p) => p,
    };
    if !hits(public, expected) {
        return match public {
            Answer::Addrs(a) => Verdict::Wrong(a.clone()),
            _ => Verdict::NotPublished,
        };
    }
    let Ok(lan) = lan else {
        return Verdict::LanUnreachable;
    };
    if hits(lan, expected) {
        return Verdict::Ok;
    }
    match lan {
        // some routers answer 0.0.0.0 / :: instead of dropping
        Answer::Addrs(a) if a.iter().all(|x| x.is_unspecified()) => Verdict::RebindBlocked,
        Answer::Addrs(a) => Verdict::LanOverride(a.clone()),
        // SERVFAIL and other codes are a broken resolver (DNSSEC, upstream), not rebind
        // protection (review 2026-11-07, L1)
        Answer::ServFail | Answer::Other(_) => Verdict::LanUnreachable,
        _ if expected.iter().any(is_private) => Verdict::RebindBlocked,
        _ => Verdict::LanOverride(Vec::new()),
    }
}

/// English fix for rebind protection, for the checklist detail.
pub const REBIND_FIX: &str = "your router's DNS drops public answers that point to private addresses (DNS rebind protection). Allow the domain there: OpenWrt: Network → DHCP and DNS → Filter → 'Domain whitelist' (dnsmasq rebind_domain) gets the domain; Fritz!Box: Home Network → Network → Network Settings → DNS rebind protection → add the domain; pfSense/OPNsense: Unbound 'private-domain' for the domain. Or run AdGuard Home / Pi-hole as your LAN DNS with a local record";

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn verdicts() {
        let lan_ip = vec![ip("192.168.1.10")];
        let good = Ok(Answer::Addrs(lan_ip.clone()));
        assert_eq!(verdict(&lan_ip, &good, &good), Verdict::Ok);
        for dropped in [
            Answer::NxDomain,
            Answer::NoData,
            Answer::Refused,
            Answer::Addrs(vec![ip("0.0.0.0")]),
        ] {
            assert_eq!(
                verdict(&lan_ip, &good, &Ok(dropped.clone())),
                Verdict::RebindBlocked,
                "{dropped:?}"
            );
        }
        assert_eq!(
            verdict(&lan_ip, &good, &Ok(Answer::Addrs(vec![ip("192.168.1.99")]))),
            Verdict::LanOverride(vec![ip("192.168.1.99")])
        );
        assert_eq!(
            verdict(&lan_ip, &Ok(Answer::NxDomain), &Ok(Answer::NxDomain)),
            Verdict::NotPublished
        );
        assert_eq!(
            verdict(&lan_ip, &Ok(Answer::Addrs(vec![ip("203.0.113.5")])), &good),
            Verdict::Wrong(vec![ip("203.0.113.5")])
        );
        assert_eq!(
            verdict(&lan_ip, &Err("timeout".into()), &good),
            Verdict::PublicUnreachable(true)
        );
        assert_eq!(
            verdict(&lan_ip, &good, &Err("x".into())),
            Verdict::LanUnreachable
        );
        for broken in [Answer::ServFail, Answer::Other(4)] {
            assert_eq!(
                verdict(&lan_ip, &good, &Ok(broken)),
                Verdict::LanUnreachable
            );
        }
        // a public address missing on the LAN is not rebind protection
        let pub_ip = vec![ip("203.0.113.5")];
        assert_eq!(
            verdict(
                &pub_ip,
                &Ok(Answer::Addrs(pub_ip.clone())),
                &Ok(Answer::NxDomain)
            ),
            Verdict::LanOverride(vec![])
        );
    }

    #[test]
    fn private_ranges() {
        for s in [
            "10.1.2.3",
            "172.16.0.1",
            "192.168.0.1",
            "100.64.1.1",
            "fd00::1",
            "fe80::1",
            "169.254.1.1",
        ] {
            assert!(is_private(&ip(s)), "{s}");
        }
        for s in ["192.0.2.10", "8.8.8.8", "2001:db8::1", "100.128.0.1"] {
            assert!(!is_private(&ip(s)), "{s}");
        }
    }
}
