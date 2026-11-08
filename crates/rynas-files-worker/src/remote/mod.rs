//! Remote servers: SMB and SFTP, browsed and copied to and from like the shared folders.
//! The connections themselves (paths `/:<id>/…`, the store) are the core's
//! (`rynas_files::conns`); this module adds the clients.

pub mod api;
pub mod sftp;
pub mod smb;
mod srvsvc;

use std::io::{self, Read, Seek, Write};
use std::time::Duration;

use serde::Serialize;

pub use rynas_files::conns::*;

/// Opens a connection to its server.
pub trait Connect {
    fn connect(&self) -> io::Result<Box<dyn Fs>>;
}

impl Connect for Conn {
    fn connect(&self) -> io::Result<Box<dyn Fs>> {
        Ok(match self.kind {
            Kind::Smb => Box::new(smb::Smb::connect(self)?),
            Kind::Sftp => Box::new(sftp::Sftp::connect(self)?),
        })
    }
}

/// An item on a server, the way fsops lists the local ones.
#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub name: String,
    /// d directory, f file, l symlink
    pub kind: char,
    pub size: u64,
    pub mtime: i64,
    pub mode: u32,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == 'd'
    }
}

#[allow(clippy::len_without_is_empty)]
pub trait Reader: Read + Seek + Send {
    fn len(&self) -> u64;
}

pub trait Writer: Write + Send {
    /// Flushes and closes; an error here means the data may not be there.
    fn finish(self: Box<Self>) -> io::Result<()>;
}

/// A server, as the API and the worker use it. Paths are the parts below
/// the connection: for SMB the share first.
pub trait Fs: Send {
    fn list(&self, dir: &[String]) -> io::Result<Vec<Entry>>;
    fn stat(&self, path: &[String]) -> io::Result<Entry>;
    fn reader(&self, path: &[String]) -> io::Result<Box<dyn Reader>>;
    /// `None`: a new file, replacing any; `Some(n)`: an existing file,
    /// written from byte n on (an upload that resumes).
    fn writer(&self, path: &[String], at: Option<u64>) -> io::Result<Box<dyn Writer>>;
    fn mkdir(&self, path: &[String]) -> io::Result<()>;
    /// Never replaces: fails when `to` exists.
    fn rename(&self, from: &[String], to: &[String]) -> io::Result<()>;
    fn remove(&self, path: &[String], dir: bool) -> io::Result<()>;
    /// free and total bytes where `path` is
    fn space(&self, path: &[String]) -> Option<(u64, u64)>;
    fn set_mtime(&self, path: &[String], mtime: i64);
    /// whether a path names the server itself (SMB: the list of shares)
    fn is_top(&self, path: &[String]) -> bool {
        let _ = path;
        false
    }
}

/// How long a server may leave one request without an answer before the transfer gives up
/// on the connection (and reconnects, see `jobs::remote_jobs`).
pub const REMOTE_TIMEOUT: Duration = Duration::from_secs(60);

/// The error of a server that stopped answering: the job shows it, with Retry.
pub fn stalled(limit: Duration) -> io::Error {
    rynas_files::fsops::coded(
        io::ErrorKind::TimedOut,
        rynas_api_types::error_codes::REMOTE_UNREACHABLE,
        t!("the remote server stopped answering (no reply in {} s)", limit.as_secs()),
    )
}

/// Runs a blocking call of a client on a thread of its own and waits at most `limit` for
/// it. A call still blocked then is left behind (it holds only its own clones of the
/// connection, which the caller drops) and [`stalled`] is returned: a client library that
/// waits forever (the smb crate on credits, a read whose answer never comes) can no longer
/// hang the job, its cancel or its pause.
pub fn timed<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> io::Result<T> + Send + 'static) -> io::Result<T> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new().name("remote-call".into()).spawn(move || {
        let _ = tx.send(f());
    })?;
    match rx.recv_timeout(limit) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(stalled(limit)),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::other(t!("the remote call failed"))),
    }
}

/// Whether an error is the connection's (timed out, reset, closed) rather than the
/// server's answer about a file: worth a new connection and another try from where the
/// copy stopped.
pub fn retryable(e: &io::Error) -> bool {
    use io::ErrorKind as K;
    matches!(e.kind(), K::TimedOut | K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe | K::NotConnected | K::UnexpectedEof)
        || matches!(e.raw_os_error(), Some(libc::ETIMEDOUT | libc::ECONNRESET | libc::ECONNABORTED | libc::EPIPE | libc::ENOTCONN | libc::EHOSTUNREACH | libc::ENETUNREACH))
}

/// TCP keepalive and a user timeout on every TCP socket of this process: the SMB and SSH
/// libraries open theirs without, so a peer that vanishes behind a router or VPN (or a
/// path whose MTU swallows large segments) is noticed by the kernel within about a minute
/// instead of never. The worker has no other TCP sockets; descriptors that are not TCP
/// refuse the options harmlessly.
pub fn tune_sockets() {
    let set = |fd: i32, level: i32, opt: i32, value: i32| {
        // SAFETY: setsockopt with a pointer to a local int and its size
        unsafe { libc::setsockopt(fd, level, opt, (&value as *const i32).cast(), std::mem::size_of::<i32>() as libc::socklen_t) == 0 }
    };
    let get = |fd: i32, opt: i32| -> Option<i32> {
        let mut value: i32 = 0;
        let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
        // SAFETY: getsockopt into a local int; fails with ENOTSOCK/EBADF for other descriptors
        let ok = unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, opt, (&mut value as *mut i32).cast(), &mut len) } == 0;
        ok.then_some(value)
    };
    for fd in 3..1024 {
        if get(fd, libc::SO_TYPE) != Some(libc::SOCK_STREAM) || !matches!(get(fd, libc::SO_DOMAIN), Some(libc::AF_INET | libc::AF_INET6)) {
            continue;
        }
        if get(fd, libc::SO_KEEPALIVE) == Some(1) {
            continue;
        }
        set(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1);
        set(fd, libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, 20);
        set(fd, libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, 10);
        set(fd, libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 3);
        // data sent and not acknowledged for this long ends the connection
        set(fd, libc::IPPROTO_TCP, libc::TCP_USER_TIMEOUT, REMOTE_TIMEOUT.as_millis() as i32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_that_never_returns_is_given_up() {
        let start = std::time::Instant::now();
        let e = timed(Duration::from_millis(200), || -> io::Result<()> {
            std::thread::sleep(Duration::from_secs(3600));
            Ok(())
        })
        .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(retryable(&e));
        assert_eq!(timed(Duration::from_secs(5), || Ok(7)).unwrap(), 7);
        assert_eq!(timed(Duration::from_secs(5), || -> io::Result<()> { Err(errno(libc::ENOENT)) }).unwrap_err().raw_os_error(), Some(libc::ENOENT));
    }

    #[test]
    fn only_connection_errors_are_worth_a_new_connection() {
        assert!(retryable(&io::Error::from(io::ErrorKind::ConnectionReset)));
        assert!(retryable(&errno(libc::ETIMEDOUT)));
        assert!(!retryable(&errno(libc::ENOENT)));
        assert!(!retryable(&errno(libc::EACCES)));
        assert!(!retryable(&io::Error::other("verification failed")));
    }

    #[test]
    fn sockets_get_keepalive() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let s = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        tune_sockets();
        let fd = std::os::fd::AsRawFd::as_raw_fd(&s);
        let mut v: i32 = 0;
        let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
        // SAFETY: getsockopt into a local int
        unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE, (&mut v as *mut i32).cast(), &mut len) };
        assert_eq!(v, 1);
    }
}
