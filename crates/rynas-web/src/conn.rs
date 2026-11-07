//! Both ends of a client connection, and the client behind the local reverse proxy.
//!
//! axum's `ConnectInfo<SocketAddr>` carries only the peer. The network endpoints also need
//! the **local** address the connection arrived on (`getsockname`): it tells which interface
//! the administrator comes through (the management interface, D-NET) and whether a network
//! confirmation arrived over the new path. [`ConnAddrs`] captures both when the connection is
//! accepted; the [`expose`] middleware hands them to handlers as the usual
//! `ConnectInfo<SocketAddr>` (peer) plus a [`LocalAddr`] extension, and adds the request's
//! [`Client`].
//!
//! **Trusted proxy** (D-PROXY: rynas-caddy fronts the interface and relays it to the plain
//! port on loopback). `X-Forwarded-For` is honoured only when the connection came from the
//! reverse proxy on this host: plain HTTP (never the HTTPS port, which no proxy fronts),
//! loopback peer **and** loopback local address (the plain listener as Caddy dials it,
//! `127.0.0.1:<port>`). From any other connection — a LAN peer, `:8443` — the headers are
//! ignored and the client is the TCP peer. The address is the **right-most untrusted** entry
//! of every `X-Forwarded-For` field taken together (MDN, "Selecting an IP address"): entries
//! are walked from the right, loopback entries (the trusted proxy itself) are skipped, the
//! first other one is the client; an entry that is not an address stops the walk and the
//! peer is used. Caddy (2.11) replaces the `X-Forwarded-For` of clients it does not trust (it
//! has no `trusted_proxies`, being the edge) with the client's address, so the right-most
//! entry is the one it saw.

use std::net::{IpAddr, SocketAddr};

use axum::extract::Request;
use axum::extract::connect_info::{ConnectInfo, Connected};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use axum::serve::IncomingStream;

/// Peer and local address of an accepted TCP connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnAddrs {
    pub remote: SocketAddr,
    pub local: Option<SocketAddr>,
}

impl Connected<IncomingStream<'_, tokio::net::TcpListener>> for ConnAddrs {
    fn connect_info(stream: IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        ConnAddrs {
            remote: crate::tls::canonical(*stream.remote_addr()),
            local: stream.io().local_addr().ok().map(crate::tls::canonical),
        }
    }
}

/// The local address of the request's connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalAddr(pub SocketAddr);

/// The client of a request: the TCP peer, or the address the local reverse proxy forwarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Client {
    pub ip: IpAddr,
    /// The address came from the trusted proxy's `X-Forwarded-For`.
    pub proxied: bool,
}

fn canonical_ip(ip: IpAddr) -> IpAddr {
    crate::tls::canonical(SocketAddr::new(ip, 0)).ip()
}

/// Whether a connection's forwarding headers come from the reverse proxy on this host:
/// plain HTTP, loopback peer, loopback local address.
pub fn trusted_proxy(remote: SocketAddr, local: Option<SocketAddr>, https: bool) -> bool {
    !https
        && canonical_ip(remote.ip()).is_loopback()
        && local.is_some_and(|l| canonical_ip(l.ip()).is_loopback())
}

/// One `X-Forwarded-For` entry: an address, optionally with a port (`192.0.2.1:5000`,
/// `[2001:db8::1]:5000`) or in brackets.
fn parse_entry(e: &str) -> Option<IpAddr> {
    let e = e.trim();
    let ip = e
        .parse::<IpAddr>()
        .ok()
        .or_else(|| e.parse::<SocketAddr>().ok().map(|s| s.ip()))
        .or_else(|| {
            e.strip_prefix('[')
                .and_then(|r| r.strip_suffix(']'))
                .and_then(|r| r.parse::<IpAddr>().ok())
        })?;
    let ip = canonical_ip(ip);
    (!ip.is_unspecified() && !ip.is_multicast()).then_some(ip)
}

/// The right-most untrusted address of all `X-Forwarded-For` fields (loopback entries are
/// the trusted proxy and skipped). `None` without the header, when only loopback entries are
/// there, or when the walk meets an entry that is not an address.
pub fn forwarded_for(headers: &HeaderMap) -> Option<IpAddr> {
    let mut entries = Vec::new();
    for v in headers.get_all("x-forwarded-for") {
        entries.extend(v.to_str().ok()?.split(','));
    }
    for e in entries.iter().rev() {
        let ip = parse_entry(e)?;
        if !ip.is_loopback() {
            return Some(ip);
        }
    }
    None
}

/// The client of a connection: see the module documentation.
pub fn client_of(
    remote: SocketAddr,
    local: Option<SocketAddr>,
    https: bool,
    headers: &HeaderMap,
) -> Client {
    if trusted_proxy(remote, local, https)
        && let Some(ip) = forwarded_for(headers)
    {
        return Client { ip, proxied: true };
    }
    Client {
        ip: canonical_ip(remote.ip()),
        proxied: false,
    }
}

/// Middleware: `ConnectInfo<ConnAddrs>` → `ConnectInfo<SocketAddr>` + [`LocalAddr`], and the
/// request's [`Client`]. A server started with `ConnectInfo<SocketAddr>` (tests) gets the
/// peer as the client, headers never trusted.
pub async fn expose(mut req: Request, next: Next) -> Response {
    let https = req.extensions().get::<crate::tls::Https>().is_some();
    if let Some(ConnectInfo(a)) = req.extensions().get::<ConnectInfo<ConnAddrs>>().copied() {
        let client = client_of(a.remote, a.local, https, req.headers());
        req.extensions_mut().insert(ConnectInfo(a.remote));
        if let Some(l) = a.local {
            req.extensions_mut().insert(LocalAddr(l));
        }
        req.extensions_mut().insert(client);
    } else if let Some(ConnectInfo(a)) = req.extensions().get::<ConnectInfo<SocketAddr>>().copied()
    {
        req.extensions_mut().insert(Client {
            ip: canonical_ip(a.ip()),
            proxied: false,
        });
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn xff(values: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append("x-forwarded-for", HeaderValue::from_str(v).unwrap());
        }
        h
    }

    const PROXY: &str = "127.0.0.1:51000";
    const PLAIN: &str = "127.0.0.1:8080";

    #[test]
    fn right_most_untrusted_entry() {
        let real = Some(ip("192.0.2.7"));
        assert_eq!(forwarded_for(&xff(&["192.0.2.7"])), real);
        // a client-supplied left part never wins
        assert_eq!(forwarded_for(&xff(&["203.0.113.66, 192.0.2.7"])), real);
        // several fields are one list, in order
        assert_eq!(forwarded_for(&xff(&["203.0.113.66", "192.0.2.7"])), real);
        // loopback hops (the proxy itself) are skipped
        assert_eq!(forwarded_for(&xff(&["192.0.2.7, 127.0.0.1, ::1"])), real);
        // ports, brackets, IPv4-mapped IPv6
        assert_eq!(
            forwarded_for(&xff(&["[2001:db8::5]:443"])),
            Some(ip("2001:db8::5"))
        );
        assert_eq!(
            forwarded_for(&xff(&["[2001:db8::5]"])),
            Some(ip("2001:db8::5"))
        );
        assert_eq!(forwarded_for(&xff(&["192.0.2.7:5000"])), real);
        assert_eq!(forwarded_for(&xff(&["::ffff:192.0.2.7"])), real);
        // garbage on the right stops the walk: nothing is guessed from the left
        assert_eq!(forwarded_for(&xff(&["192.0.2.7, unknown"])), None);
        assert_eq!(forwarded_for(&xff(&["192.0.2.7,"])), None);
        assert_eq!(forwarded_for(&xff(&["0.0.0.0"])), None);
        assert_eq!(forwarded_for(&xff(&["127.0.0.1"])), None);
        assert_eq!(forwarded_for(&HeaderMap::new()), None);
    }

    #[test]
    fn headers_only_from_the_local_proxy_on_the_plain_port() {
        let h = xff(&["192.0.2.7"]);
        // Caddy on loopback, plain listener: honoured
        assert_eq!(
            client_of(sa(PROXY), Some(sa(PLAIN)), false, &h),
            Client {
                ip: ip("192.0.2.7"),
                proxied: true
            }
        );
        // the same over IPv6 loopback
        assert!(client_of(sa("[::1]:5"), Some(sa("[::1]:8080")), false, &h).proxied);
        // a LAN peer spoofing the header: ignored
        let lan = client_of(
            sa("192.0.2.50:40000"),
            Some(sa("192.0.2.1:8080")),
            false,
            &h,
        );
        assert_eq!(
            lan,
            Client {
                ip: ip("192.0.2.50"),
                proxied: false
            }
        );
        // a loopback peer on a non-loopback local address is refused too
        assert!(!client_of(sa(PROXY), Some(sa("192.0.2.1:8080")), false, &h).proxied);
        // the HTTPS port never trusts it, even from loopback
        assert!(!client_of(sa(PROXY), Some(sa("127.0.0.1:8443")), true, &h).proxied);
        // no local address known: not trusted
        assert!(!client_of(sa(PROXY), None, false, &h).proxied);
        // no header: the peer
        assert_eq!(
            client_of(sa(PROXY), Some(sa(PLAIN)), false, &HeaderMap::new()).ip,
            ip("127.0.0.1")
        );
        // IPv4-mapped peers are reported as IPv4
        assert_eq!(
            client_of(sa("[::ffff:192.0.2.50]:1"), None, true, &h).ip,
            ip("192.0.2.50")
        );
    }
}
