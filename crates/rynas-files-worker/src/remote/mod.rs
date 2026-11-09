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

/// What tells a file apart from the same name after a change: compared when a copy goes
/// on over a new connection, before it seeks to where it stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Ident {
    pub size: u64,
    /// last write time, in the server's own unit (SMB: FILETIME; SFTP: seconds)
    pub mtime: i64,
    /// SMB: the change time (FILETIME, metadata changes too); 0 where the server has none
    pub change: i64,
}

#[allow(clippy::len_without_is_empty)]
pub trait Reader: Read + Seek + Send {
    fn len(&self) -> u64;
    /// the file as it was opened
    fn ident(&self) -> Ident;
    /// The next bytes from the position, in a buffer of the reader's own: swapped into
    /// `buf` (whose old buffer the reader may keep), no copy. 0 at the end.
    fn next_block(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
        if buf.len() < 1 << 20 {
            buf.resize(1 << 20, 0);
        }
        let n = self.read(buf)?;
        buf.truncate(n);
        Ok(n)
    }
}

pub trait Writer: Write + Send {
    /// Waits for the writes still in flight, flushes and closes; an error here means the
    /// data may not be there.
    fn finish(self: Box<Self>) -> io::Result<()>;
    /// After a failed write (the connection broke): the blocks handed over that the server
    /// has not confirmed, by offset, for a new writer to send again. A writer that writes
    /// before `write` returns has none.
    fn unconfirmed(&mut self) -> Vec<(u64, std::sync::Arc<[u8]>)> {
        Vec::new()
    }
    /// Sends again blocks another writer of the same file did not get confirmed, each at its
    /// own offset (the position of `write` stays where it is).
    fn resend(&mut self, blocks: Vec<(u64, std::sync::Arc<[u8]>)>) -> io::Result<()> {
        if blocks.is_empty() { Ok(()) } else { Err(io::Error::from(io::ErrorKind::Unsupported)) }
    }
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

/// The progress watchdog of a connection: when requests wait and nothing at all came from
/// the server for this long, the connection is taken for dead (the transfer reconnects, see
/// `jobs::remote_jobs`). A slow server that keeps answering is never cut off, however long
/// a listing or a batch of reads takes.
pub const REMOTE_TIMEOUT: Duration = Duration::from_secs(60);

/// The error of a server that stopped answering: the job shows it, with Retry.
pub fn stalled(limit: Duration) -> io::Error {
    rynas_files::fsops::coded(
        io::ErrorKind::TimedOut,
        rynas_api_types::error_codes::REMOTE_UNREACHABLE,
        t!("the remote server stopped answering (no reply in {} s)", limit.as_secs()),
    )
}

/// The connection to the server broke (closed, reset, out of sync, refused while
/// reconnecting): the job shows it, with Retry. Only the clients make these, so an error of
/// the NAS's own disks or sockets is never taken for one ([`retryable`]).
pub fn lost(kind: io::ErrorKind, detail: impl std::fmt::Display) -> io::Error {
    rynas_files::fsops::coded(kind, rynas_api_types::error_codes::REMOTE_UNREACHABLE, t!("the connection to the remote server broke: {}", detail))
}

/// Whether an error is the remote connection's (it stopped answering or broke) rather than
/// the server's answer about a file or an error on the NAS's side: worth a new connection
/// and another try from where the copy stopped. Only the clients' own errors count
/// ([`stalled`], [`lost`]): a local ETIMEDOUT or EPIPE (a disk, a pipe) does not.
pub fn retryable(e: &io::Error) -> bool {
    rynas_files::fsops::io_code(e) == rynas_api_types::error_codes::REMOTE_UNREACHABLE
}

/// [`tune_socket`] on every TCP socket of this process: the SMB library opens its own
/// without these options; the worker has no other TCP sockets but SSH's, set up directly
/// (`sftp`). Descriptors that are not TCP refuse the options harmlessly.
pub fn tune_sockets() {
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
        tune_socket(fd);
    }
}

/// TCP keepalive and a user timeout on one socket: a peer that vanishes behind a router or
/// VPN (or a path whose MTU swallows large segments) is noticed within about a minute.
pub fn tune_socket(fd: i32) {
    let set = |level: i32, opt: i32, value: i32| {
        // SAFETY: setsockopt with a pointer to a local int and its size
        unsafe { libc::setsockopt(fd, level, opt, (&value as *const i32).cast(), std::mem::size_of::<i32>() as libc::socklen_t) == 0 }
    };
    // requests are small writes waiting for an answer: never held back for an ACK
    set(libc::IPPROTO_TCP, libc::TCP_NODELAY, 1);
    set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, 20);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, 10);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 3);
    // data sent and not acknowledged for this long ends the connection
    set(libc::IPPROTO_TCP, libc::TCP_USER_TIMEOUT, REMOTE_TIMEOUT.as_millis() as i32);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_connections_errors_are_worth_a_new_connection() {
        assert!(retryable(&stalled(Duration::from_secs(60))));
        assert!(retryable(&lost(io::ErrorKind::ConnectionReset, "reset")));
        // the NAS's own side: a local disk or pipe timing out is not the server's fault
        assert!(!retryable(&io::Error::from(io::ErrorKind::ConnectionReset)));
        assert!(!retryable(&errno(libc::ETIMEDOUT)));
        assert!(!retryable(&errno(libc::EPIPE)));
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
