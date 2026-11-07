//! Route diagnostics ("Test" on a route): each step says what it found, in order of the
//! request's path: the name resolves to this NAS, the proxy runs, the route is loaded, the
//! HTTPS port listens, the certificate covers the name, the upstream answers.

use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::mpsc;
use std::time::Duration;

use rynas_api_types::web_access::{CheckState, RouteCheck};

/// Network probes, replaceable in tests.
pub trait NetProbe: Send + Sync {
    /// Addresses of a host name through the system resolver (nss-mdns for `.local`).
    fn resolve(&self, name: &str) -> Result<Vec<IpAddr>, String>;
    /// A TCP connection to `addr`.
    fn connect(&self, addr: SocketAddr) -> Result<(), String>;
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
}

pub fn check(id: &str, state: CheckState, detail: impl Into<String>) -> RouteCheck {
    RouteCheck {
        id: id.into(),
        state,
        detail: detail.into(),
    }
}
