//! HTTPS on rynas-web's own port (`:8443`, D-PROXY: the emergency door that never goes through
//! the reverse proxy; wave W0 of `docs/research/web-access.md`).
//!
//! - rustls with the ring provider; the certificate comes from the files rynasd publishes
//!   (`/etc/rynas/tls/ui.crt` + `ui.key`, see `rynasd::tls`) through a [`Resolver`] whose
//!   pair is swapped in place: a new certificate takes effect on the next handshake, without a
//!   restart. [`CertReloader`] re-reads the files when their size, mtime or inode change
//!   (checked every 10 s), on `SIGHUP`, and right after an import through the API. A pair whose
//!   key does not match its chain (caught between the two renames of an update) is ignored
//!   until the next check.
//! - Until a pair is loaded the port is open but handshakes fail, and plain HTTP is not
//!   redirected.
//! - [`serve`]: one task per connection, TLS handshake bounded by [`HANDSHAKE_TIMEOUT`], then
//!   HTTP/1.1 (ALPN `http/1.1`) on the same router as the plain listener, with the peer and
//!   local address ([`ConnAddrs`], IPv4-mapped addresses as IPv4) and the [`Https`] marker that
//!   makes the session cookie `Secure`.
//! - [`redirect`]: on the plain listener, `307` to `https://<same host>:<port>` once a
//!   certificate is loaded, except for loopback peers (a reverse proxy on the same host, SSH
//!   tunnels), requests a proxy marked `X-Forwarded-Proto: https`, and `--no-https-redirect`.
//!   `307` and `Cache-Control: no-store`: browsers do not remember it, so turning HTTPS off
//!   again never leaves a cached redirect to a closed port.
//! - No HSTS on this port: with a certificate the browser does not trust, HSTS would remove
//!   the "accept the risk" button and lock the administrator out (RFC 6797 §12.1).

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::extract::connect_info::ConnectInfo;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hyper_util::rt::{TokioIo, TokioTimer};
use rustls::ServerConfig;
use rustls::crypto::CryptoProvider;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::TcpListener;
use tower::ServiceExt;

use crate::conn::ConnAddrs;

/// Default certificate chain and key, published by rynasd.
pub const DEFAULT_CERT: &str = "/etc/rynas/tls/ui.crt";
pub const DEFAULT_KEY: &str = "/etc/rynas/tls/ui.key";
/// Longest TLS handshake (slow or idle clients).
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the files are checked for a change.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(10);

/// Request extension: the request came over HTTPS (the session cookie gets `Secure`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Https;

/// The certificate served to every client, swapped in place on reload.
#[derive(Debug, Default)]
pub struct Resolver {
    current: RwLock<Option<Arc<CertifiedKey>>>,
}

impl ResolvesServerCert for Resolver {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Size, mtime and inode of both files: a change of any of them triggers a reload.
type Stamp = [(u64, Option<SystemTime>, u64); 2];

fn stamp_of(paths: [&PathBuf; 2]) -> Option<Stamp> {
    use std::os::unix::fs::MetadataExt;
    let one = |p: &PathBuf| {
        std::fs::metadata(p)
            .ok()
            .map(|m| (m.len(), m.modified().ok(), m.ino()))
    };
    Some([one(paths[0])?, one(paths[1])?])
}

/// Loads the certificate files into the [`Resolver`] and keeps them current.
pub struct CertReloader {
    cert: PathBuf,
    key: PathBuf,
    resolver: Arc<Resolver>,
    stamp: Mutex<Option<Stamp>>,
    provider: Arc<CryptoProvider>,
}

impl CertReloader {
    pub fn new(cert: PathBuf, key: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            cert,
            key,
            resolver: Arc::default(),
            stamp: Mutex::new(None),
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        })
    }

    /// A certificate is loaded: HTTPS answers, plain HTTP may redirect.
    pub fn ready(&self) -> bool {
        self.resolver.resolve_any().is_some()
    }

    /// The loaded pair (tests, diagnostics).
    pub fn current(&self) -> Option<Arc<CertifiedKey>> {
        self.resolver.resolve_any()
    }

    /// Reloads when the files changed since the last load. `Ok(true)`: a new pair is served.
    /// On an error the previous pair stays.
    pub fn reload(&self) -> Result<bool, String> {
        self.reload_inner(false)
    }

    /// Reloads in any case (after an import or a `SIGHUP`).
    pub fn reload_now(&self) -> Result<bool, String> {
        self.reload_inner(true)
    }

    fn reload_inner(&self, force: bool) -> Result<bool, String> {
        let mut last = self.stamp.lock().unwrap_or_else(|e| e.into_inner());
        let now = stamp_of([&self.cert, &self.key]);
        if now.is_none() {
            return Err(format!(
                "{} or {} missing",
                self.cert.display(),
                self.key.display()
            ));
        }
        if !force && now == *last && self.ready() {
            return Ok(false);
        }
        let ck = self.load()?;
        *self
            .resolver
            .current
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(ck);
        *last = now;
        Ok(true)
    }

    fn load(&self) -> Result<Arc<CertifiedKey>, String> {
        let chain =
            std::fs::read(&self.cert).map_err(|e| format!("{}: {e}", self.cert.display()))?;
        let key = std::fs::read(&self.key).map_err(|e| format!("{}: {e}", self.key.display()))?;
        let certs = CertificateDer::pem_slice_iter(&chain)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("{}: {e:?}", self.cert.display()))?;
        if certs.is_empty() {
            return Err(format!("{}: no certificate", self.cert.display()));
        }
        let key = PrivateKeyDer::from_pem_slice(&key)
            .map_err(|e| format!("{}: {e:?}", self.key.display()))?;
        let signer = self
            .provider
            .key_provider
            .load_private_key(key)
            .map_err(|e| format!("{}: {e}", self.key.display()))?;
        let ck = CertifiedKey::new(certs, signer);
        ck.keys_match().map_err(|_| {
            "the key does not match the certificate (update in progress?)".to_string()
        })?;
        Ok(Arc::new(ck))
    }

    /// The rustls configuration of the HTTPS listener (TLS 1.2 and 1.3, HTTP/1.1).
    pub fn server_config(&self) -> Arc<ServerConfig> {
        let mut c = ServerConfig::builder_with_provider(self.provider.clone())
            .with_safe_default_protocol_versions()
            .expect("the ring provider supports the default versions")
            .with_no_client_auth()
            .with_cert_resolver(self.resolver.clone());
        c.alpn_protocols = vec![b"http/1.1".to_vec()];
        Arc::new(c)
    }
}

impl Resolver {
    fn resolve_any(&self) -> Option<Arc<CertifiedKey>> {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Re-checks the files every `every` and on `SIGHUP`.
pub fn spawn_watch(r: Arc<CertReloader>, every: Duration) {
    tokio::spawn(async move {
        let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();
        let mut failing = false;
        loop {
            let forced = tokio::select! {
                _ = tokio::time::sleep(every) => false,
                Some(_) = async {
                    match hup.as_mut() {
                        Some(h) => h.recv().await,
                        None => std::future::pending().await,
                    }
                } => true,
            };
            let rr = r.clone();
            let res =
                tokio::task::spawn_blocking(
                    move || {
                        if forced { rr.reload_now() } else { rr.reload() }
                    },
                )
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
            match res {
                Ok(true) => {
                    failing = false;
                    tracing::info!("https: certificate reloaded");
                }
                Ok(false) => failing = false,
                // one miss is normal (between the two renames of an update); say it once
                Err(e) if !failing => {
                    failing = true;
                    tracing::debug!("https: certificate not reloaded: {e}");
                }
                Err(e) => tracing::warn!("https: certificate not reloaded: {e}"),
            }
        }
    });
}

/// Binds the HTTPS listener. An unspecified IPv6 address (`[::]`) is bound dual-stack
/// (`IPV6_V6ONLY` off, whatever `net.ipv6.bindv6only` says), and falls back to `0.0.0.0` on a
/// kernel without IPv6.
pub async fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
    if let SocketAddr::V6(v6) = addr
        && v6.ip().is_unspecified()
    {
        match bind_dual(addr.port()) {
            Ok(l) => return TcpListener::from_std(l),
            Err(e) if e.raw_os_error() == Some(libc::EAFNOSUPPORT) => {
                tracing::warn!("https: no IPv6 in this kernel, listening on IPv4 only");
                return TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, addr.port())).await;
            }
            Err(e) => return Err(e),
        }
    }
    TcpListener::bind(addr).await
}

fn bind_dual(port: u16) -> io::Result<std::net::TcpListener> {
    use std::os::fd::FromRawFd;
    let check = |r: libc::c_int| {
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(r)
        }
    };
    // SAFETY: plain socket calls on a descriptor owned here; it is wrapped in a TcpListener
    // (which closes it) right after creation, so every early return closes it too.
    unsafe {
        let fd = check(libc::socket(
            libc::AF_INET6,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        ))?;
        let listener = std::net::TcpListener::from_raw_fd(fd);
        let off: libc::c_int = 0;
        let on: libc::c_int = 1;
        let len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        check(libc::setsockopt(
            fd,
            libc::IPPROTO_IPV6,
            libc::IPV6_V6ONLY,
            &off as *const _ as *const libc::c_void,
            len,
        ))?;
        check(libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            &on as *const _ as *const libc::c_void,
            len,
        ))?;
        let mut sa: libc::sockaddr_in6 = std::mem::zeroed();
        sa.sin6_family = libc::AF_INET6 as libc::sa_family_t;
        sa.sin6_port = port.to_be();
        check(libc::bind(
            fd,
            &sa as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
        ))?;
        check(libc::listen(fd, 1024))?;
        Ok(listener)
    }
}

/// IPv4-mapped IPv6 (`::ffff:a.b.c.d`, what a dual-stack socket reports for IPv4 clients) as
/// plain IPv4: the login rate limit groups IPv6 by /64, which would put every IPv4 client in
/// one bucket.
pub fn canonical(a: SocketAddr) -> SocketAddr {
    match a {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), v6.port()),
            None => a,
        },
        v4 => v4,
    }
}

/// Serves `app` over TLS on `listener` until the process ends.
pub async fn serve(listener: TcpListener, app: Router, config: Arc<ServerConfig>) {
    let acceptor = tokio_rustls::TlsAcceptor::from(config);
    loop {
        let (tcp, remote) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                // out of descriptors and the like: do not spin
                tracing::warn!("https: accept: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let addrs = ConnAddrs {
            remote: canonical(remote),
            local: tcp.local_addr().ok().map(canonical),
        };
        let acceptor = acceptor.clone();
        let app = app.clone();
        tokio::spawn(async move {
            let _ = tcp.set_nodelay(true);
            let stream = match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    tracing::debug!("https: handshake with {}: {e}", addrs.remote);
                    return;
                }
                Err(_) => return,
            };
            let svc = hyper::service::service_fn(move |mut req: Request<hyper::body::Incoming>| {
                req.extensions_mut().insert(ConnectInfo(addrs));
                req.extensions_mut().insert(Https);
                app.clone().oneshot(req)
            });
            let conn = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .serve_connection(TokioIo::new(stream), svc)
                .with_upgrades();
            if let Err(e) = conn.await {
                tracing::debug!("https: connection with {}: {e}", addrs.remote);
            }
        });
    }
}

/// Settings of the plain-HTTP redirect.
pub struct Redirect {
    /// The HTTPS port in the `Location`.
    pub port: u16,
    pub reloader: Arc<CertReloader>,
}

fn peer(req: &Request) -> Option<IpAddr> {
    req.extensions()
        .get::<ConnectInfo<ConnAddrs>>()
        .map(|c| c.0.remote.ip())
        .or_else(|| {
            req.extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0.ip())
        })
        .map(|ip| canonical(SocketAddr::new(ip, 0)).ip())
}

/// `https://<host of the request>[:port]<path and query>`, or `None` for a missing or odd
/// `Host` (then the request is served as it is).
pub fn location(host: &str, port: u16, path_and_query: &str) -> Option<String> {
    let name = if let Some(rest) = host.strip_prefix('[') {
        let (inner, after) = rest.split_once(']')?;
        if inner.parse::<std::net::Ipv6Addr>().is_err()
            || !(after.is_empty()
                || after
                    .strip_prefix(':')
                    .is_some_and(|p| p.parse::<u16>().is_ok()))
        {
            return None;
        }
        &host[..inner.len() + 2]
    } else {
        let name = match host.split_once(':') {
            Some((n, p)) if p.parse::<u16>().is_ok() => n,
            Some(_) => return None,
            None => host,
        };
        let ok = !name.is_empty()
            && name.len() <= 253
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_');
        if !ok {
            return None;
        }
        name
    };
    let pq = if path_and_query.starts_with('/') {
        path_and_query
    } else {
        "/"
    };
    Some(if port == 443 {
        format!("https://{name}{pq}")
    } else {
        format!("https://{name}:{port}{pq}")
    })
}

/// Middleware of the plain listener: see the module documentation.
pub async fn redirect(State(cfg): State<Arc<Redirect>>, req: Request, next: Next) -> Response {
    let proxied_https = req
        .headers()
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("https"));
    let remote = peer(&req);
    if cfg.reloader.ready() && !proxied_https && remote.is_some_and(|ip| !ip.is_loopback()) {
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .or_else(|| req.uri().authority().map(|a| a.as_str()));
        let pq = req
            .uri()
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/");
        if let Some(loc) = host.and_then(|h| location(h, cfg.port, pq))
            && let Ok(v) = HeaderValue::from_str(&loc)
        {
            return (
                StatusCode::TEMPORARY_REDIRECT,
                [
                    (header::LOCATION, v),
                    (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
                ],
            )
                .into_response();
        }
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_keeps_host_and_path_and_swaps_port() {
        assert_eq!(
            location("nas.local:8080", 8443, "/storage?x=1").as_deref(),
            Some("https://nas.local:8443/storage?x=1")
        );
        assert_eq!(
            location("192.0.2.10", 8443, "/").as_deref(),
            Some("https://192.0.2.10:8443/")
        );
        assert_eq!(
            location("[2001:db8::1]:8080", 8443, "/a").as_deref(),
            Some("https://[2001:db8::1]:8443/a")
        );
        assert_eq!(location("nas", 443, "/").as_deref(), Some("https://nas/"));
        // odd hosts are not redirected
        for bad in [
            "",
            "evil.example/x",
            "a@b",
            "[zz]",
            "2001:db8::1",
            "nas:80:80",
            "[::1]x",
        ] {
            assert_eq!(location(bad, 8443, "/"), None, "{bad}");
        }
    }

    #[test]
    fn mapped_ipv4_is_plain_ipv4() {
        let a: SocketAddr = "[::ffff:192.0.2.7]:5000".parse().unwrap();
        assert_eq!(
            canonical(a),
            "192.0.2.7:5000".parse::<SocketAddr>().unwrap()
        );
        let b: SocketAddr = "[2001:db8::1]:5000".parse().unwrap();
        assert_eq!(canonical(b), b);
    }
}
