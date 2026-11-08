use crate::error::*;
use crate::{SmbTransport, SmbTransportRead, SmbTransportWrite};

#[cfg(feature = "async")]
use futures_core::future::BoxFuture;
use maybe_async::*;
use std::net::SocketAddr;
use std::time::Duration;

#[cfg(feature = "async")]
use futures_util::FutureExt;
#[cfg(not(feature = "async"))]
use std::{
    io::{self, Read, Write},
    net::TcpStream,
};
#[cfg(feature = "async")]
use tokio::{
    io::{self, AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, tcp},
    select,
};

use binrw::prelude::*;
#[cfg(not(feature = "async"))]
use crate::SmbTcpMessageHeader;

#[cfg(feature = "async")]
type TcpRead = tcp::OwnedReadHalf;
#[cfg(feature = "async")]
type TcpWrite = tcp::OwnedWriteHalf;

#[cfg(not(feature = "async"))]
type TcpRead = TcpStream;
#[cfg(not(feature = "async"))]
type TcpWrite = TcpStream;
pub struct TcpTransport {
    reader: Option<TcpRead>,
    writer: Option<TcpWrite>,
    timeout: Duration,
}

impl TcpTransport {
    pub const DEFAULT_PORT: u16 = 445;

    pub fn new(timeout: Duration) -> TcpTransport {
        TcpTransport {
            reader: None,
            writer: None,
            timeout,
        }
    }

    /// Connects to a NetBios server in the specified endpoint with a timeout.
    /// This is the threaded version of [connect](NetBiosClient::connect) -
    /// using the [std::net::TcpStream] as the underlying socket provider.
    #[cfg(not(feature = "async"))]
    fn connect_timeout(&mut self, endpoint: &SocketAddr) -> Result<TcpStream> {
        if self.timeout == Duration::ZERO {
            log::debug!("Connecting to {endpoint}.");
            return TcpStream::connect(endpoint).map_err(Into::into);
        }

        log::debug!("Connecting to {endpoint} with timeout {:?}.", self.timeout);
        TcpStream::connect_timeout(endpoint, self.timeout).map_err(|e| match e.kind() {
            io::ErrorKind::TimedOut => {
                log::error!("Connection timed out after {:?}", self.timeout);
                TransportError::Timeout(self.timeout)
            }
            _ => {
                log::error!("Failed to connect to {endpoint}: {e}");
                e.into()
            }
        })
    }

    /// Connects to a NetBios server in the specified endpoint with a timeout.
    /// This is the async version of [connect](NetBiosClient::connect) -
    /// using the [tokio::net::TcpStream] as the underlying socket provider.
    #[cfg(feature = "async")]
    async fn connect_timeout(&mut self, endpoint: &SocketAddr) -> Result<TcpStream> {
        if self.timeout == Duration::ZERO {
            log::debug!("Connecting to {endpoint}.",);
            return TcpStream::connect(&endpoint).await.map_err(Into::into);
        }

        log::debug!("Connecting to {endpoint} with timeout {:?}.", self.timeout);
        select! {
            res = TcpStream::connect(&endpoint) => res.map_err(Into::into),
            _ = tokio::time::sleep(self.timeout) => Err(
                TransportError::Timeout(self.timeout)
            ),
        }
    }

    /// Async implementation of split socket to read and write halves.
    #[cfg(feature = "async")]
    fn split_socket(socket: TcpStream) -> (TcpRead, TcpWrite) {
        let (r, w) = socket.into_split();
        (r, w)
    }

    /// Sync implementation of split socket to read and write halves.
    #[cfg(not(feature = "async"))]
    fn split_socket(socket: TcpStream) -> (TcpRead, TcpWrite) {
        let rsocket = socket.try_clone().unwrap();
        let wsocket = socket;

        (rsocket, wsocket)
    }

    /// For synchronous implementations, gets the read timeout for the connection.
    #[cfg(not(feature = "async"))]
    pub fn read_timeout(&self) -> Result<Option<std::time::Duration>> {
        self.reader
            .as_ref()
            .ok_or(TransportError::NotConnected)?
            .read_timeout()
            .map_err(|e| e.into())
    }

    /// Maps a TCP error to a crate error.
    /// Connection aborts and unexpected EOFs are mapped to [Error::NotConnected].
    #[inline]
    fn map_tcp_error(e: io::Error) -> TransportError {
        if e.kind() == io::ErrorKind::ConnectionAborted || e.kind() == io::ErrorKind::UnexpectedEof
        {
            log::error!("Got IO error: {e} -- Connection Error, notify NotConnected!");
            return TransportError::NotConnected;
        }
        if e.kind() == io::ErrorKind::WouldBlock {
            log::trace!("Got IO error: {e} -- with ErrorKind::WouldBlock.");
        } else {
            log::error!("Got IO error: {e} -- Mapping to IO error.",);
        }
        e.into()
    }

    #[maybe_async]
    #[inline]
    async fn receive_exact(&mut self, out_buf: &mut [u8]) -> Result<()> {
        let reader = self.reader.as_mut().ok_or(TransportError::NotConnected)?;
        log::trace!("Reading {} bytes.", out_buf.len());
        reader
            .read_exact(out_buf)
            .await
            .map_err(Self::map_tcp_error)?;
        log::trace!("Read {} bytes OK.", out_buf.len());
        Ok(())
    }

    /// rynas patch, see `receive`: fills `buf`; a read timeout ends it (WouldBlock) only
    /// when `may_idle` and nothing was read yet. A message that stops in the middle for
    /// longer than the transport's timeout ends the connection (NotConnected): a peer
    /// frozen halfway still answers TCP keepalives, and the stream cannot be resumed.
    #[cfg(not(feature = "async"))]
    fn read_full(&mut self, buf: &mut [u8], may_idle: bool) -> Result<()> {
        let limit = if self.timeout.is_zero() { Duration::from_secs(60) } else { self.timeout };
        let reader = self.reader.as_mut().ok_or(TransportError::NotConnected)?;
        let mut got = 0;
        let mut last = std::time::Instant::now();
        while got < buf.len() {
            match reader.read(&mut buf[got..]) {
                Ok(0) => return Err(TransportError::NotConnected),
                Ok(n) => {
                    got += n;
                    last = std::time::Instant::now();
                }
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                    if may_idle && got == 0 {
                        return Err(Self::map_tcp_error(io::Error::from(io::ErrorKind::WouldBlock)));
                    }
                    if last.elapsed() >= limit {
                        log::error!("A message stopped halfway for {limit:?}: connection lost.");
                        return Err(TransportError::NotConnected);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Self::map_tcp_error(e)),
            }
        }
        Ok(())
    }

    #[maybe_async::maybe_async]
    #[inline]
    async fn send_raw(&mut self, message: &[u8]) -> Result<()> {
        log::trace!("Sending {} bytes.", message.len());
        let writer = self.writer.as_mut().ok_or(TransportError::NotConnected)?;
        writer
            .write_all(message)
            .await
            .map_err(Self::map_tcp_error)?;
        Ok(())
    }

    #[maybe_async::maybe_async]
    #[inline]
    async fn do_connect(&mut self, _server_name: &str, server_address: SocketAddr) -> Result<()> {
        let socket = self.connect_timeout(&server_address).await?;
        // rynas patch: no Nagle. A request goes out as two writes (the 4-byte transport
        // header, then the message); with Nagle the second waits for the ACK of the first,
        // which the server delays by up to 40 ms: every request round trip cost that.
        let _ = socket.set_nodelay(true);
        // rynas patch: the first answer (the multi-protocol negotiation is read here, before
        // the connection's worker and its watchdog exist) waits no longer than a connect
        #[cfg(not(feature = "async"))]
        if !self.timeout.is_zero() {
            let _ = socket.set_read_timeout(Some(self.timeout));
        }
        let (r, w) = Self::split_socket(socket);
        self.reader = Some(r);
        self.writer = Some(w);
        Ok(())
    }
}

impl SmbTransport for TcpTransport {
    #[cfg(feature = "async")]
    fn connect<'a>(
        &'a mut self,
        server_name: &'a str,
        server_address: SocketAddr,
    ) -> BoxFuture<'a, Result<()>> {
        self.do_connect(server_name, server_address).boxed()
    }
    #[cfg(not(feature = "async"))]
    fn connect(&mut self, server_name: &str, server_address: SocketAddr) -> Result<()> {
        self.do_connect(server_name, server_address)
    }

    fn split(self: Box<Self>) -> Result<(Box<dyn SmbTransportRead>, Box<dyn SmbTransportWrite>)> {
        Ok((
            Box::new(Self {
                reader: self.reader,
                writer: None,
                timeout: self.timeout,
            }),
            Box::new(Self {
                reader: None,
                writer: self.writer,
                timeout: self.timeout,
            }),
        ))
    }

    fn default_port(&self) -> u16 {
        Self::DEFAULT_PORT
    }

    fn remote_address(&self) -> Result<SocketAddr> {
        self.reader
            .as_ref()
            .ok_or(TransportError::NotConnected)?
            .peer_addr()
            .map_err(Into::into)
    }
}

impl SmbTransportWrite for TcpTransport {
    #[cfg(feature = "async")]
    fn send_raw<'a>(&'a mut self, buf: &'a [u8]) -> BoxFuture<'a, Result<()>> {
        self.send_raw(buf).boxed()
    }
    #[cfg(not(feature = "async"))]
    fn send_raw(&mut self, buf: &[u8]) -> Result<()> {
        self.send_raw(buf)
    }
}

impl SmbTransportRead for TcpTransport {
    #[cfg(feature = "async")]
    fn receive_exact<'a>(&'a mut self, out_buf: &'a mut [u8]) -> BoxFuture<'a, Result<()>> {
        self.receive_exact(out_buf).boxed()
    }
    #[cfg(not(feature = "async"))]
    fn receive_exact(&mut self, out_buf: &mut [u8]) -> Result<()> {
        self.receive_exact(out_buf)
    }

    /// rynas patch: the receive loop polls with a short read timeout (to notice a stop), and
    /// the default `receive` reads the header and the body with `read_exact`, which drops
    /// the bytes it already read when the timeout hits in the middle of a message: the
    /// stream then lost its framing and the connection was dead for good (a server that
    /// pauses for more than the poll interval inside a large response, a busy disk, a slow
    /// link). Here a timeout counts only before the first byte of a message; once a message
    /// has begun it is read to its end (a peer that vanishes ends it with an error: TCP
    /// keepalive and the user timeout are set on the socket).
    #[cfg(not(feature = "async"))]
    fn receive(&mut self) -> Result<Vec<u8>> {
        let mut header_data = [0; SmbTcpMessageHeader::SIZE];
        self.read_full(&mut header_data, true)?;
        let header = SmbTcpMessageHeader::read(&mut std::io::Cursor::new(header_data))?;
        let mut data = vec![0; header.stream_protocol_length as usize];
        self.read_full(&mut data, false)?;
        Ok(data)
    }

    #[cfg(not(feature = "async"))]
    fn set_read_timeout(&self, timeout: std::time::Duration) -> Result<()> {
        self.reader
            .as_ref()
            .ok_or(TransportError::NotConnected)?
            .set_read_timeout(Some(timeout))
            .map_err(|e| e.into())
    }
}
