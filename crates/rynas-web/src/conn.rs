//! Both ends of a client connection.
//!
//! axum's `ConnectInfo<SocketAddr>` carries only the peer. The network endpoints also need
//! the **local** address the connection arrived on (`getsockname`): it tells which interface
//! the administrator comes through (the management interface, D-NET) and whether a network
//! confirmation arrived over the new path. [`ConnAddrs`] captures both when the connection is
//! accepted; the [`expose`] middleware hands them to handlers as the usual
//! `ConnectInfo<SocketAddr>` (peer) plus a [`LocalAddr`] extension.

use std::net::SocketAddr;

use axum::extract::Request;
use axum::extract::connect_info::{ConnectInfo, Connected};
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
            remote: *stream.remote_addr(),
            local: stream.io().local_addr().ok(),
        }
    }
}

/// The local address of the request's connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalAddr(pub SocketAddr);

/// Middleware: `ConnectInfo<ConnAddrs>` → `ConnectInfo<SocketAddr>` + [`LocalAddr`]. A server
/// started with `ConnectInfo<SocketAddr>` (tests) passes through unchanged.
pub async fn expose(mut req: Request, next: Next) -> Response {
    if let Some(ConnectInfo(a)) = req.extensions().get::<ConnectInfo<ConnAddrs>>().copied() {
        req.extensions_mut().insert(ConnectInfo(a.remote));
        if let Some(l) = a.local {
            req.extensions_mut().insert(LocalAddr(l));
        }
    }
    next.run(req).await
}
