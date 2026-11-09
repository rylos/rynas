//! SFTP servers, with russh. The library is async: each connection keeps a small runtime of
//! its own whose threads are always running (keepalives, window adjusts and rekeys never
//! depend on a caller waiting), and the rest of SynoFile stays plain threads.
//!
//! Reads of a large file run ahead of the copy in one continuous window of requests, kept
//! by a task on the runtime and delivered in order over a bounded channel; the thread that
//! writes the blocks to disk overlaps the network. Uploads keep a window of writes the same
//! way and know which blocks the server has not confirmed. Metadata goes over the raw
//! session as single requests.

use std::collections::BTreeMap;
use std::future::Future;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use futures_util::stream::{FuturesOrdered, StreamExt};
use russh::keys::ssh_key::{HashAlg, LineEnding};
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh_sftp::client::RawSftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::rawsession::Limits;
use russh_sftp::extensions;
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

use super::{Conn, Entry, Fs, Ident, Reader, Writer, errno};

/// How long connecting, logging in and opening the channel may each take.
const TIMEOUT: Duration = Duration::from_secs(20);
/// How long the file's final `fsync@openssh.com` may take. A server flushing a large file to
/// slow disks is not a failure, and a connection that died is noticed by the keepalives.
const FSYNC_LIMIT: Duration = Duration::from_secs(3600);
/// How long closing a file may hold up the caller (a connection that is gone must not).
const CLOSE_LIMIT: Duration = Duration::from_secs(5);

/// One request at most (OpenSSH's own: `limits@openssh.com` says 255 KiB); smaller when the
/// server says so.
const BLOCK: usize = 255 * 1024;
/// Requests in flight, reading and writing (OpenSSH's: 64): about 16 MiB outstanding.
const READS_IN_FLIGHT: usize = 64;
const WRITES_IN_FLIGHT: usize = 64;
/// Blocks read and waiting for the copy (about 4 MiB): with the reads in flight, the memory a
/// download takes is bounded near 20 MiB.
const READ_QUEUE: usize = 16;
/// Requests in flight are paced so that the last one waits at most this long behind those
/// before it: a request's own timeout must never fire because the link is slow.
const QUEUE_SECS: f64 = 20.0;
/// The smallest block a short read shrinks requests to.
const MIN_BLOCK: u32 = 32 * 1024;
/// The channel's flow-control window: larger than everything a download keeps in flight.
const WINDOW_BYTES: u32 = 32 * 1024 * 1024;
/// SSH packets as large as the protocol allows (russh: +13 % over its 32 KiB default).
const MAX_PACKET: u32 = 65535;

/// Whether to log each connection's terms: jobs do, browsing does not.
static LOG_TERMS: AtomicBool = AtomicBool::new(false);

/// Logs (to standard error, rynasd's journal) the terms of each new connection.
pub fn log_connections() {
    LOG_TERMS.store(true, Ordering::Relaxed);
}

/// What the server offers, and the block sizes it allows.
#[derive(Clone, Copy, Debug)]
struct Features {
    fsync: bool,
    statvfs: bool,
    read_block: u32,
    write_block: usize,
    limits: Option<Limits>,
}

impl Features {
    fn none() -> Self {
        Self { fsync: false, statvfs: false, read_block: BLOCK as u32, write_block: BLOCK, limits: None }
    }
}

pub struct Sftp {
    rt: Arc<Runtime>,
    sftp: Arc<RawSftpSession>,
    features: Features,
    _session: Option<russh::client::Handle<Check>>,
    /// the server's key as seen on this connection
    pub host_key: String,
    /// where the user lands, absolute
    pub home: String,
}

/// Accepts the server's key when it is the one remembered. When none is
/// yet, only where it can be remembered: in the package's own processes,
/// which keep the connections (see `remote::may_pin`). Anywhere else an
/// unknown key would be taken on trust again and again, never pinned.
pub struct Check {
    expected: String,
    learn: bool,
    seen: Arc<Mutex<String>>,
    /// what the key exchange agreed on, for the connection's log line
    names: Arc<Mutex<Option<russh::Names>>>,
}

impl Check {
    fn accepts(&self, seen: &str) -> bool {
        if self.expected.is_empty() { self.learn } else { self.expected == seen }
    }
}

impl russh::client::Handler for Check {
    type Error = russh::Error;
    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let seen = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.fingerprint(HashAlg::Sha256).to_string(),
            PublicKeyOrCertificate::Certificate(cert) => PublicKey::new(cert.public_key().clone(), "").fingerprint(HashAlg::Sha256).to_string(),
        };
        let ok = self.accepts(&seen);
        *self.seen.lock().unwrap() = seen;
        Ok(ok)
    }
    async fn kex_done(&mut self, _shared_secret: Option<&[u8]>, names: &russh::Names, _session: &mut russh::client::Session) -> Result<(), Self::Error> {
        *self.names.lock().unwrap() = Some(names.clone());
        Ok(())
    }
}

/// Why a server's key was refused.
fn unknown_key(expected: &str) -> io::Error {
    rynas_files::fsops::coded(
        io::ErrorKind::PermissionDenied,
        rynas_api_types::error_codes::HOST_KEY_CHANGED,
        if expected.is_empty() {
            t!("the server's key is not verified yet: open Edit connection and save")
        } else {
            t!("the server's key changed since the last connection: if that is right, tick “forget” in Edit connection")
        },
    )
}

/// The server's key as SFTP would see it, without logging in: for the
/// first connection, to pin it (`remote::pin_host_key`).
pub fn server_key(conn: &Conn, timeout: Duration) -> io::Result<String> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let seen = Arc::new(Mutex::new(String::new()));
    let check = Check { expected: String::new(), learn: true, seen: Arc::clone(&seen), names: Arc::default() };
    let port = if conn.port == 0 { 22 } else { conn.port };
    let host = conn.host.trim().to_string();
    rt.block_on(async {
        // the same ciphers as the connections it pins the key for
        let config = Arc::new(client_config());
        match tokio::time::timeout(timeout, russh::client::connect(config, (host.as_str(), port), check)).await {
            Err(_) => Err(errno(libc::ETIMEDOUT)),
            Ok(Err(e)) => Err(other(e)),
            Ok(Ok(session)) => {
                let _ = session.disconnect(russh::Disconnect::ByApplication, "", "").await;
                Ok(())
            }
        }
    })?;
    let key = seen.lock().unwrap().clone();
    if key.is_empty() { Err(other("no host key")) } else { Ok(key) }
}

fn other(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

/// The library's errors: the server's status about a file is that file's error; no answer
/// in time or a session that broke is the connection's ([`super::retryable`]).
fn map(e: SftpError) -> io::Error {
    match e {
        SftpError::Status(s) => match s.status_code {
            StatusCode::NoSuchFile => errno(libc::ENOENT),
            StatusCode::PermissionDenied => errno(libc::EACCES),
            StatusCode::Eof => io::Error::from(io::ErrorKind::UnexpectedEof),
            StatusCode::OpUnsupported => errno(libc::EOPNOTSUPP),
            StatusCode::NoConnection | StatusCode::ConnectionLost => super::lost(io::ErrorKind::ConnectionAborted, s.error_message),
            _ if s.error_message.is_empty() => other(s.status_code),
            _ => other(s.error_message),
        },
        // each request waits at most this long for its answer (the session's own timeout)
        SftpError::Timeout => super::stalled(super::REMOTE_TIMEOUT),
        // the packet stream is broken or out of sync: the session cannot be trusted any more
        // (russh-sftp ends its receive loop on any such error, which fails the waiting
        // requests here too)
        SftpError::IO(_) | SftpError::UnexpectedPacket | SftpError::UnexpectedBehavior(_) => super::lost(io::ErrorKind::ConnectionAborted, e),
        SftpError::Limited(_) => other(e),
    }
}

/// `limit` for a future, the timer created where it is polled: `tokio::time::timeout` built
/// outside the runtime panics.
async fn within<T>(limit: Duration, what: impl Future<Output = io::Result<T>>) -> io::Result<T> {
    match tokio::time::timeout(limit, what).await {
        Ok(r) => r,
        Err(_) => Err(errno(libc::ETIMEDOUT)),
    }
}

/// Whether the CPU encrypts AES in hardware: AES-GCM is then far cheaper than ChaCha20.
fn aes_accelerated() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("aes") && std::arch::is_x86_feature_detected!("pclmulqdq")
    }
    #[cfg(target_arch = "aarch64")]
    {
        std::arch::is_aarch64_feature_detected!("aes") && std::arch::is_aarch64_feature_detected!("pmull")
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        false
    }
}

/// The ciphers to offer, in order. With AES in hardware (no matter which vendor), the AES-GCM
/// ciphers first and the 128-bit one before the 256-bit one (fewer rounds, measured faster);
/// otherwise ChaCha20 first. The rest stays as russh's default list: a server that offers
/// nothing better still connects.
fn cipher_order(aes: bool) -> Vec<russh::cipher::Name> {
    use russh::cipher::{AES_128_CTR, AES_128_GCM, AES_192_CTR, AES_256_CTR, AES_256_GCM, CHACHA20_POLY1305};
    let mut order = if aes { vec![AES_128_GCM, AES_256_GCM, CHACHA20_POLY1305] } else { vec![CHACHA20_POLY1305, AES_128_GCM, AES_256_GCM] };
    order.extend([AES_256_CTR, AES_192_CTR, AES_128_CTR]);
    order
}

/// The connection's parameters: see [`Sftp::connect`].
fn client_config() -> russh::client::Config {
    let preferred = russh::Preferred {
        cipher: std::borrow::Cow::Owned(cipher_order(aes_accelerated())),
        // no compression: the files are not worth it and the CPU is the limit
        compression: std::borrow::Cow::Borrowed(&[russh::compression::NONE]),
        ..Default::default()
    };
    russh::client::Config {
        // keepalives: a server that vanished behind a router or VPN ends the session in
        // under a minute, and every request then fails instead of waiting
        inactivity_timeout: Some(Duration::from_secs(600)),
        keepalive_interval: Some(Duration::from_secs(15)),
        keepalive_max: 3,
        window_size: WINDOW_BYTES,
        maximum_packet_size: MAX_PACKET,
        nodelay: true,
        preferred,
        ..Default::default()
    }
}

/// The socket a session runs on: `TCP_NODELAY` (requests are small writes waiting for an
/// answer) and the options `remote::tune_socket` sets.
async fn tcp(host: &str, port: u16) -> io::Result<tokio::net::TcpStream> {
    let stream = within(TIMEOUT, async { tokio::net::TcpStream::connect((host, port)).await }).await?;
    stream.set_nodelay(true)?;
    super::tune_socket(stream.as_raw_fd());
    Ok(stream)
}

/// Starts the SFTP protocol on a stream: asks the server's version and limits.
async fn start_session<S>(stream: S) -> io::Result<(RawSftpSession, Features)>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let config = russh_sftp::client::Config {
        // the largest packet accepted from the server: replies to reads, and a listing page
        // of a server that does not follow OpenSSH's
        max_packet_len: 2 * 1024 * 1024,
        // each request waits at most this long for its answer (reads in flight each on
        // their own, a listing each of its pages): set before the first request
        request_timeout_secs: super::REMOTE_TIMEOUT.as_secs(),
        ..Default::default()
    };
    let mut session = RawSftpSession::new_with_config(stream, config);
    let version = session.init().await.map_err(map)?;
    let ext = |name: &str, v: &str| version.extensions.get(name).is_some_and(|x| x == v);
    let mut features = Features { fsync: ext(extensions::FSYNC, "1"), statvfs: ext(extensions::STATVFS, "2"), ..Features::none() };
    if ext(extensions::LIMITS, "1") {
        match session.limits().await {
            Ok(l) => {
                let limits = Limits::from(l);
                session.set_limits(limits);
                features.limits = Some(limits);
                if let Some(n) = limits.read_len {
                    features.read_block = (n as usize).clamp(4096, BLOCK) as u32;
                }
                if let Some(n) = limits.write_len {
                    features.write_block = (n as usize).clamp(4096, BLOCK);
                }
            }
            // the extension is advertised and refuses this request: no limits known
            Err(SftpError::Status(_)) => {}
            Err(e) => return Err(map(e)),
        }
    }
    Ok((session, features))
}

/// The handshake, login and channel, each with its own time limit.
async fn ssh_session(conn: &Conn, host: &str, port: u16, config: Arc<russh::client::Config>, check: Check) -> io::Result<(russh::client::Handle<Check>, russh::ChannelStream<russh::client::Msg>)> {
    let stream = tcp(host, port).await?;
    let mut session = match tokio::time::timeout(TIMEOUT, russh::client::connect_stream(config, stream, check)).await {
        Err(_) => return Err(errno(libc::ETIMEDOUT)),
        Ok(Err(russh::Error::UnknownKey)) => return Err(unknown_key(&conn.host_key)),
        Ok(Err(e)) => return Err(other(e)),
        Ok(Ok(s)) => s,
    };
    let auth = if conn.auth == "key" || conn.auth == "own" {
        if conn.ssh_key.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                if conn.auth == "key" {
                    t!("there is no rynas key yet: create it and add it to authorized_keys on the server")
                } else {
                    t!("load the private key, or paste it")
                },
            ));
        }
        let key = russh::keys::decode_secret_key(&conn.ssh_key, None).map_err(other)?;
        let hash = within(TIMEOUT, async { session.best_supported_rsa_hash().await.map_err(other) }).await?.flatten();
        within(TIMEOUT, async { session.authenticate_publickey(&conn.user, PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await.map_err(other) }).await?
    } else {
        within(TIMEOUT, async { session.authenticate_password(&conn.user, &conn.password).await.map_err(other) }).await?
    };
    if !auth.success() {
        return Err(rynas_files::fsops::coded(io::ErrorKind::PermissionDenied, rynas_api_types::error_codes::REMOTE_AUTH_FAILED, t!("the server refuses the login: wrong user name, password or key")));
    }
    let channel = within(TIMEOUT, async { session.channel_open_session().await.map_err(other) }).await?;
    within(TIMEOUT, async { channel.request_subsystem(true, "sftp").await.map_err(other) }).await?;
    Ok((session, channel.into_stream()))
}

/// The runtime of a connection: two threads, so decrypting the stream and handing replies
/// over overlap, always running.
fn runtime() -> io::Result<Arc<Runtime>> {
    Ok(Arc::new(tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("sftp-net").enable_all().build()?))
}

impl Sftp {
    pub fn connect(conn: &Conn) -> io::Result<Self> {
        let rt = runtime()?;
        let seen = Arc::new(Mutex::new(String::new()));
        let names = Arc::new(Mutex::new(None));
        let learn = conn.host_key.is_empty() && super::may_pin();
        let check = Check { expected: conn.host_key.clone(), learn, seen: Arc::clone(&seen), names: Arc::clone(&names) };
        let port = if conn.port == 0 { 22 } else { conn.port };
        let host = conn.host.trim().to_string();
        let result = rt.block_on(async {
            let config = Arc::new(client_config());
            let (session, stream) = ssh_session(conn, &host, port, config, check).await?;
            let (sftp, features) = within(TIMEOUT, start_session(stream)).await?;
            let home = match sftp.realpath(".").await {
                Ok(n) => n.files.first().map(|f| f.filename.clone()).unwrap_or_else(|| "/".into()),
                Err(_) => "/".into(),
            };
            Ok::<_, io::Error>((session, sftp, features, home))
        });
        let (session, sftp, features, home) = result?;
        let host_key = seen.lock().unwrap().clone();
        if LOG_TERMS.load(Ordering::Relaxed)
            && let Some(names) = names.lock().unwrap().as_ref()
        {
            eprintln!("rynas-files-worker: {}", terms_line(&host, names, &features));
        }
        Ok(Self { rt, sftp: Arc::new(sftp), features, _session: Some(session), host_key, home })
    }

    fn path(parts: &[String]) -> String {
        format!("/{}", parts.join("/"))
    }

    fn entry(name: String, a: &FileAttributes) -> Entry {
        let dir = a.is_dir();
        Entry {
            name,
            kind: if dir { 'd' } else if a.is_symlink() { 'l' } else { 'f' },
            size: if dir { 0 } else { a.size.unwrap_or(0) },
            mtime: a.mtime.unwrap_or(0) as i64,
            mode: a.permissions.unwrap_or(0o644) & 0o7777,
        }
    }

    fn exists(&self, path: &str) -> bool {
        self.rt.block_on(self.sftp.lstat(path)).is_ok()
    }

    /// The items of a folder.
    fn read_dir(&self, path: String) -> io::Result<Vec<(String, FileAttributes)>> {
        self.rt.block_on(async {
            let handle = self.sftp.opendir(path).await.map_err(map)?.handle;
            let mut items = Vec::new();
            let result = loop {
                match self.sftp.readdir(handle.as_str()).await {
                    Ok(name) => items.extend(name.files.into_iter().map(|f| (f.filename, f.attrs))),
                    Err(SftpError::Status(s)) if s.status_code == StatusCode::Eof => break Ok(()),
                    Err(e) => break Err(map(e)),
                }
            };
            let _ = self.sftp.close(handle).await;
            result.map(|()| items)
        })
    }
}

/// The line a connection logs.
fn terms_line(host: &str, names: &russh::Names, features: &Features) -> String {
    let limits = match features.limits {
        Some(l) => format!("read {:?} write {:?} packet {:?} handles {:?}", l.read_len, l.write_len, l.packet_len, l.open_handles),
        None => "none".to_string(),
    };
    format!(
        "SFTP {host}: kex {:?}, cipher {:?}, max packet {MAX_PACKET}, window {} MiB, server limits: {limits}, fsync {}, reads {READS_IN_FLIGHT} x {} KiB, writes {WRITES_IN_FLIGHT} x {} KiB",
        names.kex,
        names.cipher,
        WINDOW_BYTES / (1024 * 1024),
        features.fsync,
        features.read_block / 1024,
        features.write_block / 1024
    )
}

impl Fs for Sftp {
    fn list(&self, dir: &[String]) -> io::Result<Vec<Entry>> {
        let path = Self::path(dir);
        let items = self.read_dir(path.clone())?;
        let mut out = Vec::new();
        for (name, meta) in items {
            if name == "." || name == ".." {
                continue;
            }
            let mut e = Self::entry(name.clone(), &meta);
            if e.kind == 'l' {
                // a link to a folder opens like one
                if let Ok(target) = self.rt.block_on(self.sftp.stat(format!("{}/{}", path.trim_end_matches('/'), name))) {
                    e = Self::entry(name, &target.attrs);
                }
            }
            out.push(e);
        }
        Ok(out)
    }

    fn stat(&self, path: &[String]) -> io::Result<Entry> {
        let meta = self.rt.block_on(self.sftp.stat(Self::path(path))).map_err(map)?.attrs;
        Ok(Self::entry(path.last().cloned().unwrap_or_else(|| "/".into()), &meta))
    }

    fn reader(&self, path: &[String]) -> io::Result<Box<dyn Reader>> {
        let p = Self::path(path);
        let meta = self.rt.block_on(self.sftp.stat(p.clone())).map_err(map)?.attrs;
        if meta.is_dir() {
            return Err(errno(libc::EISDIR));
        }
        let len = meta.size.unwrap_or(0);
        let handle = self.rt.block_on(self.sftp.open(p, OpenFlags::READ, FileAttributes::empty())).map_err(map)?.handle;
        let ident = Ident { size: len, mtime: meta.mtime.unwrap_or(0) as i64, change: 0 };
        Ok(Box::new(SftpReader { rt: Arc::clone(&self.rt), session: Arc::clone(&self.sftp), handle: Some(handle), ident, len, pos: 0, block: self.features.read_block, cur: Vec::new(), cur_from: 0, pump: None }))
    }

    fn writer(&self, path: &[String], at: Option<u64>) -> io::Result<Box<dyn Writer>> {
        let p = Self::path(path);
        let flags = if at.is_some() { OpenFlags::WRITE } else { OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE };
        let handle = self.rt.block_on(self.sftp.open(p, flags, FileAttributes::empty())).map_err(map)?.handle;
        Ok(Box::new(SftpWriter::new(self, handle, at.unwrap_or(0))))
    }

    fn mkdir(&self, path: &[String]) -> io::Result<()> {
        let p = Self::path(path);
        if self.exists(&p) {
            return Err(errno(libc::EEXIST));
        }
        self.rt.block_on(self.sftp.mkdir(p, FileAttributes::empty())).map(|_| ()).map_err(map)
    }

    fn rename(&self, from: &[String], to: &[String]) -> io::Result<()> {
        let to = Self::path(to);
        if self.exists(&to) {
            return Err(errno(libc::EEXIST));
        }
        self.rt.block_on(self.sftp.rename(Self::path(from), to)).map(|_| ()).map_err(map)
    }

    fn remove(&self, path: &[String], dir: bool) -> io::Result<()> {
        let p = Self::path(path);
        if dir {
            self.rt.block_on(self.sftp.rmdir(p)).map(|_| ()).map_err(|e| {
                let e = map(e);
                // OpenSSH answers a plain failure for a folder that is not empty
                if e.kind() == io::ErrorKind::Other { errno(libc::ENOTEMPTY) } else { e }
            })
        } else {
            self.rt.block_on(self.sftp.remove(p)).map(|_| ()).map_err(map)
        }
    }

    fn space(&self, path: &[String]) -> Option<(u64, u64)> {
        if !self.features.statvfs {
            return None;
        }
        let s = self.rt.block_on(self.sftp.statvfs(Self::path(path))).ok()?;
        Some((s.blocks_avail * s.fragment_size, s.blocks * s.fragment_size))
    }

    fn set_mtime(&self, path: &[String], mtime: i64) {
        let mut attrs = FileAttributes::empty();
        attrs.mtime = Some(mtime.max(0) as u32);
        attrs.atime = Some(mtime.max(0) as u32);
        let _ = self.rt.block_on(self.sftp.setstat(Self::path(path), attrs));
    }
}

/// Requests in flight, like OpenSSH's window: one at first, one more for each reply, up to
/// the largest allowed. Capped too at what the link carries in [`QUEUE_SECS`], measured
/// over the last second: requests are not left to wait behind so many others that their own
/// timeout (the watchdog) fires on a slow but working link.
struct Pace {
    max: usize,
    block: u64,
    window: usize,
    cap: usize,
    since: Instant,
    bytes: u64,
}

impl Pace {
    fn new(max: usize, block: u64) -> Self {
        Self { max, block, window: 1, cap: max, since: Instant::now(), bytes: 0 }
    }

    fn allowed(&self) -> usize {
        self.window.min(self.cap)
    }

    fn replied(&mut self, bytes: u64) {
        self.window = (self.window + 1).min(self.max);
        self.bytes += bytes;
        let elapsed = self.since.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let rate = self.bytes as f64 / elapsed.as_secs_f64();
            self.cap = ((rate * QUEUE_SECS / self.block as f64).ceil() as usize).clamp(4.min(self.max), self.max);
            self.since = Instant::now();
            self.bytes = 0;
        }
    }
}

/// What the read-ahead hands to the copy, in file order.
enum Msg {
    /// bytes from this offset
    Data(u64, Vec<u8>),
    /// the file ends at this offset (it is shorter than when it was opened)
    End(u64),
    Failed(io::Error),
}

/// One read request and what came back.
async fn read_one(session: Arc<RawSftpSession>, handle: String, at: u64, want: u32) -> (u64, u32, Result<Vec<u8>, SftpError>) {
    let result = session.read(handle, at, want).await.map(|d| d.data);
    (at, want, result)
}

/// The read-ahead of one file, on the runtime: a sliding window of requests from `start` up
/// to `len` (what the size said when the file was opened), replies delivered in order. As
/// OpenSSH does with partial replies: the rest of a short read is asked for again and the
/// following requests are made smaller; replies that arrive out of order wait in the
/// ordered queue.
async fn pump(session: Arc<RawSftpSession>, handle: String, start: u64, len: u64, mut block: u32, tx: mpsc::Sender<Msg>) {
    let mut pace = Pace::new(READS_IN_FLIGHT, block as u64);
    let mut pending = FuturesOrdered::new();
    let mut next = start;
    loop {
        while next < len && pending.len() < pace.allowed() {
            let want = (len - next).min(block as u64) as u32;
            pending.push_back(read_one(Arc::clone(&session), handle.clone(), next, want));
            next += want as u64;
        }
        let Some((at, want, result)) = pending.next().await else {
            let _ = tx.send(Msg::End(len)).await;
            return;
        };
        match result {
            Ok(data) if data.len() > want as usize => {
                let _ = tx.send(Msg::Failed(super::lost(io::ErrorKind::ConnectionAborted, "the server sent more data than asked for"))).await;
                return;
            }
            Ok(data) if data.is_empty() => {
                // an empty reply: treated like the end of the file
                let _ = tx.send(Msg::End(at)).await;
                return;
            }
            Ok(data) => {
                let got = data.len() as u32;
                pace.replied(got as u64);
                if tx.send(Msg::Data(at, data)).await.is_err() {
                    return;
                }
                if got < want {
                    // short read: ask for the rest first, the requests already out keep
                    // their place in the order
                    pending.push_front(read_one(Arc::clone(&session), handle.clone(), at + got as u64, want - got));
                    block = block.min(got.max(MIN_BLOCK));
                }
            }
            Err(SftpError::Status(s)) if s.status_code == StatusCode::Eof => {
                // everything before `at` was delivered; the rest of the window hit the end too
                let _ = tx.send(Msg::End(at)).await;
                return;
            }
            Err(e) => {
                let _ = tx.send(Msg::Failed(map(e))).await;
                return;
            }
        }
    }
}

/// A file being read.
struct SftpReader {
    rt: Arc<Runtime>,
    session: Arc<RawSftpSession>,
    handle: Option<String>,
    ident: Ident,
    len: u64,
    pos: u64,
    block: u32,
    /// a block taken and not all handed out: its bytes from `cur_from` on are at `pos`
    cur: Vec<u8>,
    cur_from: usize,
    pump: Option<mpsc::Receiver<Msg>>,
}

impl SftpReader {
    fn start_pump(&self) -> io::Result<mpsc::Receiver<Msg>> {
        let handle = self.handle.clone().ok_or_else(|| errno(libc::EBADF))?;
        let (tx, rx) = mpsc::channel(READ_QUEUE);
        self.rt.spawn(pump(Arc::clone(&self.session), handle, self.pos, self.len, self.block, tx));
        Ok(rx)
    }

    /// The next block from the position, in its own buffer; empty at the end.
    fn next(&mut self) -> io::Result<Vec<u8>> {
        if self.cur_from < self.cur.len() {
            let mut rest = std::mem::take(&mut self.cur);
            rest.drain(..self.cur_from);
            self.cur_from = 0;
            self.pos += rest.len() as u64;
            return Ok(rest);
        }
        if self.pos >= self.len {
            self.pump = None;
            return Ok(Vec::new());
        }
        if self.pump.is_none() {
            self.pump = Some(self.start_pump()?);
        }
        let pump = self.pump.as_mut().expect("just started");
        // a blocking wait: the read-ahead always ends it, at worst with a request's timeout
        match pump.blocking_recv() {
            Some(Msg::Data(at, data)) => {
                debug_assert_eq!(at, self.pos);
                self.pos += data.len() as u64;
                Ok(data)
            }
            Some(Msg::End(at)) => {
                let _ = at;
                // everything before it was handed out: the file ends at the position
                self.len = self.pos;
                self.pump = None;
                Ok(Vec::new())
            }
            Some(Msg::Failed(e)) => {
                self.pump = None;
                Err(e)
            }
            None => {
                self.pump = None;
                Err(super::lost(io::ErrorKind::ConnectionAborted, "the file's reading ended unexpectedly"))
            }
        }
    }
}

impl Read for SftpReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cur_from >= self.cur.len() {
            let block = self.next()?;
            // `next` moved the position past the block; it is handed out from here
            self.pos -= block.len() as u64;
            self.cur = block;
            self.cur_from = 0;
        }
        let n = buf.len().min(self.cur.len() - self.cur_from);
        buf[..n].copy_from_slice(&self.cur[self.cur_from..self.cur_from + n]);
        self.cur_from += n;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for SftpReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let to = match pos {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::End(n) => self.len as i64 + n,
            SeekFrom::Current(n) => self.pos as i64 + n,
        };
        if to < 0 {
            return Err(errno(libc::EINVAL));
        }
        if to as u64 != self.pos {
            // what was read ahead is for another position
            self.pump = None;
            self.cur.clear();
            self.cur_from = 0;
        }
        self.pos = to as u64;
        Ok(self.pos)
    }
}

impl Reader for SftpReader {
    fn len(&self) -> u64 {
        self.len
    }

    fn ident(&self) -> Ident {
        self.ident
    }

    fn next_block(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
        *buf = self.next()?;
        Ok(buf.len())
    }
}

/// Closes a file on the server without holding up the caller for long; the work and its
/// timer are the runtime's, so this is safe from any thread.
fn close_soon(rt: &Runtime, session: &Arc<RawSftpSession>, handle: String) {
    let session = Arc::clone(session);
    let (done, wait) = std::sync::mpsc::sync_channel::<()>(1);
    rt.spawn(async move {
        let _ = session.close(handle).await;
        let _ = done.send(());
    });
    let _ = wait.recv_timeout(CLOSE_LIMIT);
}

impl Drop for SftpReader {
    fn drop(&mut self) {
        self.pump = None;
        if let Some(handle) = self.handle.take() {
            close_soon(&self.rt, &self.session, handle);
        }
    }
}

/// The state of a file's writes in flight.
struct WriteState {
    in_flight: usize,
    unconfirmed: BTreeMap<u64, Arc<[u8]>>,
    error: Option<io::Error>,
    pace: Pace,
}

struct WriteShared {
    state: Mutex<WriteState>,
    wake: Condvar,
}

/// A file being written, several blocks in flight: `write` hands a block over and returns; a
/// failure shows on a later `write` or on `finish`, and the blocks not confirmed are given
/// back ([`Writer::unconfirmed`]) for a new connection to send.
struct SftpWriter {
    rt: Arc<Runtime>,
    session: Arc<RawSftpSession>,
    handle: Option<String>,
    pos: u64,
    block: usize,
    fsync: bool,
    shared: Arc<WriteShared>,
}

impl SftpWriter {
    fn new(sftp: &Sftp, handle: String, pos: u64) -> Self {
        let pace = Pace::new(WRITES_IN_FLIGHT, sftp.features.write_block as u64);
        let shared = Arc::new(WriteShared { state: Mutex::new(WriteState { in_flight: 0, unconfirmed: BTreeMap::new(), error: None, pace }), wake: Condvar::new() });
        Self { rt: Arc::clone(&sftp.rt), session: Arc::clone(&sftp.sftp), handle: Some(handle), pos, block: sftp.features.write_block, fsync: sftp.features.fsync, shared }
    }

    /// Hands one block over, waiting for a free place in the window.
    fn send(&mut self, at: u64, data: Arc<[u8]>) -> io::Result<()> {
        let handle = self.handle.clone().ok_or_else(|| errno(libc::EBADF))?;
        {
            let mut s = self.shared.state.lock().unwrap();
            while s.error.is_none() && s.in_flight >= s.pace.allowed() {
                s = self.shared.wake.wait(s).unwrap();
            }
            if let Some(e) = s.error.take() {
                return Err(e);
            }
            s.in_flight += 1;
            s.unconfirmed.insert(at, Arc::clone(&data));
        }
        let (session, shared, len) = (Arc::clone(&self.session), Arc::clone(&self.shared), data.len() as u64);
        self.rt.spawn(async move {
            let result = session.write(handle, at, data.to_vec()).await;
            let mut s = shared.state.lock().unwrap();
            s.in_flight -= 1;
            match result {
                Ok(_) => {
                    s.unconfirmed.remove(&at);
                    s.pace.replied(len);
                }
                Err(e) => {
                    s.error.get_or_insert(map(e));
                }
            }
            shared.wake.notify_all();
        });
        Ok(())
    }

    /// Waits until nothing is in flight; the first error, if any.
    fn drain(&mut self) -> io::Result<()> {
        let mut s = self.shared.state.lock().unwrap();
        // every wait ends: each request has its own timeout
        while s.in_flight > 0 {
            s = self.shared.wake.wait(s).unwrap();
        }
        match s.error.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Write for SftpWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = buf.len().min(self.block);
        if n == 0 {
            return Ok(0);
        }
        self.send(self.pos, buf[..n].into())?;
        self.pos += n as u64;
        Ok(n)
    }

    /// Waits until the server confirmed every block.
    fn flush(&mut self) -> io::Result<()> {
        self.drain()
    }
}

impl Writer for SftpWriter {
    fn finish(mut self: Box<Self>) -> io::Result<()> {
        Write::flush(&mut *self)?;
        let handle = self.handle.take().ok_or_else(|| errno(libc::EBADF))?;
        let (rt, session, fsync) = (Arc::clone(&self.rt), Arc::clone(&self.session), self.fsync);
        rt.block_on(async {
            if fsync {
                // the limit of this request only: nothing else is in flight on this
                // connection now, and a large file on a busy server can take minutes
                session.set_timeout(FSYNC_LIMIT.as_secs());
                let flushed = session.fsync(handle.as_str()).await;
                session.set_timeout(super::REMOTE_TIMEOUT.as_secs());
                flushed.map_err(|e| match e {
                    SftpError::Timeout => super::stalled(FSYNC_LIMIT),
                    e => map(e),
                })?;
            }
            session.close(handle).await.map(|_| ()).map_err(map)
        })
    }

    fn unconfirmed(&mut self) -> Vec<(u64, Arc<[u8]>)> {
        let _ = self.drain();
        let mut s = self.shared.state.lock().unwrap();
        s.error = None;
        std::mem::take(&mut s.unconfirmed).into_iter().collect()
    }

    fn resend(&mut self, blocks: Vec<(u64, Arc<[u8]>)>) -> io::Result<()> {
        let mut blocks = blocks.into_iter();
        while let Some((at, data)) = blocks.next() {
            if let Err(e) = self.send(at, Arc::clone(&data)) {
                // still not confirmed, this one and the rest: for the next writer
                let mut s = self.shared.state.lock().unwrap();
                s.unconfirmed.insert(at, data);
                s.unconfirmed.extend(blocks);
                return Err(e);
            }
        }
        Ok(())
    }
}

impl Drop for SftpWriter {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            close_soon(&self.rt, &self.session, handle);
        }
    }
}

/// A new Ed25519 key, in OpenSSH's format.
pub fn new_key() -> io::Result<String> {
    let mut seed = super::random_bytes::<32>()?;
    let key = PrivateKey::from(russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&seed));
    seed.fill(0);
    Ok(key.to_openssh(LineEnding::LF).map_err(other)?.to_string())
}

/// A private key of the user's, opened with its passphrase: the key in
/// OpenSSH's format without one, and its type, fingerprint and comment.
pub fn import_key(text: &str, passphrase: Option<&str>) -> Result<(String, String), String> {
    let key = russh::keys::decode_secret_key(text, passphrase).map_err(|e| match e {
        russh::keys::Error::KeyIsEncrypted => t!("the key is protected: type its passphrase"),
        _ if passphrase.is_some() => t!("invalid private key, or wrong passphrase"),
        _ => t!("not a valid OpenSSH or PEM private key"),
    })?;
    let public = key.public_key();
    let comment = key.comment();
    let info = format!("{} {}{}", public.algorithm().as_str(), public.fingerprint(HashAlg::Sha256), if comment.is_empty() { String::new() } else { format!(" {comment}") });
    let private = key.to_openssh(LineEnding::LF).map_err(|e| e.to_string())?.to_string();
    Ok((private, info))
}

/// The line for the server's authorized_keys.
pub fn public_key(private: &str, comment: &str) -> io::Result<String> {
    let key = russh::keys::decode_secret_key(private, None).map_err(other)?;
    let mut public = key.public_key().clone();
    public.set_comment(comment);
    public.to_openssh().map_err(other)
}

#[cfg(test)]
#[path = "sftp_tests.rs"]
mod tests;
