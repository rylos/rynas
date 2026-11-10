//! `tailscale` command lines and the checks of what the user typed into them.
//!
//! Changes use `tailscale set` (it changes only the flags it is given, unlike `tailscale up`,
//! which wants every non-default flag repeated); a login is `tailscale up --reset` with every
//! option spelled out. An auth key never
//! appears on a command line: the CLI reads it from a root-only file (`--auth-key=file:`).

use std::net::IpAddr;
use std::time::Duration;

use rynas_api_types::tailscale::{TailscaleLogin, TailscaleOptions};
use rynas_tasks::Cmd;

/// How long an interactive login waits for the browser.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const SHORT: Duration = Duration::from_secs(60);

fn cli() -> Cmd {
    Cmd::new(crate::CLI).env_clear()
}

fn flag(name: &str, on: bool) -> String {
    format!("--{name}={on}")
}

/// `tailscale set` with every option rynas edits.
pub fn set(o: &TailscaleOptions) -> Cmd {
    cli()
        .arg("set")
        .arg(flag("accept-routes", o.accept_routes))
        .arg(flag("accept-dns", o.accept_dns))
        .arg(flag("advertise-exit-node", o.exit_node))
        .arg(format!("--advertise-routes={}", o.advertise_routes.join(",")))
        .arg(flag("ssh", o.ssh))
        .timeout(SHORT)
}

/// `tailscale up --reset` for a login: interactive (the URL shows in the status and in the
/// task log) unless `key_file` holds an auth key. `tailscale login` is not used: it starts a
/// new, empty profile every time. `--reset` with every option rynas edits spelled out keeps
/// `keep` (the current options; a node that never logged in gets the server defaults: tailnet
/// DNS and routes not accepted) and avoids `up`'s "mention all non-default flags" refusal.
/// `login_server`: the request's, else `control_url` (the current non-default one).
pub fn login(req: &TailscaleLogin, keep: &TailscaleOptions, control_url: Option<&str>, key_file: Option<&str>) -> Cmd {
    let mut c = cli()
        .args(["up", "--reset"])
        .arg(format!("--timeout={}s", LOGIN_TIMEOUT.as_secs()))
        .arg(flag("accept-routes", keep.accept_routes))
        .arg(flag("accept-dns", keep.accept_dns))
        .arg(flag("advertise-exit-node", keep.exit_node))
        .arg(format!("--advertise-routes={}", keep.advertise_routes.join(",")))
        .arg(flag("ssh", keep.ssh));
    if let Some(f) = key_file {
        c = c.arg(format!("--auth-key=file:{f}"));
    }
    if let Some(h) = req.hostname.as_deref().filter(|h| !h.is_empty()) {
        c = c.arg(format!("--hostname={h}"));
    }
    let server = req.login_server.as_deref().filter(|s| !s.is_empty()).or(control_url.filter(|u| !u.is_empty() && *u != DEFAULT_CONTROL));
    if let Some(s) = server {
        c = c.arg(format!("--login-server={s}"));
    }
    c.timeout(LOGIN_TIMEOUT + Duration::from_secs(30))
}

/// Tailscale's own coordination server (the default `--login-server`).
pub const DEFAULT_CONTROL: &str = "https://controlplane.tailscale.com";

/// `tailscale logout`: the node key is expired at the coordination server and the
/// connection ends.
pub fn logout() -> Cmd {
    cli().arg("logout").timeout(SHORT)
}

/// The problem with a login request, as `(field, message)`.
pub fn check_login(req: &TailscaleLogin) -> Result<(), (&'static str, String)> {
    if let Some(k) = req.auth_key.as_deref().filter(|k| !k.is_empty())
        && (k.len() > 256 || !k.bytes().all(|b| b.is_ascii_graphic()))
    {
        return Err(("auth_key", "not an auth key".into()));
    }
    if let Some(h) = req.hostname.as_deref().filter(|h| !h.is_empty())
        && !valid_hostname(h)
    {
        return Err(("hostname", "letters, digits and '-', at most 63".into()));
    }
    if let Some(s) = req.login_server.as_deref().filter(|s| !s.is_empty())
        && (!s.starts_with("https://")
            || s.len() > 255
            || s.len() <= "https://".len()
            || !s.bytes().all(|b| b.is_ascii_graphic()))
    {
        return Err(("login_server", "an https:// address".into()));
    }
    Ok(())
}

/// A device name: one DNS label.
pub fn valid_hostname(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 63
        && !h.starts_with('-')
        && !h.ends_with('-')
        && h.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Ranges an advertised route may not overlap: the tailnet's own (100.64.0.0/10,
/// fd7a:115c:a1e0::/48) and link-local (169.254.0.0/16, fe80::/10).
const RESERVED: &[(&str, u8)] = &[
    ("100.64.0.0", 10),
    ("fd7a:115c:a1e0::", 48),
    ("169.254.0.0", 16),
    ("fe80::", 10),
];

/// `a/pa` and `net/pn` share addresses (one contains the other).
fn overlaps(a: IpAddr, pa: u8, net: &str, pn: u8) -> bool {
    let Ok(b) = net.parse::<IpAddr>() else { return false };
    let p = pa.min(pn);
    match (a, b) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let m = if p == 0 { 0 } else { u32::MAX << (32 - p as u32) };
            u32::from(a) & m == u32::from(b) & m
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let m = if p == 0 { 0 } else { u128::MAX << (128 - p as u32) };
            u128::from(a) & m == u128::from(b) & m
        }
        _ => false,
    }
}

/// Normalizes the advertised routes (`192.168.1.0/24`): CIDR with host bits clear, no default
/// route (that is the exit node), nothing overlapping the tailnet or link-local ranges
/// (`100.0.0.0/8` included), no duplicates, at most 16.
pub fn normalize_routes(routes: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for r in routes.iter().map(|r| r.trim()).filter(|r| !r.is_empty()) {
        let (a, p) = r.split_once('/').ok_or_else(|| format!("{r}: not a subnet (a.b.c.d/n)"))?;
        let ip: IpAddr = a.parse().map_err(|_| format!("{r}: not an address"))?;
        let p: u8 = p.parse().map_err(|_| format!("{r}: not a prefix length"))?;
        let max = if ip.is_ipv4() { 32 } else { 128 };
        if p == 0 || p > max {
            return Err(format!("{r}: prefix length out of range"));
        }
        let clean = match ip {
            IpAddr::V4(v) => {
                let m = u32::MAX << (32 - p as u32);
                IpAddr::V4((u32::from(v) & m).into())
            }
            IpAddr::V6(v) => {
                let m = if p == 128 { u128::MAX } else { !(u128::MAX >> p) };
                IpAddr::V6((u128::from(v) & m).into())
            }
        };
        if clean != ip {
            return Err(format!("{r}: host bits set"));
        }
        if ip.is_loopback() || ip.is_multicast() || RESERVED.iter().any(|(net, len)| overlaps(ip, p, net, *len)) {
            return Err(format!("{r}: not a LAN subnet"));
        }
        let s = format!("{ip}/{p}");
        if !out.contains(&s) {
            out.push(s);
        }
    }
    if out.len() > 16 {
        return Err("at most 16 subnets".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_names_every_option() {
        let o = TailscaleOptions {
            accept_routes: false,
            accept_dns: false,
            exit_node: true,
            advertise_routes: vec!["192.168.1.0/24".into(), "10.0.0.0/8".into()],
            ssh: false,
        };
        assert_eq!(
            set(&o).display(),
            "/usr/bin/tailscale set --accept-routes=false --accept-dns=false --advertise-exit-node=true --advertise-routes=192.168.1.0/24,10.0.0.0/8 --ssh=false"
        );
        assert!(set(&TailscaleOptions::default()).display().contains("--advertise-routes= "));
    }

    #[test]
    fn login_keeps_the_key_off_the_command_line() {
        let req = TailscaleLogin {
            auth_key: Some("test-only-secret".into()),
            hostname: Some("nas".into()),
            login_server: None,
        };
        let keep = TailscaleOptions { ssh: true, ..Default::default() };
        let c = login(&req, &keep, Some(DEFAULT_CONTROL), Some("/var/lib/rynas/tailscale/authkey")).display();
        assert!(!c.contains("test-only-secret"));
        assert!(c.starts_with("/usr/bin/tailscale up --reset --timeout=600s"));
        assert!(c.contains("--auth-key=file:/var/lib/rynas/tailscale/authkey"));
        assert!(c.contains("--accept-dns=false") && c.contains("--ssh=true") && c.contains("--hostname=nas"));
        assert!(!c.contains("--login-server"));
        let again = login(&TailscaleLogin::default(), &keep, Some("https://hs.example.com"), None).display();
        assert!(!again.contains("auth-key"));
        assert!(again.ends_with("--login-server=https://hs.example.com"));
    }

    #[test]
    fn login_checks() {
        let ok = TailscaleLogin {
            auth_key: Some("test-only-key".into()),
            hostname: Some("nas-2".into()),
            login_server: Some("https://hs.example.com".into()),
        };
        assert!(check_login(&ok).is_ok());
        let bad = |f: fn(&mut TailscaleLogin)| {
            let mut r = ok.clone();
            f(&mut r);
            check_login(&r).unwrap_err().0
        };
        assert_eq!(bad(|r| r.auth_key = Some("a b".into())), "auth_key");
        assert_eq!(bad(|r| r.hostname = Some("nas.local".into())), "hostname");
        assert_eq!(bad(|r| r.login_server = Some("http://x".into())), "login_server");
        assert!(check_login(&TailscaleLogin::default()).is_ok());
    }

    #[test]
    fn routes_are_lan_subnets() {
        let ok = normalize_routes(&[" 192.168.1.0/24".into(), "192.168.1.0/24".into(), "fd00:1::/64".into(), String::new()]).unwrap();
        assert_eq!(ok, ["192.168.1.0/24", "fd00:1::/64"]);
        for bad in ["192.168.1.5/24", "0.0.0.0/0", "100.64.0.0/10", "10.0.0.0", "10.0.0.0/33", "x/8", "127.0.0.0/8", "100.0.0.0/8", "100.96.0.0/12", "64.0.0.0/2", "169.254.0.0/16", "169.254.10.0/24", "fe80::/64", "fd7a::/16", "fd7a:115c:a1e0:1::/64"] {
            assert!(normalize_routes(&[bad.into()]).is_err(), "{bad}");
        }
    }
}
