//! The plain HTTP listener and its address (D-PROXY: once rynas-caddy fronts the interface,
//! the plain port is its upstream on loopback only).
//!
//! `--listen` takes an address, kept as it is, or `auto[:PORT]` (the package default, port
//! 8080):
//! - while the reverse proxy does **not** front the interface, `0.0.0.0:PORT`: an upgraded
//!   install without rynas-caddy, external proxy mode, or before the first apply keeps the
//!   plain port reachable from the network (where it redirects to HTTPS once a certificate is
//!   loaded);
//! - once it does — rynasd publishes the interface's names in the proxy state file
//!   (`/run/rynas/web-access.ui`, see `rynasd::web_access::UI_STATE`) — **and** the HTTPS port
//!   is listening with a certificate, `127.0.0.1:PORT` only. The emergency door `:8443` must
//!   work before the plain port leaves the network: with HTTPS off, not bound or without a
//!   certificate, the plain port stays where it is, so nobody is locked out.
//!
//! The state is checked when inotify reports a change in its folder, and every
//! [`crate::fswatch::SAFETY_NET`] (every [`CHECK_INTERVAL`] where inotify is missing). A move stops accepting on the old socket
//! (open connections, event streams included, carry on) and binds the new one; when the new
//! address cannot be bound, the old one is bound again and the move retried on the next check.
//! Caddy dials `127.0.0.1:PORT` in both states, so the proxy never notices.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::Request;
use axum::extract::connect_info::ConnectInfo;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tower::ServiceExt;

use crate::conn::ConnAddrs;

/// Where rynasd publishes the names the proxy fronts the interface on.
pub const DEFAULT_PROXY_STATE: &str = "/run/rynas/web-access.ui";
/// Port of `auto` without one.
pub const DEFAULT_PORT: u16 = 8080;
/// How often the proxy state is read where inotify is missing.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// `--listen`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlainListen {
    /// Always this address.
    Fixed(SocketAddr),
    /// The network until the proxy fronts the interface, then loopback (module docs).
    Auto(u16),
}

impl std::str::FromStr for PlainListen {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s == "auto" {
            return Ok(Self::Auto(DEFAULT_PORT));
        }
        if let Some(p) = s.strip_prefix("auto:") {
            return p
                .parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .map(Self::Auto)
                .ok_or_else(|| format!("{s}: not a port"));
        }
        s.parse().map(Self::Fixed).map_err(|e| format!("{s}: {e}"))
    }
}

impl PlainListen {
    /// The address to listen on now.
    pub fn address(self, fronted: bool, https_ready: bool) -> SocketAddr {
        match self {
            Self::Fixed(a) => a,
            Self::Auto(port) if fronted && https_ready => {
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
            }
            Self::Auto(port) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port),
        }
    }
}

/// Whether the proxy fronts the interface: the state file names at least one host.
pub fn fronted(state: &std::path::Path) -> bool {
    std::fs::read_to_string(state).is_ok_and(|s| s.lines().any(|l| !l.trim().is_empty()))
}

/// The plain listener: see the module documentation.
pub struct Plain {
    pub mode: PlainListen,
    pub app: Router,
    pub proxy_state: PathBuf,
    /// Whether the HTTPS port listens with a certificate loaded.
    pub https_ready: Arc<dyn Fn() -> bool + Send + Sync>,
    /// How often the state is read ([`CHECK_INTERVAL`]).
    pub check: Duration,
}

struct Running {
    /// The address asked for (the socket's own address differs for port 0).
    want: SocketAddr,
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

/// Binds `addr`, retrying for a moment while the socket just closed is released.
async fn bind_retry(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let mut last = None;
    for _ in 0..40 {
        match TcpListener::bind(addr).await {
            Ok(l) => return Ok(l),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                last = Some(e);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.expect("at least one attempt"))
}

/// How long a connection may wait for a request head. hyper counts it on an idle keep-alive
/// connection too, and the reverse proxy in front of this port keeps its connections for two
/// minutes: a shorter wait here would close one just as the proxy sends a request on it.
const HEAD_WAIT: Duration = Duration::from_secs(150);

/// Serves `app` over plain HTTP/1.1 on `listener` until `stop` (sent or dropped): then the
/// socket closes at once and open connections finish their requests. One task per
/// connection, as on the HTTPS port ([`crate::tls::serve`]), with hyper's timer: a client
/// that does not send a request head within [`HEAD_WAIT`] is disconnected, so connections
/// left open without a request do not pile up.
async fn serve(listener: TcpListener, app: Router, mut stop: oneshot::Receiver<()>) {
    let (closing, _) = watch::channel(false);
    let mut conns = JoinSet::new();
    loop {
        let (tcp, remote) = tokio::select! {
            _ = &mut stop => break,
            // finished connections leave the set as they end
            Some(_) = conns.join_next() => continue,
            r = listener.accept() => match r {
                Ok(c) => c,
                Err(e) => {
                    // out of descriptors and the like: do not spin
                    tracing::warn!("http: accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        let addrs = ConnAddrs {
            remote: crate::tls::canonical(remote),
            local: tcp.local_addr().ok().map(crate::tls::canonical),
        };
        let app = app.clone();
        let mut closing = closing.subscribe();
        conns.spawn(async move {
            let svc = hyper::service::service_fn(move |mut req: Request<hyper::body::Incoming>| {
                req.extensions_mut().insert(ConnectInfo(addrs));
                app.clone().oneshot(req)
            });
            let conn = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HEAD_WAIT)
                .serve_connection(TokioIo::new(tcp), svc)
                .with_upgrades();
            let mut conn = std::pin::pin!(conn);
            let r = tokio::select! {
                r = conn.as_mut() => r,
                _ = closing.changed() => {
                    conn.as_mut().graceful_shutdown();
                    conn.await
                }
            };
            if let Err(e) = r {
                tracing::debug!("http: connection with {}: {e}", addrs.remote);
            }
        });
    }
    drop(listener);
    let _ = closing.send(true);
    while conns.join_next().await.is_some() {}
}

impl Plain {
    fn desired(&self) -> SocketAddr {
        match self.mode {
            PlainListen::Fixed(a) => a,
            auto => auto.address(fronted(&self.proxy_state), (self.https_ready)()),
        }
    }

    fn start(&self, listener: TcpListener, want: SocketAddr) -> Running {
        let (stop, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(serve(listener, self.app.clone(), rx));
        Running { want, stop, task }
    }

    /// Binds the first address (an error ends the process, as before) and returns the
    /// socket's address with the future that serves, and moves the listener, until
    /// `shutdown`; then it stops accepting and gives open connections `grace` to finish.
    pub async fn bind(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
        grace: Duration,
    ) -> std::io::Result<(SocketAddr, impl Future<Output = ()> + Send + 'static)> {
        let want = self.desired();
        let listener = TcpListener::bind(want).await?;
        let local = listener.local_addr()?;
        let running = self.start(listener, want);
        Ok((local, self.run(running, shutdown, grace)))
    }

    async fn run(
        self,
        mut current: Running,
        shutdown: impl Future<Output = ()> + Send + 'static,
        grace: Duration,
    ) {
        let mut shutdown = std::pin::pin!(shutdown);
        // the proxy state is written by rynasd: inotify on its folder wakes the check, the
        // HTTPS readiness (a certificate loaded) is seen within the safety net; without
        // inotify the state is read every `check` as before
        let auto = matches!(self.mode, PlainListen::Auto(_));
        let mut changes = if auto { self.proxy_state.parent().and_then(|d| crate::fswatch::watch_dirs(&[d])) } else { None };
        loop {
            tokio::select! {
                _ = crate::fswatch::next(&mut changes, self.check), if auto => {}
                _ = &mut shutdown => break,
            }
            let want = self.desired();
            if want != current.want {
                current = self.switch(current, want).await;
            }
        }
        let _ = current.stop.send(());
        // event streams never end by themselves: do not wait for them more than a moment
        if tokio::time::timeout(grace, current.task).await.is_err() {
            tracing::info!("shutdown: closing open streams");
        }
    }

    /// Moves the listener from `current` to `want`; on failure back to the old address.
    async fn switch(&self, current: Running, want: SocketAddr) -> Running {
        let old = current.want;
        // the old server drops its socket as soon as it sees the signal; its connections
        // carry on in their own tasks
        let _ = current.stop.send(());
        drop(current.task);
        tokio::task::yield_now().await;
        match bind_retry(want).await {
            Ok(l) => {
                if want.ip().is_loopback() {
                    tracing::info!(
                        "http: the reverse proxy fronts the interface, plain HTTP now on {want} only (HTTPS stays on its own port)"
                    );
                } else {
                    tracing::info!(
                        "http: the reverse proxy does not front the interface, plain HTTP on {want} again"
                    );
                }
                return self.start(l, want);
            }
            Err(e) => tracing::error!("http: cannot listen on {want}: {e}; staying on {old}"),
        }
        loop {
            match bind_retry(old).await {
                Ok(l) => return self.start(l, old),
                Err(e) => {
                    tracing::error!("http: cannot listen on {old} again: {e}; retrying");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_values() {
        assert_eq!("auto".parse(), Ok(PlainListen::Auto(8080)));
        assert_eq!("auto:9080".parse(), Ok(PlainListen::Auto(9080)));
        assert!("auto:0".parse::<PlainListen>().is_err());
        assert!("auto:x".parse::<PlainListen>().is_err());
        assert_eq!(
            "0.0.0.0:8080".parse(),
            Ok(PlainListen::Fixed("0.0.0.0:8080".parse().unwrap()))
        );
        assert!("nas:8080".parse::<PlainListen>().is_err());
    }

    #[test]
    fn auto_leaves_the_network_only_with_the_proxy_and_https() {
        let lan: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        let lo: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let a = PlainListen::Auto(8080);
        assert_eq!(a.address(false, false), lan);
        assert_eq!(a.address(false, true), lan);
        // no emergency door yet: the plain port stays on the network
        assert_eq!(a.address(true, false), lan);
        assert_eq!(a.address(true, true), lo);
        // an explicit address is the administrator's choice
        let f = PlainListen::Fixed(lan);
        assert_eq!(f.address(true, true), lan);
    }

    #[test]
    fn proxy_state_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("web-access.ui");
        assert!(!fronted(&p));
        std::fs::write(&p, "\n \n").unwrap();
        assert!(!fronted(&p));
        std::fs::write(&p, "nas.local\nnas\n").unwrap();
        assert!(fronted(&p));
    }
}
