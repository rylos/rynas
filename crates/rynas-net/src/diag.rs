//! Diagnostics: ping, traceroute (`mtr --json` when installed, else `traceroute`, else
//! `tracepath`), DNS lookup (`resolvectl query`, `dig`, `getent`). Arguments are validated and
//! passed as argv (never a shell), every tool has a timeout, output is capped, and the
//! endpoints share a rate limit ([`RateLimit`]).

use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rynas_api_types::network::{
    DiagnosticResult, DnsLookupRequest, PingRequest, TracerouteRequest,
};

use crate::os::NetOs;

/// Output kept per run.
pub const OUTPUT_MAX: usize = 64 << 10;

/// A host name (RFC 1123, at most 253 characters) or an IP address. Never starts with `-`,
/// so it cannot be taken for an option.
pub fn valid_host(h: &str) -> bool {
    if h.parse::<IpAddr>().is_ok() {
        return true;
    }
    !h.is_empty()
        && h.len() <= 253
        && !h.starts_with(['-', '.'])
        && h.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && l.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

fn bad(msg: impl Into<String>) -> String {
    msg.into()
}

/// argv of a ping.
pub fn ping_argv(r: &PingRequest) -> Result<Vec<String>, String> {
    let host = r.host.trim();
    if !valid_host(host) {
        return Err(bad("invalid host name or address"));
    }
    let count = r.count.unwrap_or(4);
    if !(1..=10).contains(&count) {
        return Err(bad("count must be 1-10"));
    }
    let mut v: Vec<String> = vec![
        "ping".into(),
        "-n".into(),
        "-c".into(),
        count.to_string(),
        "-W".into(),
        "2".into(),
        "-w".into(),
        (u32::from(count) * 2 + 2).to_string(),
    ];
    if host.parse::<std::net::Ipv6Addr>().is_ok() {
        v.push("-6".into());
    }
    if let Some(i) = r.interface.as_deref().filter(|i| !i.is_empty()) {
        if !crate::networkd::valid_iface(i) {
            return Err(bad("invalid interface name"));
        }
        v.extend(["-I".into(), i.to_string()]);
    }
    if let Some(s) = r.size {
        if s > 65_000 {
            return Err(bad("size must be 0-65000"));
        }
        v.extend(["-s".into(), s.to_string()]);
    }
    if r.dont_fragment {
        v.extend(["-M".into(), "do".into()]);
    }
    v.push(host.to_string());
    Ok(v)
}

fn which(os: &dyn NetOs, prog: &str) -> bool {
    ["/usr/bin", "/usr/sbin", "/bin", "/sbin"]
        .iter()
        .any(|d| os.runtime_exists(&format!("{d}/{prog}")))
}

/// argv of a traceroute with the tool found.
pub fn traceroute_argv(os: &dyn NetOs, r: &TracerouteRequest) -> Result<Vec<String>, String> {
    let host = r.host.trim();
    if !valid_host(host) {
        return Err(bad("invalid host name or address"));
    }
    let h = host.to_string();
    if which(os, "mtr") {
        Ok(vec![
            "mtr".into(),
            "--json".into(),
            "--no-dns".into(),
            "--report-cycles".into(),
            "3".into(),
            "--max-ttl".into(),
            "20".into(),
            h,
        ])
    } else if which(os, "traceroute") {
        Ok(["traceroute", "-n", "-w", "2", "-q", "1", "-m", "20"]
            .iter()
            .map(|s| s.to_string())
            .chain([h])
            .collect())
    } else {
        Ok(vec![
            "tracepath".into(),
            "-n".into(),
            "-m".into(),
            "20".into(),
            h,
        ])
    }
}

const RECORD_TYPES: [&str; 8] = ["A", "AAAA", "MX", "TXT", "CNAME", "NS", "PTR", "SRV"];

/// argv of a DNS lookup with the tool found.
pub fn dns_argv(
    os: &dyn NetOs,
    resolved: bool,
    r: &DnsLookupRequest,
) -> Result<Vec<String>, String> {
    let name = r.name.trim().trim_end_matches('.');
    if !valid_host(name) {
        return Err(bad("invalid name"));
    }
    let t = r.record_type.as_deref().unwrap_or("A").to_ascii_uppercase();
    if !RECORD_TYPES.contains(&t.as_str()) {
        return Err(bad(format!(
            "record type must be one of {}",
            RECORD_TYPES.join(", ")
        )));
    }
    let server = r.server.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if let Some(s) = server
        && s.parse::<IpAddr>().is_err()
    {
        return Err(bad("the server must be an IP address"));
    }
    if server.is_none() && resolved && which(os, "resolvectl") {
        return Ok(vec![
            "resolvectl".into(),
            "query".into(),
            "--legend=no".into(),
            "-t".into(),
            t,
            name.into(),
        ]);
    }
    if which(os, "dig") {
        let mut v: Vec<String> = vec!["dig".into(), "+time=3".into(), "+tries=1".into()];
        if let Some(s) = server {
            v.push(format!("@{s}"));
        }
        v.extend([name.to_string(), t]);
        return Ok(v);
    }
    if server.is_some() {
        return Err(bad(
            "asking a specific server needs dig (package bind9-dnsutils)",
        ));
    }
    match t.as_str() {
        "A" => Ok(vec!["getent".into(), "ahostsv4".into(), name.into()]),
        "AAAA" => Ok(vec!["getent".into(), "ahostsv6".into(), name.into()]),
        _ => Err(bad("this record type needs resolvectl or dig")),
    }
}

/// Runs a diagnostic argv.
pub fn run(os: &dyn NetOs, argv: &[String], timeout: Duration) -> DiagnosticResult {
    let start = Instant::now();
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    let tool = argv.first().cloned().unwrap_or_default();
    match os.run(&refs, timeout) {
        Ok(o) => {
            let mut output = o.stdout;
            if !o.stderr.trim().is_empty() {
                output.push_str(&o.stderr);
            }
            if output.len() > OUTPUT_MAX {
                let mut cut = OUTPUT_MAX;
                while !output.is_char_boundary(cut) {
                    cut -= 1;
                }
                output.truncate(cut);
            }
            let report = if tool == "mtr" {
                serde_json::from_str(&output).ok()
            } else {
                None
            };
            DiagnosticResult {
                tool,
                ok: o.code == Some(0),
                exit_code: o.code,
                output,
                duration_ms: start.elapsed().as_millis() as u64,
                report,
            }
        }
        Err(e) => DiagnosticResult {
            tool,
            ok: false,
            exit_code: None,
            output: e,
            duration_ms: start.elapsed().as_millis() as u64,
            report: None,
        },
    }
}

/// A sliding-window limit shared by the diagnostic endpoints: at most `max` runs per
/// `window` (memory: `max` instants).
pub struct RateLimit {
    max: usize,
    window: Duration,
    hits: Mutex<VecDeque<Instant>>,
}

impl RateLimit {
    pub fn new(max: usize, window: Duration) -> Self {
        Self {
            max,
            window,
            hits: Mutex::new(VecDeque::with_capacity(max)),
        }
    }

    /// `Ok` and counted, or the wait before the next run is allowed.
    pub fn check(&self, now: Instant) -> Result<(), Duration> {
        let mut h = self.hits.lock().unwrap();
        while h
            .front()
            .is_some_and(|t| now.duration_since(*t) >= self.window)
        {
            h.pop_front();
        }
        if h.len() >= self.max {
            let oldest = *h.front().expect("full");
            return Err(self.window.saturating_sub(now.duration_since(oldest)));
        }
        h.push_back(now);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeOs;

    #[test]
    fn hosts() {
        for ok in [
            "nas.local",
            "192.0.2.1",
            "2001:db8::1",
            "example.com",
            "a_b.lan",
        ] {
            assert!(valid_host(ok), "{ok}");
        }
        for bad in ["-c1", "", "a b", "x;rm", "a..b", ".a", &"a".repeat(64)] {
            assert!(!valid_host(bad), "{bad}");
        }
    }

    #[test]
    fn ping_arguments() {
        let r = PingRequest {
            host: "192.0.2.1".into(),
            count: Some(2),
            interface: Some("lan1".into()),
            size: Some(8972),
            dont_fragment: true,
        };
        assert_eq!(
            ping_argv(&r).unwrap().join(" "),
            "ping -n -c 2 -W 2 -w 6 -I lan1 -s 8972 -M do 192.0.2.1"
        );
        let r6 = PingRequest {
            host: "2001:db8::1".into(),
            ..Default::default()
        };
        assert!(ping_argv(&r6).unwrap().contains(&"-6".to_string()));
        for bad in [
            PingRequest {
                host: "-f".into(),
                ..Default::default()
            },
            PingRequest {
                host: "x".into(),
                count: Some(50),
                ..Default::default()
            },
            PingRequest {
                host: "x".into(),
                interface: Some("../a".into()),
                ..Default::default()
            },
        ] {
            assert!(ping_argv(&bad).is_err());
        }
    }

    #[test]
    fn tools_are_picked_by_availability() {
        let os = FakeOs::new();
        let t = TracerouteRequest {
            host: "example.com".into(),
        };
        assert_eq!(traceroute_argv(&os, &t).unwrap()[0], "tracepath");
        os.add_runtime_file("/usr/bin/traceroute", "");
        assert_eq!(traceroute_argv(&os, &t).unwrap()[0], "traceroute");
        os.add_runtime_file("/usr/bin/mtr", "");
        assert_eq!(traceroute_argv(&os, &t).unwrap()[..2], ["mtr", "--json"]);
        let q = DnsLookupRequest {
            name: "example.com".into(),
            ..Default::default()
        };
        assert_eq!(
            dns_argv(&os, false, &q).unwrap(),
            ["getent", "ahostsv4", "example.com"]
        );
        let mx = DnsLookupRequest {
            record_type: Some("mx".into()),
            ..q.clone()
        };
        assert!(dns_argv(&os, false, &mx).is_err());
        os.add_runtime_file("/usr/bin/resolvectl", "");
        assert_eq!(
            dns_argv(&os, true, &mx).unwrap()[..5],
            ["resolvectl", "query", "--legend=no", "-t", "MX"]
        );
        let at = DnsLookupRequest {
            server: Some("192.0.2.53".into()),
            ..q.clone()
        };
        assert!(dns_argv(&os, true, &at).is_err(), "no dig");
        os.add_runtime_file("/usr/bin/dig", "");
        assert_eq!(
            dns_argv(&os, true, &at).unwrap().join(" "),
            "dig +time=3 +tries=1 @192.0.2.53 example.com A"
        );
        let bad = DnsLookupRequest {
            server: Some("dns.example".into()),
            ..q
        };
        assert!(dns_argv(&os, true, &bad).is_err());
    }

    #[test]
    fn rate_limit_window() {
        let l = RateLimit::new(2, Duration::from_secs(60));
        let t0 = Instant::now();
        assert!(l.check(t0).is_ok());
        assert!(l.check(t0 + Duration::from_secs(1)).is_ok());
        let wait = l.check(t0 + Duration::from_secs(2)).unwrap_err();
        assert_eq!(wait, Duration::from_secs(58));
        assert!(l.check(t0 + Duration::from_secs(60)).is_ok());
    }
}
