//! Route diagnostics ("Test" on a route): each step says what it found, in order of the
//! request's path: the name resolves to this NAS, the proxy runs, the route is loaded, the
//! HTTPS port listens, the certificate covers the name, the upstream answers. The domain
//! checklist (wave W2) also asks given resolvers directly ([`NetProbe::query`]): the LAN's
//! (from `/etc/resolv.conf`) and public ones, to tell DNS rebind protection apart.

use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::mpsc;
use std::time::Duration;

use rynas_api_types::web_access::{CheckState, RouteCheck};
use rynas_dns::Rtype;
use rynas_dns::wire::Answer;

/// Network probes, replaceable in tests.
pub trait NetProbe: Send + Sync {
    /// Addresses of a host name through the system resolver (nss-mdns for `.local`).
    fn resolve(&self, name: &str) -> Result<Vec<IpAddr>, String>;
    /// A TCP connection to `addr`.
    fn connect(&self, addr: SocketAddr) -> Result<(), String>;
    /// One DNS question to `server` (no system resolver in between).
    fn query(&self, server: SocketAddr, name: &str, rtype: Rtype) -> Result<Answer, String> {
        let _ = (server, name, rtype);
        Err("direct DNS queries are not available".into())
    }
    /// The LAN resolvers (`nameserver` lines of `/etc/resolv.conf`).
    fn lan_resolvers(&self) -> Vec<SocketAddr> {
        Vec::new()
    }
}

/// The running system's resolver and TCP stack, with timeouts.
pub struct SystemProbe;

impl NetProbe for SystemProbe {
    fn resolve(&self, name: &str) -> Result<Vec<IpAddr>, String> {
        let (tx, rx) = mpsc::channel();
        let n = name.to_string();
        // getaddrinfo has no timeout of its own
        std::thread::spawn(move || {
            let r = (n.as_str(), 443u16)
                .to_socket_addrs()
                .map(|it| it.map(|a| a.ip()).collect::<Vec<_>>())
                .map_err(|e| e.to_string());
            let _ = tx.send(r);
        });
        rx.recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| Err("no answer within 5 s".into()))
    }

    fn connect(&self, addr: SocketAddr) -> Result<(), String> {
        TcpStream::connect_timeout(&addr, Duration::from_secs(3))
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn query(&self, server: SocketAddr, name: &str, rtype: Rtype) -> Result<Answer, String> {
        rynas_dns::wire::query(server, name, rtype, Duration::from_secs(4))
    }

    fn lan_resolvers(&self) -> Vec<SocketAddr> {
        let text = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
        let mut v = rynas_dns::wire::nameservers(&text);
        // systemd-resolved's stub forwards to the LAN's server: ask the upstream ones too
        if v.iter().all(|a| a.ip().is_loopback())
            && let Ok(up) = std::fs::read_to_string("/run/systemd/resolve/resolv.conf")
        {
            v.extend(rynas_dns::wire::nameservers(&up));
        }
        v
    }
}

pub fn check(id: &str, state: CheckState, detail: impl Into<String>) -> RouteCheck {
    RouteCheck {
        id: id.into(),
        state,
        detail: detail.into(),
    }
}
