//! The public IPv4 address of the network the NAS is in: asked of a few independent
//! "what is my address" services over IPv4, one after the other, until two agree (or only
//! one answers). An address that is not public (a carrier-grade NAT range, a private one)
//! is refused: a name pointing to it would not reach the NAS from outside.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

/// Services that answer the caller's address as plain text.
pub const IPV4_SERVICES: &[&str] = &["https://api.ipify.org", "https://ipv4.icanhazip.com", "https://v4.ident.me"];

/// An address that may be published: not private, loopback, link-local, carrier-grade NAT
/// (100.64.0.0/10), documentation, benchmarking or reserved.
pub fn is_public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_unspecified()
        || ip.is_documentation()
        || ip.is_multicast()
        || o[0] == 0
        || o[0] >= 240
        || (o[0] == 100 && (o[1] & 0xc0) == 64)
        || (o[0] == 198 && (o[1] & 0xfe) == 18)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0))
}

/// What the answers say: two equal answers win; a single answer is taken; answers that all
/// differ are an error.
pub fn decide(answers: &[Ipv4Addr]) -> Result<Ipv4Addr, String> {
    for (i, a) in answers.iter().enumerate() {
        if answers[i + 1..].contains(a) {
            return Ok(*a);
        }
    }
    match answers {
        [one] => Ok(*one),
        [] => Err("no service answered the public address".into()),
        _ => Err(format!(
            "the services answered different addresses ({})",
            answers.iter().map(Ipv4Addr::to_string).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// The public IPv4 address, asked of `services` over IPv4.
pub fn ipv4(services: &[&str]) -> Result<Ipv4Addr, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(8))
        .connect_timeout(Duration::from_secs(5))
        .local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
        .user_agent(concat!("rynas/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    let mut answers = Vec::new();
    let mut last_err = None;
    for url in services {
        match client.get(*url).send().and_then(|r| r.error_for_status()).and_then(|r| r.text()) {
            Ok(t) => match t.trim().parse::<Ipv4Addr>() {
                Ok(ip) => answers.push(ip),
                Err(_) => last_err = Some(format!("{url}: not an address")),
            },
            Err(e) => last_err = Some(format!("{url}: {}", e.without_url())),
        }
        // with two answers or more, a decision means two of them agree
        if answers.len() >= 2 && decide(&answers).is_ok() {
            break;
        }
    }
    if answers.is_empty() {
        return Err(last_err.unwrap_or_else(|| "no service answered".into()));
    }
    let ip = decide(&answers)?;
    if !is_public_v4(ip) {
        return Err(format!("{ip} is not a public address (the provider shares one address among many customers, carrier-grade NAT): a name pointing to it cannot reach the NAS from outside"));
    }
    Ok(ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    #[test]
    fn answers_agree_or_not() {
        assert_eq!(decide(&[ip("203.0.113.5"), ip("203.0.113.5")]).unwrap(), ip("203.0.113.5"));
        assert_eq!(decide(&[ip("198.51.100.1"), ip("203.0.113.5"), ip("203.0.113.5")]).unwrap(), ip("203.0.113.5"));
        assert_eq!(decide(&[ip("203.0.113.5")]).unwrap(), ip("203.0.113.5"));
        assert!(decide(&[ip("198.51.100.1"), ip("203.0.113.5")]).is_err());
        assert!(decide(&[]).is_err());
    }

    #[test]
    fn public_or_not() {
        assert!(is_public_v4(ip("93.184.216.34")));
        for s in ["10.1.2.3", "192.168.1.1", "100.64.0.1", "100.127.255.255", "127.0.0.1", "169.254.1.1", "0.1.2.3", "240.0.0.1", "198.18.0.1", "192.0.0.8"] {
            assert!(!is_public_v4(ip(s)), "{s}");
        }
        assert!(is_public_v4(ip("100.128.0.1")));
    }
}
