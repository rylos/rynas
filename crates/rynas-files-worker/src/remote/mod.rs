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
    /// Puts `from` in the place of `to` in one step, when the server can. `Ok(false)`: it
    /// cannot, nothing was changed (see [`swap_in`]).
    fn replace(&self, from: &[String], to: &[String]) -> io::Result<bool> {
        let _ = (from, to);
        Ok(false)
    }
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

/// Suffix of the hidden name an existing file waits under while a copy takes its place, on a
/// server that cannot replace a file in one step ([`swap_in`]).
const ASIDE_TAG: &str = "rynas-aside";

/// The hidden name `name` waits under: ".name.rynas-aside".
pub fn aside_name(name: &str) -> String {
    crate::names::part_os(std::ffi::OsStr::new(name), ASIDE_TAG).to_string_lossy().into_owned()
}

/// The name a file set aside had, when `name` is one ([`aside_name`]).
pub fn aside_of(name: &str) -> Option<&str> {
    name.strip_prefix('.')?.strip_suffix(ASIDE_TAG)?.strip_suffix('.').filter(|n| !n.is_empty())
}

/// A file could not take the place of the old one. `keep_part`: the old file could not be put
/// back, so the finished new copy is the only complete one left and must stay.
pub struct ReplaceFailed {
    pub error: io::Error,
    pub keep_part: bool,
}

/// Replaces `target` with the finished `part` where the server has no replace in one step: the
/// old file goes `aside`, the new one takes its name, the old one is removed; put back when
/// the new one could not go there. At no point is either copy deleted before the other is in
/// place. `Ok(Some(e))`: the new file is in place, the old one stayed behind as `aside`.
pub fn swap_in<P: ?Sized>(part: &P, target: &P, aside: &P, rename: impl Fn(&P, &P) -> io::Result<()>, remove: impl Fn(&P) -> io::Result<()>) -> Result<Option<io::Error>, ReplaceFailed> {
    rename(target, aside).map_err(|error| ReplaceFailed { error, keep_part: false })?;
    match rename(part, target) {
        Ok(()) => Ok(remove(aside).err()),
        Err(error) => Err(ReplaceFailed { error, keep_part: rename(aside, target).is_err() }),
    }
}

/// [`Fs::replace`], or [`swap_in`] where the server cannot: for callers without a job's
/// reconnects. `keep_part` set: the part must not be removed.
pub fn replace_file(fs: &dyn Fs, part: &[String], target: &[String], keep_part: &std::cell::Cell<bool>) -> io::Result<()> {
    if fs.replace(part, target)? {
        return Ok(());
    }
    let (name, dir) = target.split_last().ok_or_else(|| errno(libc::EINVAL))?;
    let mut aside = dir.to_vec();
    aside.push(aside_name(name));
    // left by a copy stopped hard, the file it stands for being there
    if fs.stat(target).is_ok() {
        let _ = fs.remove(&aside, false);
    }
    match swap_in::<[String]>(part, target, &aside, |a, b| fs.rename(a, b), |a| fs.remove(a, false)) {
        Ok(_) => Ok(()),
        Err(f) => {
            keep_part.set(f.keep_part);
            Err(f.error)
        }
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

    /// A folder of named files in memory; a rename whose source ends in `refuse` fails.
    #[derive(Default)]
    struct Folder {
        files: std::sync::Mutex<std::collections::BTreeMap<String, &'static str>>,
        refuse: Vec<&'static str>,
    }

    impl Fs for Folder {
        fn list(&self, _: &[String]) -> io::Result<Vec<Entry>> {
            Ok(Vec::new())
        }
        fn stat(&self, p: &[String]) -> io::Result<Entry> {
            if self.files.lock().unwrap().contains_key(&p[0]) { Ok(Entry { name: p[0].clone(), kind: 'f', size: 1, mtime: 0, mode: 0 }) } else { Err(errno(libc::ENOENT)) }
        }
        fn reader(&self, _: &[String]) -> io::Result<Box<dyn Reader>> {
            Err(errno(libc::ENOSYS))
        }
        fn writer(&self, _: &[String], _: Option<u64>) -> io::Result<Box<dyn Writer>> {
            Err(errno(libc::ENOSYS))
        }
        fn mkdir(&self, _: &[String]) -> io::Result<()> {
            Ok(())
        }
        fn rename(&self, from: &[String], to: &[String]) -> io::Result<()> {
            if self.refuse.iter().any(|r| from[0].ends_with(r)) {
                return Err(errno(libc::EIO));
            }
            let mut f = self.files.lock().unwrap();
            let v = f.remove(&from[0]).ok_or_else(|| errno(libc::ENOENT))?;
            f.insert(to[0].clone(), v);
            Ok(())
        }
        fn remove(&self, p: &[String], _: bool) -> io::Result<()> {
            self.files.lock().unwrap().remove(&p[0]);
            Ok(())
        }
        fn space(&self, _: &[String]) -> Option<(u64, u64)> {
            None
        }
        fn set_mtime(&self, _: &[String], _: i64) {}
    }

    fn folder(refuse: Vec<&'static str>) -> Folder {
        let f = Folder { refuse, ..Default::default() };
        f.files.lock().unwrap().extend([("a.txt".to_string(), "old"), ("p".to_string(), "new")]);
        f
    }

    fn state(f: &Folder) -> Vec<(String, &'static str)> {
        f.files.lock().unwrap().iter().map(|(k, v)| (k.clone(), *v)).collect()
    }

    fn one(name: &str) -> Vec<String> {
        vec![name.to_string()]
    }

    #[test]
    fn replacing_without_a_servers_replace_leaves_only_the_new_file() {
        let (f, keep) = (folder(vec![]), std::cell::Cell::new(false));
        replace_file(&f, &one("p"), &one("a.txt"), &keep).unwrap();
        assert_eq!(state(&f), vec![("a.txt".to_string(), "new")]);
        assert!(!keep.get());
    }

    #[test]
    fn a_replace_that_fails_puts_the_old_file_back() {
        let (f, keep) = (folder(vec!["p"]), std::cell::Cell::new(false));
        assert!(replace_file(&f, &one("p"), &one("a.txt"), &keep).is_err());
        assert_eq!(state(&f), vec![("a.txt".to_string(), "old"), ("p".to_string(), "new")]);
        assert!(!keep.get(), "the old file is back: the part may go");
    }

    #[test]
    fn a_replace_that_cannot_restore_keeps_the_part() {
        let (f, keep) = (folder(vec!["p", "aside"]), std::cell::Cell::new(false));
        assert!(replace_file(&f, &one("p"), &one("a.txt"), &keep).is_err());
        assert!(keep.get(), "the old file is not back: the part stays");
        assert!(state(&f).iter().any(|(k, v)| k == ".a.txt.rynas-aside" && *v == "old"));
        assert!(state(&f).iter().any(|(k, v)| k == "p" && *v == "new"));
    }

    #[test]
    fn the_name_of_a_file_set_aside() {
        assert_eq!(aside_name("a.txt"), ".a.txt.rynas-aside");
        assert_eq!(aside_of(".a.txt.rynas-aside"), Some("a.txt"));
        assert_eq!(aside_of(".a.txt.rynas-part"), None);
        assert_eq!(aside_of("a.txt"), None);
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
