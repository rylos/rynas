//! tailscaled's LocalAPI over its Unix socket: `GET /localapi/v0/status` and
//! `GET /localapi/v0/prefs`, both marked stable in Tailscale's client package. One request
//! per connection (HTTP/1.0, so the body is never chunked), with the `Host` the server
//! expects and no `Origin`/`Referer` (it refuses those). Read only.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use rynas_api_types::tailscale::{TailscaleOptions, TailscaleState, TailscaleStatus};
use serde_json::Value;

/// The `Host` tailscaled accepts on its socket.
pub const HOST: &str = "local-tailscaled.sock";
/// Largest answer read (a big tailnet's status is a few hundred KiB).
const MAX: usize = 4 << 20;

/// `GET path` on the socket; the body of a 200 answer.
pub fn get(socket: &Path, path: &str, timeout: Duration) -> Result<String, String> {
    let mut s = UnixStream::connect(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    s.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    let req = format!("GET {path} HTTP/1.0\r\nHost: {HOST}\r\nTailscale-Cap: 1\r\nAccept: application/json\r\n\r\n");
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    s.take(MAX as u64 + 1).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    if buf.len() > MAX {
        return Err("LocalAPI answer too large".into());
    }
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text.split_once("\r\n\r\n").ok_or("malformed LocalAPI answer")?;
    let status = head.split_whitespace().nth(1).unwrap_or("");
    if status != "200" {
        return Err(format!("LocalAPI {path}: HTTP {status}: {}", body.trim()));
    }
    Ok(body.to_string())
}

/// The preferences rynas reads.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Prefs {
    pub options: TailscaleOptions,
    /// `ControlURL` (empty when never set).
    pub control_url: String,
}

const EXIT_V4: &str = "0.0.0.0/0";
const EXIT_V6: &str = "::/0";

/// The preferences from `GET /localapi/v0/prefs`.
pub fn parse_prefs(json: &str) -> Result<Prefs, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("prefs: {e}"))?;
    let routes: Vec<String> = v["AdvertiseRoutes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r.as_str().map(str::to_string))
        .collect();
    let exit_node = routes.iter().any(|r| r == EXIT_V4) && routes.iter().any(|r| r == EXIT_V6);
    Ok(Prefs {
        options: TailscaleOptions {
            accept_routes: v["RouteAll"].as_bool().unwrap_or(false),
            accept_dns: v["CorpDNS"].as_bool().unwrap_or(false),
            exit_node,
            advertise_routes: routes.into_iter().filter(|r| r != EXIT_V4 && r != EXIT_V6).collect(),
            ssh: v["RunSSH"].as_bool().unwrap_or(false),
        },
        control_url: v["ControlURL"].as_str().unwrap_or("").to_string(),
    })
}

fn state_of(s: &str) -> TailscaleState {
    match s {
        "NoState" => TailscaleState::NoState,
        "NeedsLogin" => TailscaleState::NeedsLogin,
        "NeedsMachineAuth" => TailscaleState::NeedsMachineAuth,
        "Stopped" => TailscaleState::Stopped,
        "Starting" => TailscaleState::Starting,
        "Running" => TailscaleState::Running,
        _ => TailscaleState::NoState,
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect()
}

/// RFC 3339 time (`2027-05-01T10:00:00Z`) → ms since the epoch.
fn time_ms(v: &Value) -> Option<u64> {
    let t: jiff::Timestamp = v.as_str()?.parse().ok()?;
    u64::try_from(t.as_millisecond()).ok()
}

/// Fills `out` from `GET /localapi/v0/status` and the preferences: state, version, names,
/// addresses, peers, key expiry, options, routes waiting for approval, health.
pub fn fill_status(out: &mut TailscaleStatus, status_json: &str, prefs: Option<&Prefs>) -> Result<(), String> {
    let v: Value = serde_json::from_str(status_json).map_err(|e| format!("status: {e}"))?;
    out.running = true;
    out.installed = true;
    out.state = state_of(v["BackendState"].as_str().unwrap_or(""));
    out.version = v["Version"].as_str().filter(|s| !s.is_empty()).map(|s| s.split('-').next().unwrap_or(s).to_string());
    out.update_available = (v["ClientVersion"]["RunningLatest"] == false)
        .then(|| v["ClientVersion"]["LatestVersion"].as_str().map(str::to_string))
        .flatten()
        .filter(|s| !s.is_empty());
    out.auth_url = v["AuthURL"].as_str().filter(|s| s.starts_with("https://")).map(str::to_string);
    let tn = &v["CurrentTailnet"];
    out.tailnet = tn["Name"].as_str().filter(|s| !s.is_empty()).map(str::to_string);
    out.magic_dns = tn["MagicDNSEnabled"].as_bool().unwrap_or(false);
    let me = &v["Self"];
    out.dns_name = me["DNSName"].as_str().map(|s| s.trim_end_matches('.').to_string()).filter(|s| !s.is_empty());
    out.hostname = me["HostName"].as_str().filter(|s| !s.is_empty()).map(str::to_string);
    out.addresses = strings(&v["TailscaleIPs"]);
    out.online = me["Online"].as_bool().unwrap_or(false);
    out.key_expiry_ms = time_ms(&me["KeyExpiry"]);
    if let Some(peers) = v["Peer"].as_object() {
        out.peers = peers.len().min(u32::MAX as usize) as u32;
        out.peers_online = peers.values().filter(|p| p["Online"] == true).count().min(u32::MAX as usize) as u32;
    }
    out.health = strings(&v["Health"]).into_iter().take(10).collect();
    if let Some(p) = prefs {
        out.options = p.options.clone();
        // advertised routes the admin console has not approved: not in AllowedIPs (approved
        // exit routes and subnet routes show there) nor in PrimaryRoutes
        if out.state == TailscaleState::Running {
            let approved: Vec<String> = strings(&me["AllowedIPs"]).into_iter().chain(strings(&me["PrimaryRoutes"])).collect();
            let mut wanted = p.options.advertise_routes.clone();
            if p.options.exit_node {
                wanted.push(EXIT_V4.into());
            }
            out.routes_pending_approval = wanted.into_iter().filter(|r| !approved.contains(r)).collect();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const STATUS: &str = r#"{
      "Version": "1.90.6-tdeadbeef-gcafe",
      "BackendState": "Running",
      "AuthURL": "",
      "TailscaleIPs": ["100.101.102.103", "fd7a:115c:a1e0::1234"],
      "Self": {"HostName": "nas", "DNSName": "nas.tail1234.ts.net.", "Online": true,
               "KeyExpiry": "2027-05-01T10:00:00Z",
               "AllowedIPs": ["100.101.102.103/32", "fd7a:115c:a1e0::1234/128", "192.168.1.0/24"],
               "PrimaryRoutes": ["192.168.1.0/24"]},
      "Health": ["some warning"],
      "CurrentTailnet": {"Name": "user@example.com", "MagicDNSSuffix": "tail1234.ts.net", "MagicDNSEnabled": true},
      "Peer": {"nodekey:a": {"Online": true}, "nodekey:b": {"Online": false}, "nodekey:c": {"Online": true}},
      "ClientVersion": {"RunningLatest": false, "LatestVersion": "1.92.1"}
    }"#;

    pub const PREFS: &str = r#"{"ControlURL": "https://controlplane.tailscale.com", "RouteAll": false,
      "CorpDNS": false, "RunSSH": false, "WantRunning": true,
      "AdvertiseRoutes": ["192.168.1.0/24", "10.1.0.0/16", "0.0.0.0/0", "::/0"]}"#;

    #[test]
    fn prefs_split_exit_node_and_subnets() {
        let p = parse_prefs(PREFS).unwrap();
        assert!(p.options.exit_node);
        assert_eq!(p.options.advertise_routes, ["192.168.1.0/24", "10.1.0.0/16"]);
        assert!(!p.options.accept_dns && !p.options.accept_routes && !p.options.ssh);
        assert_eq!(p.control_url, "https://controlplane.tailscale.com");
        let none = parse_prefs(r#"{"AdvertiseRoutes": null, "CorpDNS": true}"#).unwrap();
        assert!(none.options.accept_dns && !none.options.exit_node && none.options.advertise_routes.is_empty());
        assert!(parse_prefs("nope").is_err());
    }

    #[test]
    fn status_of_a_running_node() {
        let p = parse_prefs(PREFS).unwrap();
        let mut s = TailscaleStatus::default();
        fill_status(&mut s, STATUS, Some(&p)).unwrap();
        assert_eq!(s.state, TailscaleState::Running);
        assert_eq!(s.version.as_deref(), Some("1.90.6"));
        assert_eq!(s.update_available.as_deref(), Some("1.92.1"));
        assert_eq!(s.dns_name.as_deref(), Some("nas.tail1234.ts.net"));
        assert_eq!(s.tailnet.as_deref(), Some("user@example.com"));
        assert!(s.magic_dns && s.online && s.running && s.installed);
        assert_eq!((s.peers, s.peers_online), (3, 2));
        assert_eq!(s.addresses.len(), 2);
        assert_eq!(s.key_expiry_ms, Some(1_809_165_600_000));
        assert_eq!(s.auth_url, None);
        // the subnet is approved, the second subnet and the exit node are not
        assert_eq!(s.routes_pending_approval, ["10.1.0.0/16", "0.0.0.0/0"]);
    }

    #[test]
    fn status_waiting_for_login() {
        let j = r#"{"Version": "1.90.6", "BackendState": "NeedsLogin",
                    "AuthURL": "https://login.tailscale.com/a/abc123", "Self": {"Online": false},
                    "TailscaleIPs": null, "CurrentTailnet": null, "Peer": null, "ClientVersion": {"RunningLatest": true}}"#;
        let mut s = TailscaleStatus::default();
        fill_status(&mut s, j, None).unwrap();
        assert_eq!(s.state, TailscaleState::NeedsLogin);
        assert_eq!(s.auth_url.as_deref(), Some("https://login.tailscale.com/a/abc123"));
        assert_eq!(s.update_available, None);
        assert!(s.addresses.is_empty() && s.tailnet.is_none() && s.key_expiry_ms.is_none());
    }

    #[test]
    fn get_over_a_unix_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("ts.sock");
        let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let t = std::thread::spawn(move || {
            for code in ["200 OK", "403 Forbidden"] {
                let (mut c, _) = l.accept().unwrap();
                let mut req = Vec::new();
                let mut b = [0u8; 1];
                while !req.ends_with(b"\r\n\r\n") {
                    c.read_exact(&mut b).unwrap();
                    req.push(b[0]);
                }
                let req = String::from_utf8_lossy(&req).to_string();
                assert!(req.starts_with("GET /localapi/v0/status HTTP/1.0\r\n"));
                assert!(req.contains("Host: local-tailscaled.sock\r\n"));
                write!(c, "HTTP/1.0 {code}\r\nContent-Type: application/json\r\n\r\n{{\"BackendState\":\"Running\"}}").unwrap();
            }
        });
        let body = get(&sock, "/localapi/v0/status", Duration::from_secs(2)).unwrap();
        assert_eq!(body, r#"{"BackendState":"Running"}"#);
        assert!(get(&sock, "/localapi/v0/status", Duration::from_secs(2)).unwrap_err().contains("403"));
        t.join().unwrap();
        assert!(get(&dir.path().join("none.sock"), "/x", Duration::from_secs(1)).is_err());
    }
}
