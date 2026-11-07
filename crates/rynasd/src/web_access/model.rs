//! The engine-neutral side of web access: validation of the `web_access` key, local names,
//! source ranges of the access levels, and the host facts they are computed from.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use rynas_api_types::apps::WebAuth;
use rynas_api_types::web_access::{
    AccessLevel, ChallengeKind, DnsProvider, ProxyMode, RouteKind, UpstreamScheme,
    WebAccessConfig, WebRoute,
};
use rynas_config::Invalid;

/// Longest route label: `<label>-<host>` must fit a 63-character DNS label for hosts names
/// of up to 22 characters.
pub const MAX_LABEL: usize = 40;

/// Interfaces whose subnets are never "the LAN": container and VM bridges, veth pairs, and
/// the VPN interfaces (they belong to `lan_vpn`).
const NOT_LAN: &[&str] = &[
    "docker",
    "br-",
    "veth",
    "virbr",
    "lxcbr",
    "lxdbr",
    "cni",
    "podman",
    "flannel",
    "cali",
    "vnet",
    "kube",
    "tailscale",
    "wg",
    "tun",
    "tap",
];

/// Tailnet ranges (Tailscale's CGNAT range and its ULA prefix). Trusted only while
/// `tailscale0` exists; the firewall plugin drops them on any other interface (anti-spoof,
/// 100.64.0.0/10 is also ISP CGNAT).
pub const TAILNET: &[&str] = &["100.64.0.0/10", "fd7a:115c:a1e0::/48"];

// ---------------------------------------------------------------------------------------------
// CIDR
// ---------------------------------------------------------------------------------------------

/// A network in CIDR notation, host bits cleared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cidr {
    pub addr: IpAddr,
    pub prefix: u8,
}

impl Cidr {
    pub fn new(addr: IpAddr, prefix: u8) -> Option<Self> {
        let max = if addr.is_ipv4() { 32 } else { 128 };
        if prefix > max {
            return None;
        }
        let addr = match addr {
            IpAddr::V4(a) => {
                let m = if prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - prefix)
                };
                IpAddr::V4(Ipv4Addr::from(u32::from(a) & m))
            }
            IpAddr::V6(a) => {
                let m = if prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - prefix)
                };
                IpAddr::V6(Ipv6Addr::from(u128::from(a) & m))
            }
        };
        Some(Self { addr, prefix })
    }

    /// `a.b.c.d/n`, `x::/n` or a single address.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        match s.split_once('/') {
            Some((a, p)) => {
                let a: IpAddr = a.parse().ok()?;
                let p: u8 = p.parse().ok()?;
                Self::new(a, p)
            }
            None => {
                let a: IpAddr = s.parse().ok()?;
                Self::new(a, if a.is_ipv4() { 32 } else { 128 })
            }
        }
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
            v4 => v4,
        };
        match (self.addr, ip) {
            (IpAddr::V4(_), IpAddr::V4(_)) | (IpAddr::V6(_), IpAddr::V6(_)) => {
                Cidr::new(ip, self.prefix).is_some_and(|c| c.addr == self.addr)
            }
            _ => false,
        }
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

// ---------------------------------------------------------------------------------------------
// host facts
// ---------------------------------------------------------------------------------------------

/// What the render needs from the running system.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostFacts {
    /// Kernel host name (any case, maybe with dots).
    pub hostname: String,
    /// Connected subnets of the LAN interfaces.
    pub lan: BTreeSet<Cidr>,
    /// `tailscale0` exists.
    pub tailscale: bool,
    /// Subnets of WireGuard interfaces (`wg*`).
    pub wireguard: BTreeSet<Cidr>,
    /// The IPv4 address local names point to: the one of the interface with the default
    /// route, else the first LAN address.
    pub primary_v4: Option<Ipv4Addr>,
    /// A stable IPv6 address of the interface with the default route (global or ULA, not
    /// temporary, not deprecated): the AAAA of own-domain records with `lan_ipv6`.
    pub primary_v6: Option<Ipv6Addr>,
    /// Every address of the LAN interfaces (diagnostics: does a name point to this NAS?).
    pub addrs: BTreeSet<IpAddr>,
}

impl HostFacts {
    /// First label of the host name, lowercase (`nas` for `NAS.example.lan`).
    pub fn host(&self) -> String {
        host_label(&self.hostname)
    }

    /// What changes the rendered files (Avahi hosts) or the access rules: a change of it
    /// re-applies the configuration.
    pub fn fingerprint(&self) -> String {
        format!(
            "{}|{:?}|{}|{:?}|{:?}|{:?}",
            self.host(),
            self.lan,
            self.tailscale,
            self.wireguard,
            self.primary_v4,
            self.primary_v6
        )
    }
}

/// First label of a host name, lowercase; `nas` when it is not a valid label.
pub fn host_label(hostname: &str) -> String {
    let h = hostname
        .trim()
        .split('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if valid_label(&h, 63) { h } else { "nas".into() }
}

/// Host facts from `ip -j addr show` and `ip -j route show default`.
pub fn parse_facts(hostname: &str, addr_json: &str, route_json: &str) -> HostFacts {
    let mut f = HostFacts {
        hostname: hostname.trim().to_string(),
        ..Default::default()
    };
    let default_dev: Option<String> = serde_json::from_str::<serde_json::Value>(route_json)
        .ok()
        .and_then(|v| {
            v.as_array()?
                .iter()
                .find_map(|r| r["dev"].as_str().map(str::to_string))
        });
    let Ok(serde_json::Value::Array(links)) = serde_json::from_str(addr_json) else {
        return f;
    };
    let mut first_v4 = None;
    for l in &links {
        let name = l["ifname"].as_str().unwrap_or("");
        if l["link_type"] == "loopback" || name == "lo" {
            continue;
        }
        if name.starts_with("tailscale") {
            f.tailscale = true;
            continue;
        }
        let wg = name.starts_with("wg");
        if !wg && NOT_LAN.iter().any(|p| name.starts_with(p)) {
            continue;
        }
        for a in l["addr_info"].as_array().into_iter().flatten() {
            let Some(ip) = a["local"].as_str().and_then(|s| s.parse::<IpAddr>().ok()) else {
                continue;
            };
            let Some(prefix) = a["prefixlen"].as_u64().and_then(|p| u8::try_from(p).ok()) else {
                continue;
            };
            let Some(net) = Cidr::new(ip, prefix) else {
                continue;
            };
            if wg {
                f.wireguard.insert(net);
                continue;
            }
            // link-local is added as a whole below
            let link_local = match ip {
                IpAddr::V4(v4) => v4.is_link_local(),
                IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
            };
            if !link_local {
                f.lan.insert(net);
            }
            f.addrs.insert(ip);
            if let IpAddr::V6(v6) = ip
                && !link_local
                && f.primary_v6.is_none()
                && default_dev.as_deref() == Some(name)
                && a["temporary"] != true
                && a["deprecated"] != true
                && a["scope"].as_str().unwrap_or("global") == "global"
            {
                f.primary_v6 = Some(v6);
            }
            if let IpAddr::V4(v4) = ip
                && !v4.is_link_local()
            {
                if first_v4.is_none() {
                    first_v4 = Some(v4);
                }
                if f.primary_v4.is_none() && default_dev.as_deref() == Some(name) {
                    f.primary_v4 = Some(v4);
                }
            }
        }
    }
    if f.primary_v4.is_none() {
        f.primary_v4 = first_v4;
    }
    f
}

/// Source ranges of an access level: always loopback (the NAS itself, diagnostics) and
/// link-local; `lan` the connected subnets; `lan_vpn` adds the tailnet (while `tailscale0`
/// exists) and WireGuard subnets; then `allow`. `Public` gives no ranges (anyone).
pub fn access_ranges(level: AccessLevel, facts: &HostFacts, allow: &[String]) -> Vec<String> {
    if level == AccessLevel::Public {
        return Vec::new();
    }
    let mut set: BTreeSet<Cidr> = BTreeSet::new();
    for s in ["127.0.0.0/8", "::1/128", "169.254.0.0/16", "fe80::/10"] {
        set.extend(Cidr::parse(s));
    }
    set.extend(facts.lan.iter().copied());
    if level == AccessLevel::LanVpn {
        set.extend(vpn_ranges(facts).iter().filter_map(|s| Cidr::parse(s)));
    }
    set.extend(allow.iter().filter_map(|a| Cidr::parse(a)));
    set.iter().map(|c| c.to_string()).collect()
}

/// What `lan_vpn` adds to `lan`.
pub fn vpn_ranges(facts: &HostFacts) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    if facts.tailscale {
        v.extend(TAILNET.iter().map(|s| s.to_string()));
    }
    v.extend(facts.wireguard.iter().map(|c| c.to_string()));
    v
}

// ---------------------------------------------------------------------------------------------
// names
// ---------------------------------------------------------------------------------------------

/// A DNS label of letters, digits and `-` (lowercase), not starting or ending with `-`.
pub fn valid_label(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && !s.starts_with('-')
        && !s.ends_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `<label>-<host>.local`, or `None` when the first label would exceed 63 characters.
pub fn local_name(label: &str, host: &str) -> Option<String> {
    let first = format!("{label}-{host}");
    (first.len() <= 63).then(|| format!("{first}.local"))
}

/// Names of the interface's own route: `<host>.local` and the bare host name (LAN DNS
/// often resolves it).
pub fn ui_names(host: &str) -> Vec<String> {
    vec![format!("{host}.local"), host.to_string()]
}

/// A label for a stack: lowercase, `_` and other characters to `-`, at most
/// [`MAX_LABEL`] characters, then `-2`, `-3`... while it collides with `taken`.
pub fn suggest_label(stack: &str, taken: &BTreeSet<String>) -> String {
    let mut base: String = stack
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while base.contains("--") {
        base = base.replace("--", "-");
    }
    let mut base = base.trim_matches('-').to_string();
    base.truncate(MAX_LABEL - 3);
    let base = base.trim_end_matches('-').to_string();
    let base = if base.is_empty() { "app".into() } else { base };
    if !taken.contains(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|l| !taken.contains(l))
        .expect("an unused suffix exists")
}

/// A route id for a stack, unique among `taken`.
pub fn suggest_id(stack: &str, taken: &BTreeSet<String>) -> String {
    let l = suggest_label(stack, &BTreeSet::new());
    let base = format!("app-{l}");
    if !taken.contains(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|l| !taken.contains(l))
        .expect("an unused suffix exists")
}

// ---------------------------------------------------------------------------------------------
// validation
// ---------------------------------------------------------------------------------------------

fn size_ok(s: &str) -> bool {
    parse_size(s).is_some()
}

/// `<digits>[K|M|G]` in bytes (binary units).
pub fn parse_size(s: &str) -> Option<u64> {
    let (digits, mul) = match s.as_bytes().last()? {
        b'K' => (&s[..s.len() - 1], 1u64 << 10),
        b'M' => (&s[..s.len() - 1], 1 << 20),
        b'G' => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits
        .parse::<u64>()
        .ok()?
        .checked_mul(mul)
        .filter(|n| *n > 0)
}

/// `<digits>s|m|h`.
pub fn duration_ok(s: &str) -> bool {
    match s.strip_suffix(['s', 'm', 'h']) {
        Some(d) => {
            !d.is_empty() && d.len() <= 6 && d.bytes().all(|b| b.is_ascii_digit()) && d != "0"
        }
        None => false,
    }
}

fn path_ok(p: &str) -> bool {
    p.starts_with('/')
        && p.len() <= 512
        && !p.contains("..")
        && p.bytes().all(|b| b.is_ascii_graphic())
}

/// Checks and normalises the key. `reserved` are ports the proxy may not take (rynas-web's
/// plain and emergency ports).
pub fn validate(mut c: WebAccessConfig, reserved: &[u16]) -> Result<WebAccessConfig, Invalid> {
    let s = &mut c.settings;
    for (field, port) in [
        ("settings.http_port", s.http_port),
        ("settings.https_port", s.https_port),
    ] {
        if port == 0 {
            return Err(Invalid::at(field, "port 0"));
        }
        if reserved.contains(&port) {
            return Err(Invalid::at(
                field,
                format!("port {port} belongs to the web interface"),
            ));
        }
    }
    if s.http_port == s.https_port {
        return Err(Invalid::at(
            "settings.https_port",
            "the HTTP and HTTPS ports must differ",
        ));
    }
    check_access(s.ui.access, "settings.ui.access")?;
    check_allow(&mut s.ui.allow, "settings.ui.allow")?;
    let mut ids = BTreeSet::new();
    let mut labels = BTreeMap::new();
    for (i, r) in c.routes.iter_mut().enumerate() {
        let at = |f: &str| format!("routes[{i}].{f}");
        r.id = r.id.trim().to_string();
        if !valid_label(&r.id, 63) {
            return Err(Invalid::at(
                at("id"),
                "use 1-63 characters a-z, 0-9 and '-'",
            ));
        }
        if !ids.insert(r.id.clone()) {
            return Err(Invalid::at(
                at("id"),
                format!("the id {} is used twice", r.id),
            ));
        }
        r.label = r.label.trim().to_ascii_lowercase();
        if !valid_label(&r.label, MAX_LABEL) {
            return Err(Invalid::at(
                at("label"),
                format!("use 1-{MAX_LABEL} characters a-z, 0-9 and '-', not at the start or end"),
            ));
        }
        if let Some(other) = labels.insert(r.label.clone(), r.id.clone()) {
            return Err(Invalid::at(
                at("label"),
                format!("the name {} is already used by the route {other}", r.label),
            ));
        }
        match r.kind {
            RouteKind::App | RouteKind::Stack => {
                if r.stack.as_deref().is_none_or(|s| s.is_empty()) {
                    return Err(Invalid::at(
                        at("stack"),
                        "an app or stack route names its stack",
                    ));
                }
            }
            RouteKind::Custom => {}
        }
        let host = r
            .upstream
            .host
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        let ip: Option<IpAddr> = host.parse().ok();
        if r.kind == RouteKind::Custom {
            let ok = match ip {
                Some(IpAddr::V4(v4)) => {
                    !v4.is_unspecified() && !v4.is_multicast() && !v4.is_broadcast()
                }
                Some(IpAddr::V6(v6)) => !v6.is_unspecified() && !v6.is_multicast(),
                None => {
                    host.len() <= 253
                        && !host.is_empty()
                        && host.split('.').all(|l| valid_label(l, 63))
                }
            };
            if !ok {
                return Err(Invalid::at(
                    at("upstream.host"),
                    "an address or a host name such as 192.168.1.20 or printer.lan",
                ));
            }
        } else if !ip.is_some_and(|i| i.is_loopback()) {
            return Err(Invalid::at(
                at("upstream.host"),
                "the upstream must be on this NAS (127.0.0.1 or ::1)",
            ));
        }
        r.upstream.host = host;
        if r.upstream.port == 0 {
            return Err(Invalid::at(at("upstream.port"), "port 0"));
        }
        // nothing to skip: plain HTTP, or loopback (never verified)
        if r.upstream.scheme == UpstreamScheme::Http || ip.is_some_and(|i| i.is_loopback()) {
            r.upstream.skip_verify = false;
        }
        r.prefix = match r.prefix.as_deref().map(|p| p.trim().trim_end_matches('/')) {
            None | Some("") => None,
            Some(_) if r.kind != RouteKind::Custom => {
                return Err(Invalid::at(
                    at("prefix"),
                    "a path prefix is for routes to other addresses",
                ));
            }
            Some(p) if path_ok(p) && !p.contains(['*', '?', '#', '%', '{', '}']) => {
                Some(p.to_string())
            }
            Some(_) => {
                return Err(Invalid::at(
                    at("prefix"),
                    "a path such as /grafana (letters, digits, - _ . /)",
                ));
            }
        };
        check_access(r.access, &at("access"))?;
        check_allow(&mut r.allow, &at("allow"))?;
        if r.sso.is_some() {
            return Err(Invalid::at(
                at("sso"),
                "the rynas login in front of apps is not available yet",
            ));
        }
        if r.auth == WebAuth::None && r.access != AccessLevel::Lan {
            return Err(Invalid::at(
                at("access"),
                "an app without its own login stays on the local network",
            ));
        }
        if let Some(b) = &r.limits.body
            && !size_ok(b)
        {
            return Err(Invalid::at(
                at("limits.body"),
                "use <digits>[K|M|G], e.g. 50G",
            ));
        }
        if let Some(t) = &r.limits.timeout
            && !duration_ok(t)
        {
            return Err(Invalid::at(
                at("limits.timeout"),
                "use <digits>s|m|h, e.g. 600s",
            ));
        }
        if !path_ok(&r.path) {
            return Err(Invalid::at(at("path"), "a path starting with /"));
        }
    }
    validate_w2(&mut c)?;
    validate_ports(&mut c, reserved)?;
    // external mode keeps the routes: they come back with the built-in proxy
    Ok(c)
}

/// Longest own domain: `<label>.<domain>` must stay within 253 characters.
pub const MAX_DOMAIN: usize = 253 - MAX_LABEL - 1;

/// Suffixes no public CA issues for (they add nothing over `.local` + the local CA).
const PRIVATE_SUFFIXES: &[&str] = &[
    "local",
    "lan",
    "home",
    "internal",
    "home.arpa",
    "localhost",
    "test",
    "invalid",
    "example",
    "corp",
    "intranet",
    "localdomain",
];

/// A DNS name of at least two labels (lowercase letters, digits, `-`; IDN as `xn--`), no
/// wildcard, at most `max` characters; returned lowercase without a final dot.
pub fn dns_name(s: &str, max: usize) -> Option<String> {
    let n = s.trim().trim_end_matches('.').to_ascii_lowercase();
    if n.len() > max || n.split('.').count() < 2 {
        return None;
    }
    n.split('.').all(|l| valid_label(l, 63)).then_some(n)
}

/// A CNAME target: like [`dns_name`], labels may also hold `_` (`_acme-challenge.x.example.net`).
fn target_name(s: &str) -> Option<String> {
    let n = s.trim().trim_end_matches('.').to_ascii_lowercase();
    let ok = n.len() <= 253
        && n.split('.').count() >= 2
        && n.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        });
    ok.then_some(n)
}

/// Domains, credentials and the ACME settings (wave W2).
fn validate_w2(c: &mut WebAccessConfig) -> Result<(), Invalid> {
    let mut ids = BTreeSet::new();
    for (i, cr) in c.credentials.iter_mut().enumerate() {
        let at = |f: &str| format!("credentials[{i}].{f}");
        cr.id = cr.id.trim().to_string();
        if !valid_label(&cr.id, 63) {
            return Err(Invalid::at(
                at("id"),
                "use 1-63 characters a-z, 0-9 and '-'",
            ));
        }
        if !ids.insert(cr.id.clone()) {
            return Err(Invalid::at(
                at("id"),
                format!("the id {} is used twice", cr.id),
            ));
        }
        cr.label = cr.label.trim().to_string();
        if cr.label.chars().count() > 100 || cr.label.chars().any(char::is_control) {
            return Err(Invalid::at(
                at("label"),
                "at most 100 characters on one line",
            ));
        }
        if !cr.secrets.is_empty() {
            rynas_dns::check_creds(cr.provider, &cr.secrets)
                .map_err(|(f, m)| Invalid::at(at(&format!("secrets.{f}")), m))?;
        }
    }
    let mut dids = BTreeSet::new();
    let mut names = BTreeSet::new();
    for (i, d) in c.domains.iter_mut().enumerate() {
        let at = |f: &str| format!("domains[{i}].{f}");
        d.id = d.id.trim().to_string();
        if !valid_label(&d.id, 63) {
            return Err(Invalid::at(
                at("id"),
                "use 1-63 characters a-z, 0-9 and '-'",
            ));
        }
        if !dids.insert(d.id.clone()) {
            return Err(Invalid::at(
                at("id"),
                format!("the id {} is used twice", d.id),
            ));
        }
        let name = dns_name(&d.name, MAX_DOMAIN).ok_or_else(|| {
            Invalid::at(
                at("name"),
                format!("a domain name such as nas.example.com, at most {MAX_DOMAIN} characters, no wildcard"),
            )
        })?;
        if PRIVATE_SUFFIXES
            .iter()
            .any(|p| name == *p || name.ends_with(&format!(".{p}")))
        {
            return Err(Invalid::at(
                at("name"),
                "no public CA issues certificates for this name: use a domain you own (local names already cover .local)",
            ));
        }
        if !names.insert(name.clone()) {
            return Err(Invalid::at(at("name"), format!("{name} is used twice")));
        }
        d.name = name;
        if d.challenge == ChallengeKind::Http01 {
            // one certificate per name: no wildcard with HTTP-01
            d.wildcard = false;
        }
        let needs_provider = d.challenge != ChallengeKind::Http01 || d.lan_record;
        match d.provider {
            None if needs_provider => {
                return Err(Invalid::at(
                    at("provider"),
                    "choose the DNS provider of the domain",
                ));
            }
            None => d.credential = None,
            Some(p) => {
                if p == DnsProvider::Acmedns && d.challenge != ChallengeKind::CnameDelegation {
                    return Err(Invalid::at(
                        at("provider"),
                        "acme-dns only answers the delegated challenge: use CNAME delegation",
                    ));
                }
                if p == DnsProvider::Duckdns
                    && !(d.name.ends_with(".duckdns.org") && d.name.split('.').count() == 3)
                {
                    return Err(Invalid::at(
                        at("name"),
                        "a DuckDNS domain is <name>.duckdns.org",
                    ));
                }
                let cid = d.credential.as_deref().unwrap_or("").trim().to_string();
                let cred = c.credentials.iter().find(|x| x.id == cid).ok_or_else(|| {
                    Invalid::at(at("credential"), "choose a credential of the provider")
                })?;
                if cred.provider != p {
                    return Err(Invalid::at(
                        at("credential"),
                        format!(
                            "the credential {cid} is for {}, not {}",
                            cred.provider.as_str(),
                            p.as_str()
                        ),
                    ));
                }
                d.credential = Some(cid);
            }
        }
        if d.challenge == ChallengeKind::CnameDelegation {
            let t = d
                .delegate_to
                .as_deref()
                .and_then(target_name)
                .ok_or_else(|| {
                    Invalid::at(
                        at("delegate_to"),
                        "the name _acme-challenge.<domain> points to (the CNAME target)",
                    )
                })?;
            d.delegate_to = Some(t);
        } else {
            d.delegate_to = None;
        }
        // record names rynas may take over: the domain or names below it, lowercase
        let mut take = Vec::new();
        for t in &d.take_over {
            let t = t.trim().trim_end_matches('.').to_ascii_lowercase();
            let base = t.strip_prefix("*.").unwrap_or(&t);
            let below = base == d.name || base.ends_with(&format!(".{}", d.name));
            if !below || dns_name(base, MAX_DOMAIN).is_none() {
                return Err(Invalid::at(
                    at("take_over"),
                    format!("{t}: a record name of {} (or *.{})", d.name, d.name),
                ));
            }
            if !take.contains(&t) {
                take.push(t);
            }
        }
        d.take_over = take;
    }
    let a = &mut c.acme;
    if let Some(e) = a.email.as_mut() {
        *e = e.trim().to_string();
        if e.is_empty() {
            a.email = None;
        } else if e.len() > 254
            || e.matches('@').count() != 1
            || e.starts_with('@')
            || e.ends_with('@')
            || e.chars().any(|ch| ch.is_whitespace() || ch.is_control())
        {
            return Err(Invalid::at("acme.email", "an e-mail address"));
        }
    }
    if let Some(p) = a.profile.as_mut() {
        *p = p.trim().to_ascii_lowercase();
        if p.is_empty() {
            a.profile = None;
        } else if !valid_label(p, 32) {
            return Err(Invalid::at(
                "acme.profile",
                "a profile name such as classic or tlsserver",
            ));
        }
    }
    Ok(())
}

/// Most extra ports one route may have.
pub const MAX_ROUTE_PORTS: usize = 8;

/// Extra HTTPS ports, path prefixes on shared ports, and the upstream loop (§5.6).
fn validate_ports(c: &mut WebAccessConfig, reserved: &[u16]) -> Result<(), Invalid> {
    let (http, https) = (c.settings.http_port, c.settings.https_port);
    let http01 = c
        .domains
        .iter()
        .any(|d| d.enabled && d.challenge == ChallengeKind::Http01);
    // (port, prefix) of the routes that answer on the NAS's names there
    let mut shared: BTreeMap<(u16, String), String> = BTreeMap::new();
    for (i, r) in c.routes.iter_mut().enumerate() {
        let at = |f: &str| format!("routes[{i}].{f}");
        r.ports.sort_unstable();
        r.ports.dedup();
        if r.ports.len() > MAX_ROUTE_PORTS {
            return Err(Invalid::at(
                at("ports"),
                format!("at most {MAX_ROUTE_PORTS} extra ports per address"),
            ));
        }
        for &p in &r.ports {
            if p == http || p == https {
                return Err(Invalid::at(
                    at("ports"),
                    format!("port {p} is the proxy's own port: every address is already on it"),
                ));
            }
            if reserved.contains(&p) {
                return Err(Invalid::at(
                    at("ports"),
                    format!("port {p} belongs to the web interface"),
                ));
            }
            if p < 1024 {
                return Err(Invalid::at(
                    at("ports"),
                    format!(
                        "port {p}: ports below 1024 belong to system services, use 1024-65535"
                    ),
                ));
            }
        }
        if !r.main_port && r.ports.is_empty() {
            return Err(Invalid::at(
                at("main_port"),
                "an address only on extra ports needs at least one port",
            ));
        }
        if r.names.nas && r.ports.is_empty() {
            return Err(Invalid::at(
                at("names.nas"),
                "the NAS's own names are only served on extra ports: add a port",
            ));
        }
        // HTTP-01 answers on port 80 only: an address that lives on another port cannot
        // count on it (the router forwards that port, not 80)
        if !r.main_port && http01 && (r.names.domain || r.names.nas) {
            return Err(Invalid::at(
                at("main_port"),
                "a domain with the HTTP-01 challenge needs port 80 reachable on that name: use DNS-01 for addresses only on extra ports, or keep the address on the main port",
            ));
        }
        if r.enabled && r.names.nas {
            for &p in &r.ports {
                let key = (p, r.prefix.clone().unwrap_or_else(|| "/".into()));
                if let Some(other) = shared.insert(key, r.id.clone()) {
                    return Err(Invalid::at(
                        at("prefix"),
                        format!(
                            "the route {other} already answers on the NAS's names on port {p}{}: give one of them another path prefix or port",
                            r.prefix.as_deref().unwrap_or("")
                        ),
                    ));
                }
            }
        }
    }
    // a custom upstream on loopback may not be one of the proxy's own listeners
    let mut proxy_ports: BTreeSet<u16> = [http, https].into();
    proxy_ports.extend(c.routes.iter().flat_map(|r| r.ports.iter().copied()));
    for (i, r) in c.routes.iter().enumerate() {
        let lo = r
            .upstream
            .host
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(r.upstream.host == "localhost");
        if r.kind == RouteKind::Custom && lo && proxy_ports.contains(&r.upstream.port) {
            return Err(Invalid::at(
                format!("routes[{i}].upstream.port"),
                format!(
                    "port {} on this NAS is the proxy itself: the requests would loop",
                    r.upstream.port
                ),
            ));
        }
    }
    Ok(())
}

/// Ports with a listening TCP socket in `/proc/net/tcp` or `tcp6` text.
pub fn listening_tcp(text: &str) -> BTreeSet<u16> {
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 4 || f[3] != "0A" {
                return None;
            }
            f[1].rsplit_once(':')
                .and_then(|(_, p)| u16::from_str_radix(p, 16).ok())
        })
        .collect()
}

/// The extra ports of `c` another program already listens on: `busy` are the host's
/// listening ports, `held` the ones the proxy itself holds now (the applied key).
pub fn check_busy_ports(
    c: &WebAccessConfig,
    busy: &BTreeSet<u16>,
    held: &BTreeSet<u16>,
) -> Result<(), Invalid> {
    if c.settings.mode != ProxyMode::Builtin {
        return Ok(());
    }
    for (i, r) in c.routes.iter().enumerate() {
        if let Some(p) = r
            .ports
            .iter()
            .find(|p| r.enabled && busy.contains(p) && !held.contains(p))
        {
            return Err(Invalid::at(
                format!("routes[{i}].ports"),
                format!("port {p} is already used on this NAS by another service or container"),
            ));
        }
    }
    Ok(())
}

/// The extra ports the proxy listens on with `c` (enabled routes, built-in mode).
pub fn extra_ports(c: &WebAccessConfig) -> BTreeSet<u16> {
    if c.settings.mode != ProxyMode::Builtin {
        return BTreeSet::new();
    }
    c.routes
        .iter()
        .filter(|r| r.enabled)
        .flat_map(|r| r.ports.iter().copied())
        .collect()
}

fn check_access(a: AccessLevel, field: &str) -> Result<(), Invalid> {
    if a == AccessLevel::Public {
        return Err(Invalid::at(
            field,
            "public access needs a domain and the public-exposure checks, not available yet",
        ));
    }
    Ok(())
}

fn check_allow(allow: &mut Vec<String>, field: &str) -> Result<(), Invalid> {
    let mut out = Vec::new();
    for a in allow.iter() {
        let c = Cidr::parse(a)
            .ok_or_else(|| Invalid::at(field, format!("{a} is not an address or a CIDR range")))?;
        if c.prefix == 0 {
            return Err(Invalid::at(field, format!("{a} would allow everyone")));
        }
        out.push(c.to_string());
    }
    out.sort();
    out.dedup();
    *allow = out;
    Ok(())
}

/// The route of a stack, if any.
pub fn route_of_stack<'a>(c: &'a WebAccessConfig, stack: &str) -> Option<&'a WebRoute> {
    c.routes.iter().find(|r| r.stack.as_deref() == Some(stack))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_parse_contains() {
        let c = Cidr::parse("192.168.1.77/24").unwrap();
        assert_eq!(c.to_string(), "192.168.1.0/24");
        assert!(c.contains("192.168.1.5".parse().unwrap()));
        assert!(c.contains("::ffff:192.168.1.5".parse().unwrap()));
        assert!(!c.contains("192.168.2.5".parse().unwrap()));
        let v6 = Cidr::parse("fd7a:115c:a1e0::5/48").unwrap();
        assert_eq!(v6.to_string(), "fd7a:115c:a1e0::/48");
        assert_eq!(Cidr::parse("10.0.0.1").unwrap().to_string(), "10.0.0.1/32");
        assert!(Cidr::parse("10.0.0.1/33").is_none());
        assert!(Cidr::parse("nas").is_none());
    }

    #[test]
    fn sizes_and_durations() {
        assert_eq!(parse_size("50G"), Some(50 << 30));
        assert_eq!(parse_size("512M"), Some(512 << 20));
        assert_eq!(parse_size("100"), Some(100));
        assert_eq!(parse_size("0"), None);
        assert_eq!(parse_size("G"), None);
        assert_eq!(parse_size("5T"), None);
        assert!(duration_ok("600s") && duration_ok("10m") && duration_ok("1h"));
        assert!(!duration_ok("0s") && !duration_ok("10") && !duration_ok("1d"));
    }
}
