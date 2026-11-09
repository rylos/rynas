//! SMB 2/3 servers (Windows, Samba, other NAS), with the smb crate.
//!
//! Every wait of the crate is bounded by its progress watchdog ([`REMOTE_TIMEOUT`], see the
//! "rynas patch" notes in `vendor/smb`): a request fails once nothing at all came from the
//! server for that long, never because a slow listing or a long batch of reads took more.
//! No call runs on a thread of its own to be timed. A big file is read and written with
//! several requests in flight ([`Pipeline`], [`SmbWriter`]): one at a time a round trip
//! caps the speed far below the line.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use smb::{
    Client, ClientConfig, CreateOptions, FileAccessMask, FileAttributes, FileBasicInformation, FileCreateArgs, FileDirectoryInformation,
    FileDispositionInformation, FileFsFullSizeInformation, FileRenameInformation, IoctlBuffer, PipeTransceiveRequest, Resource,
    UncPath,
};

use smb::binrw_util::prelude::FileTime;
use smb::resource::GetLen;

use super::{Conn, Entry, Fs, Ident, REMOTE_TIMEOUT, Reader, Writer, errno, srvsvc};

/// Largest single read or write: servers offer 1–8 MiB (fewer when they say so, see
/// [`Smb::max_io`]). Each costs 16 credits with large MTU.
const BLOCK: usize = 1024 * 1024;
/// Reads in flight for one file: at 1 MiB each, enough to keep a gigabit line busy across
/// the server's turnaround (8 MiB in flight is about 70 ms of line time).
const READS_IN_FLIGHT: usize = 8;
/// Blocks a file's reader holds ahead of the copy, those in flight included (16 MiB): the
/// local writes overlap the network reads.
const READ_WINDOW: u64 = 16;
/// Writes in flight for one file.
const WRITES_IN_FLIGHT: usize = 8;
/// Credits the client asks the server to keep granted: the reads and writes in flight
/// (16 credits per MiB) never wait for one.
const CREDITS_BACKLOG: u16 = 512;
/// 1601-01-01 to 1970-01-01, in seconds
const EPOCH_DIFF: u64 = 11_644_473_600;

pub struct Smb {
    client: Client,
    server: String,
    user: String,
    password: String,
    /// shares already connected, and the IPC$ of the share list
    trees: Mutex<HashSet<String>>,
    /// the largest read and write the server takes, learned once
    io: OnceLock<(usize, usize)>,
}

/// Whether to log each connection's terms (dialect, signing…): jobs do, browsing does not.
static LOG_TERMS: AtomicBool = AtomicBool::new(false);

/// Logs (to standard error, rynasd's journal) the terms of each new connection.
pub fn log_connections() {
    LOG_TERMS.store(true, Ordering::Relaxed);
}

impl Smb {
    pub fn connect(conn: &Conn) -> io::Result<Self> {
        Self::connect_with(conn, REMOTE_TIMEOUT)
    }

    /// With `limit` for the progress watchdog instead of [`REMOTE_TIMEOUT`] (tests).
    fn connect_with(conn: &Conn, limit: Duration) -> io::Result<Self> {
        let mut config = ClientConfig { dfs: false, ..Default::default() };
        // the progress watchdog: nothing from the server for this long while requests wait
        config.connection.timeout = Some(limit);
        config.connection.credits_backlog = Some(CREDITS_BACKLOG);
        if conn.port != 0 {
            config.connection.port = Some(conn.port);
        }
        let user = match (conn.domain.trim(), conn.user.trim()) {
            (_, "") => "Guest".to_string(),
            ("", u) => u.to_string(),
            (d, u) if u.contains(['\\', '@']) => {
                let _ = d;
                u.to_string()
            }
            (d, u) => format!("{d}\\{u}"),
        };
        Ok(Self {
            client: Client::new(config),
            server: conn.host.trim().to_string(),
            user,
            password: conn.password.clone(),
            trees: Mutex::new(HashSet::new()),
            io: OnceLock::new(),
        })
    }

    fn tree(&self, share: &str) -> io::Result<UncPath> {
        let unc = UncPath::from_str(&format!(r"\\{}\{}", self.server, share)).map_err(|_| errno(libc::EINVAL))?;
        let mut trees = self.trees.lock().unwrap();
        if !trees.contains(share) {
            self.client.share_connect(&unc, &self.user, self.password.clone()).map_err(map)?;
            if trees.is_empty() {
                self.connected();
            }
            trees.insert(share.to_string());
        }
        Ok(unc)
    }

    /// After the first tree of the connection: socket options, the server's I/O sizes and
    /// (for jobs) one line about the connection's terms.
    fn connected(&self) {
        super::tune_sockets();
        let Ok(conn) = self.client.get_connection(&self.server) else { return };
        let Some(info) = conn.conn_info() else { return };
        let n = &info.negotiation;
        let _ = self.io.set(((n.max_read_size as usize).clamp(64 * 1024, BLOCK), (n.max_write_size as usize).clamp(64 * 1024, BLOCK)));
        if LOG_TERMS.load(Ordering::Relaxed) {
            eprintln!(
                "rynas-files-worker: SMB {}: dialect {:?}, signing {:?}, encryption cipher {:?} (used only where the server or the share asks), max read {} KiB, max write {} KiB, credits granted {}",
                self.server,
                n.dialect_rev,
                n.signing_algo,
                n.encryption_cipher,
                n.max_read_size / 1024,
                n.max_write_size / 1024,
                conn.credits_granted()
            );
        }
    }

    fn unc(&self, path: &[String]) -> io::Result<UncPath> {
        let share = path.first().ok_or_else(|| errno(libc::EISDIR))?;
        Ok(self.tree(share)?.with_path(&path[1..].join("\\")))
    }

    fn rename_to(&self, from: &[String], to: &[String], replace: bool) -> io::Result<()> {
        if from.len() < 2 || to.len() < 2 {
            return Err(errno(libc::EACCES));
        }
        if from[0] != to[0] {
            return Err(errno(libc::EXDEV));
        }
        let target = to[1..].join("\\");
        self.with(from, FileCreateArgs::make_open_existing(FileAccessMask::new().with_delete(true).with_file_read_attributes(true)), |r| {
            let info = FileRenameInformation { replace_if_exists: replace.into(), root_directory: 0, file_name: target.as_str().into() };
            match r {
                Resource::File(f) => f.set_info(info),
                Resource::Directory(d) => d.set_info(info),
                Resource::Pipe(_) => Ok(()),
            }
            .map_err(map)
        })
    }

    /// Opens `path` and runs `f` on what opened; the handle is closed after, whatever `f`
    /// returned (not when the connection broke: nothing would answer).
    fn with<T>(&self, path: &[String], args: FileCreateArgs, f: impl FnOnce(&Resource) -> io::Result<T>) -> io::Result<T> {
        let unc = self.unc(path)?;
        let r = self.client.create_file(&unc, &args).map_err(map)?;
        let out = f(&r);
        if !out.as_ref().is_err_and(super::retryable) {
            close(r);
        }
        out
    }

    fn shares(&self) -> io::Result<Vec<Entry>> {
        {
            let mut trees = self.trees.lock().unwrap();
            if !trees.contains("IPC$") {
                self.client.ipc_connect(&self.server, &self.user, self.password.clone()).map_err(map)?;
                if trees.is_empty() {
                    self.connected();
                }
                trees.insert("IPC$".into());
            }
        }
        let pipe = self.client.open_pipe(&self.server, "srvsvc").map_err(map)?;
        let call = |data: Vec<u8>, max: u32| pipe.handle().fsctl_with_options(PipeTransceiveRequest::from(IoctlBuffer::from(data)), max).map(|r| r.to_vec()).map_err(map);
        let resp = call(srvsvc::bind(), 4096).and_then(|_| call(srvsvc::share_enum(&self.server), 1 << 20));
        let _ = pipe.handle().close();
        let list = srvsvc::stub(&resp?).and_then(|s| srvsvc::parse(&s)).map_err(io::Error::other)?;
        // disk shares only: no printers, no IPC$, no C$ and ADMIN$
        Ok(list
            .into_iter()
            .filter(|(_, kind, _)| *kind == 0)
            .map(|(name, _, _)| Entry { name, kind: 'd', size: 0, mtime: 0, mode: 0o755 })
            .collect())
    }

    /// The largest read and write the server takes in one request.
    fn max_io(&self) -> (usize, usize) {
        self.io.get().copied().unwrap_or((64 * 1024, 64 * 1024))
    }
}

fn entry(name: String, attrs: &FileAttributes, size: u64, mtime: FileTime) -> Entry {
    let dir = attrs.directory();
    let mode = match (dir, attrs.readonly()) {
        (true, _) => 0o755,
        (false, true) => 0o444,
        (false, false) => 0o644,
    };
    Entry { name, kind: if dir { 'd' } else { 'f' }, size: if dir { 0 } else { size }, mtime: unix(mtime), mode }
}

fn unix(t: FileTime) -> i64 {
    (*t / 10_000_000).saturating_sub(EPOCH_DIFF) as i64
}

fn filetime(secs: i64) -> FileTime {
    FileTime::from((secs.max(0) as u64 + EPOCH_DIFF) * 10_000_000)
}

/// The crate's errors as the job and the messages know them: the server's answer about a
/// file (not found, access denied, locked, in use…) is an errno for that file only; the
/// connection's own failures (no answer in time, closed, out of sync, the session or the
/// handle gone) are [`super::lost`] / [`super::stalled`], worth a new connection.
fn map(e: smb::Error) -> io::Error {
    use smb::Error as E;
    let status = match &e {
        E::ReceivedErrorMessage(s, _) | E::UnexpectedMessageStatus(s) => *s,
        E::OperationTimeout(_, limit) => return super::stalled(*limit),
        // connecting, or the first answer before the connection's watchdog runs
        E::TransportError(smb::transport::TransportError::Timeout(limit)) => return super::stalled(*limit),
        E::TransportError(smb::transport::TransportError::IoError(io)) if matches!(io.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
            return super::stalled(REMOTE_TIMEOUT);
        }
        E::IoError(_)
        | E::TransportError(_)
        | E::ConnectionStopped
        | E::Cancelled(_)
        | E::ChannelRecvError(_)
        | E::MessageProcessingError(_)
        | E::AcquireError(_)
        | E::LockError
        | E::JoinError(_)
        | E::InvalidMessage(_)
        | E::SignatureVerificationFailed
        | E::UnexpectedMessageId(..)
        | E::BinRWError(_)
        | E::TranformFailed(_) => return super::lost(io::ErrorKind::ConnectionAborted, e),
        _ => return io::Error::other(e.to_string()),
    };
    let code = match status {
        0xC000_0034 | 0xC000_003A | 0xC000_00CC | 0xC000_0056 | 0xC000_000F => libc::ENOENT,
        0xC000_0022 | 0xC000_0061 => libc::EACCES,
        0xC000_006D | 0xC000_006E | 0xC000_0070 | 0xC000_0071 | 0xC000_0072 | 0xC000_0234 | 0xC000_0064 => {
            return rynas_files::fsops::coded(io::ErrorKind::PermissionDenied, rynas_api_types::error_codes::REMOTE_AUTH_FAILED, t!("the server refuses the login: wrong user name or password"));
        }
        0xC000_0035 => libc::EEXIST,
        0xC000_0101 => libc::ENOTEMPTY,
        // sharing violation, file lock conflict, lock not granted: the file is in use
        0xC000_0043 | 0xC000_0054 | 0xC000_0055 => libc::EBUSY,
        0xC000_00BA => libc::EISDIR,
        0xC000_0103 => libc::ENOTDIR,
        0xC000_007F | 0xC000_0044 => libc::ENOSPC,
        0xC000_00A2 => libc::EROFS,
        0xC000_0033 => libc::EINVAL,
        0xC000_00D4 => libc::EXDEV,
        0xC000_0185 => libc::EIO,
        0xC000_00BB | 0xC000_0010 => libc::EOPNOTSUPP,
        // the handle, the share or the session is gone (a server restart, a session that
        // expired): a new connection opens it again
        0xC000_0008 | 0xC000_0128 | 0xC000_00C9 | 0xC000_0203 | 0xC000_035C | 0xC000_020C | 0xC000_013C => {
            return super::lost(io::ErrorKind::ConnectionAborted, e);
        }
        _ => return io::Error::other(e.to_string()),
    };
    errno(code)
}

/// An I/O error of the crate's file methods: its own error inside (the "rynas patch" in
/// `resource/file.rs` keeps it), mapped by [`map`].
fn map_io(e: io::Error) -> io::Error {
    if e.get_ref().is_some_and(|r| r.is::<smb::Error>()) {
        let inner = e.into_inner().expect("checked above");
        return map(*inner.downcast::<smb::Error>().expect("checked above"));
    }
    e
}

fn close(r: Resource) {
    let _ = match &r {
        Resource::File(f) => f.close(),
        Resource::Directory(d) => d.close(),
        Resource::Pipe(p) => p.close(),
    };
}

fn read_only() -> FileCreateArgs {
    FileCreateArgs::make_open_existing(FileAccessMask::new().with_generic_read(true))
}

impl Fs for Smb {
    fn list(&self, dir: &[String]) -> io::Result<Vec<Entry>> {
        if dir.is_empty() {
            return self.shares();
        }
        // each page of the listing is a request of its own: a huge folder on a slow server
        // is fine as long as pages keep coming
        self.with(dir, read_only(), |r| {
            let Resource::Directory(d) = r else { return Err(errno(libc::ENOTDIR)) };
            let mut out = Vec::new();
            for item in d.query::<FileDirectoryInformation>("*").map_err(map)? {
                let item = item.map_err(map)?;
                let name = item.file_name.to_string();
                if name == "." || name == ".." {
                    continue;
                }
                out.push(entry(name, &item.file_attributes, item.end_of_file, item.last_write_time));
            }
            Ok(out)
        })
    }

    fn stat(&self, path: &[String]) -> io::Result<Entry> {
        if path.len() <= 1 {
            if let Some(share) = path.first() {
                self.tree(share)?;
            }
            return Ok(Entry { name: path.first().cloned().unwrap_or_else(|| self.server.clone()), kind: 'd', size: 0, mtime: 0, mode: 0o755 });
        }
        // the parent's listing, for this one name: one round trip
        let (name, parent) = path.split_last().unwrap();
        let found = self.with(parent, read_only(), |r| {
            let Resource::Directory(d) = r else { return Err(errno(libc::ENOTDIR)) };
            let found = d.query::<FileDirectoryInformation>(name).map_err(map).and_then(|mut it| match it.next() {
                Some(item) => item.map_err(map),
                None => Err(errno(libc::ENOENT)),
            });
            found.map(|item| entry(item.file_name.to_string(), &item.file_attributes, item.end_of_file, item.last_write_time))
        });
        match found {
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) || e.kind() == io::ErrorKind::Other => Err(errno(libc::ENOENT)),
            other => other,
        }
    }

    fn reader(&self, path: &[String]) -> io::Result<Box<dyn Reader>> {
        let unc = self.unc(path)?;
        let file = match self.client.create_file(&unc, &read_only()).map_err(map)? {
            Resource::File(file) => file,
            other => {
                close(other);
                return Err(errno(libc::EISDIR));
            }
        };
        let len = file.get_len().map_err(map)?;
        let (mtime, change) = file.times();
        let ident = Ident { size: len, mtime: mtime as i64, change: change as i64 };
        Ok(Box::new(SmbReader::new(Arc::new(file), len, ident, self.max_io().0)))
    }

    fn writer(&self, path: &[String], at: Option<u64>) -> io::Result<Box<dyn Writer>> {
        let unc = self.unc(path)?;
        let args = match at {
            None => FileCreateArgs::make_overwrite(FileAttributes::new(), CreateOptions::new().with_non_directory_file(true)),
            Some(_) => FileCreateArgs::make_open_existing(FileAccessMask::new().with_generic_read(true).with_generic_write(true)),
        };
        let file = match self.client.create_file(&unc, &args).map_err(map)? {
            Resource::File(file) => file,
            other => {
                close(other);
                return Err(errno(libc::EISDIR));
            }
        };
        Ok(Box::new(SmbWriter::new(Arc::new(file), at.unwrap_or(0), self.max_io().1)))
    }

    fn mkdir(&self, path: &[String]) -> io::Result<()> {
        if path.len() < 2 {
            return Err(errno(libc::EACCES));
        }
        self.with(path, FileCreateArgs::make_create_new(FileAttributes::new().with_directory(true), CreateOptions::new().with_directory_file(true)), |_| Ok(()))
    }

    fn rename(&self, from: &[String], to: &[String]) -> io::Result<()> {
        self.rename_to(from, to, false)
    }

    /// SMB renames over the old file in one step.
    fn replace(&self, from: &[String], to: &[String]) -> io::Result<bool> {
        self.rename_to(from, to, true).map(|()| true)
    }

    fn remove(&self, path: &[String], dir: bool) -> io::Result<()> {
        if path.len() < 2 {
            return Err(errno(libc::EACCES));
        }
        let options = if dir { CreateOptions::new().with_directory_file(true) } else { CreateOptions::new().with_non_directory_file(true) };
        let args = FileCreateArgs { options, ..FileCreateArgs::make_open_existing(FileAccessMask::new().with_delete(true).with_file_read_attributes(true)) };
        self.with(path, args, |r| {
            match r {
                Resource::File(f) => f.set_info(FileDispositionInformation::default()),
                Resource::Directory(d) => d.set_info(FileDispositionInformation::default()),
                Resource::Pipe(_) => Ok(()),
            }
            .map_err(map)
        })
    }

    fn space(&self, path: &[String]) -> Option<(u64, u64)> {
        let share = path.first()?;
        let i = self
            .with(std::slice::from_ref(share), FileCreateArgs::make_open_existing(FileAccessMask::new().with_file_read_attributes(true)), |r| match r {
                Resource::Directory(d) => d.query_fs_info::<FileFsFullSizeInformation>().map_err(map),
                _ => Err(errno(libc::ENOTDIR)),
            })
            .ok()?;
        let unit = i.sectors_per_allocation_unit as u64 * i.bytes_per_sector as u64;
        Some((i.caller_available_allocation_units * unit, i.total_allocation_units * unit))
    }

    fn set_mtime(&self, path: &[String], mtime: i64) {
        let _ = self.with(path, FileCreateArgs::make_open_existing(FileAccessMask::new().with_file_write_attributes(true)), |r| {
            // zero leaves a time as it is
            let info = FileBasicInformation {
                creation_time: FileTime::ZERO,
                last_access_time: FileTime::ZERO,
                last_write_time: filetime(mtime),
                change_time: FileTime::ZERO,
                file_attributes: FileAttributes::new(),
            };
            let _ = match r {
                Resource::File(f) => f.set_info(info),
                Resource::Directory(d) => d.set_info(info),
                Resource::Pipe(_) => Ok(()),
            };
            Ok(())
        });
    }

    fn is_top(&self, path: &[String]) -> bool {
        path.is_empty()
    }
}

/// Reads one block at `at`: the server answers up to `want` bytes; a short answer before
/// the end is asked again for the rest. Fewer bytes than `want` only at the end of the file.
fn read_at(file: &smb::File, at: u64, want: usize) -> io::Result<Vec<u8>> {
    let mut data = file.read_owned(want as u32, at).map_err(map)?;
    while !data.is_empty() && data.len() < want {
        let more = file.read_owned((want - data.len()) as u32, at + data.len() as u64).map_err(map)?;
        if more.is_empty() {
            break;
        }
        data.extend_from_slice(&more);
    }
    Ok(data)
}

/// Reads of a file ahead of the copy: [`READS_IN_FLIGHT`] threads each take the next block
/// not yet asked for, at most [`READ_WINDOW`] blocks past the one the copy takes next, and
/// leave it to be taken in order. The threads live as long as the file's reading.
struct Pipeline {
    shared: Arc<PipeShared>,
}

struct PipeShared {
    state: Mutex<PipeState>,
    wake: Condvar,
}

#[derive(Default)]
struct PipeState {
    /// next block to ask for
    next: u64,
    /// next block the copy takes
    taken: u64,
    /// blocks to read in all
    blocks: u64,
    ready: BTreeMap<u64, io::Result<Vec<u8>>>,
    stop: bool,
}

impl Pipeline {
    fn start(file: Arc<smb::File>, start: u64, len: u64, block: usize) -> Self {
        let blocks = (len - start).div_ceil(block as u64);
        let shared = Arc::new(PipeShared { state: Mutex::new(PipeState { blocks, ..Default::default() }), wake: Condvar::new() });
        let mut spawned = 0;
        for _ in 0..(blocks as usize).min(READS_IN_FLIGHT) {
            let (shared, file) = (Arc::clone(&shared), Arc::clone(&file));
            match std::thread::Builder::new().name("smb-read".into()).spawn(move || Self::work(&shared, &file, start, len, block)) {
                Ok(_) => spawned += 1,
                Err(_) => break,
            }
        }
        if spawned == 0 {
            // nobody to read: `take` fails instead of waiting
            shared.state.lock().unwrap().stop = true;
        }
        Self { shared }
    }

    fn work(shared: &PipeShared, file: &smb::File, start: u64, len: u64, block: usize) {
        loop {
            let i = {
                let mut s = shared.state.lock().unwrap();
                loop {
                    if s.stop || s.next >= s.blocks {
                        return;
                    }
                    if s.next < s.taken + READ_WINDOW {
                        break;
                    }
                    s = shared.wake.wait(s).unwrap();
                }
                s.next += 1;
                s.next - 1
            };
            let at = start + i * block as u64;
            let want = (block as u64).min(len.saturating_sub(at)) as usize;
            let data = read_at(file, at, want);
            let failed = data.is_err();
            let mut s = shared.state.lock().unwrap();
            s.ready.insert(i, data);
            if failed {
                // the blocks after it are not wanted any more
                s.stop = true;
            }
            shared.wake.notify_all();
        }
    }

    /// The next block in order (waits for it). Every wait ends: a read fails at worst when
    /// the connection's watchdog gives up on it.
    fn take(&self) -> Option<io::Result<Vec<u8>>> {
        let mut s = self.shared.state.lock().unwrap();
        loop {
            if s.taken >= s.blocks {
                return None;
            }
            let i = s.taken;
            if let Some(data) = s.ready.remove(&i) {
                s.taken += 1;
                self.shared.wake.notify_all();
                return Some(data);
            }
            if s.stop && s.next <= i {
                // stopped before this block was asked for (an earlier one failed)
                return Some(Err(super::lost(io::ErrorKind::ConnectionAborted, "a read failed")));
            }
            s = self.shared.wake.wait(s).unwrap();
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        // the threads end after their read in flight; nobody waits for them
        self.shared.state.lock().unwrap().stop = true;
        self.shared.wake.notify_all();
    }
}

/// A file being read, from the position on.
struct SmbReader {
    file: Arc<smb::File>,
    len: u64,
    ident: Ident,
    pos: u64,
    block: usize,
    /// a block taken and not all handed out: its bytes from `cur_from` on are at `pos`
    cur: Vec<u8>,
    cur_from: usize,
    pipe: Option<Pipeline>,
    /// a request failed for the connection: closing would only wait
    broken: bool,
}

impl SmbReader {
    fn new(file: Arc<smb::File>, len: u64, ident: Ident, block: usize) -> Self {
        Self { file, len, ident, pos: 0, block, cur: Vec::new(), cur_from: 0, pipe: None, broken: false }
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
            return Ok(Vec::new());
        }
        let left = self.len - self.pos;
        let data = if left <= self.block as u64 && self.pipe.is_none() {
            // one block: no threads for a small file
            read_at(&self.file, self.pos, left as usize)
        } else {
            let pipe = match self.pipe.take() {
                Some(p) => p,
                None => Pipeline::start(Arc::clone(&self.file), self.pos, self.len, self.block),
            };
            match pipe.take() {
                Some(data) => {
                    self.pipe = Some(pipe);
                    data
                }
                None => Ok(Vec::new()),
            }
        };
        let data = data.inspect_err(|e| {
            self.broken |= super::retryable(e);
            self.pipe = None;
        })?;
        let want = (self.block as u64).min(left) as usize;
        if data.len() < want {
            // the file is shorter than when it was opened: it ends here
            self.len = self.pos + data.len() as u64;
            self.pipe = None;
        }
        self.pos += data.len() as u64;
        Ok(data)
    }
}

impl Read for SmbReader {
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

impl Seek for SmbReader {
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
            self.pipe = None;
            self.cur.clear();
            self.cur_from = 0;
        }
        self.pos = to as u64;
        Ok(self.pos)
    }
}

impl Reader for SmbReader {
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

impl Drop for SmbReader {
    fn drop(&mut self) {
        self.pipe = None;
        if !self.broken {
            let _ = self.file.close();
        }
    }
}

/// A file being written, several blocks in flight: `write` hands a block to the writing
/// threads and returns; a failure shows on a later `write` or on `finish`, and the blocks
/// not confirmed are given back ([`Writer::unconfirmed`]) for a new connection to send.
struct SmbWriter {
    file: Arc<smb::File>,
    pos: u64,
    block: usize,
    shared: Arc<WriteShared>,
    /// the first block, held back: a file of one block is written without threads
    held: Option<(u64, Arc<[u8]>)>,
    threads: Vec<std::thread::JoinHandle<()>>,
    broken: bool,
}

struct WriteShared {
    state: Mutex<WriteState>,
    wake: Condvar,
}

#[derive(Default)]
struct WriteState {
    queue: VecDeque<(u64, Arc<[u8]>)>,
    /// handed over and not confirmed by the server, by offset (the queue's included)
    unconfirmed: BTreeMap<u64, Arc<[u8]>>,
    busy: usize,
    error: Option<io::Error>,
    stop: bool,
}

/// Writes all of `data` at `at` (a short write is followed by the rest).
fn write_at(file: &smb::File, at: u64, data: &Arc<[u8]>) -> io::Result<()> {
    let mut done = file.write_block_zc(Arc::clone(data), at, None).map_err(map_io)?;
    while done < data.len() {
        if done == 0 {
            return Err(errno(libc::EIO));
        }
        let rest: Arc<[u8]> = data[done..].into();
        let n = file.write_block_zc(rest, at + done as u64, None).map_err(map_io)?;
        if n == 0 {
            return Err(errno(libc::EIO));
        }
        done += n;
    }
    Ok(())
}

impl SmbWriter {
    fn new(file: Arc<smb::File>, pos: u64, block: usize) -> Self {
        let shared = Arc::new(WriteShared { state: Mutex::new(WriteState::default()), wake: Condvar::new() });
        Self { file, pos, block, shared, held: None, threads: Vec::new(), broken: false }
    }

    fn work(shared: &WriteShared, file: &smb::File) {
        loop {
            let (at, data) = {
                let mut s = shared.state.lock().unwrap();
                loop {
                    if s.error.is_some() || (s.stop && s.queue.is_empty()) {
                        return;
                    }
                    if let Some(job) = s.queue.pop_front() {
                        s.busy += 1;
                        break job;
                    }
                    s = shared.wake.wait(s).unwrap();
                }
            };
            let result = write_at(file, at, &data);
            let mut s = shared.state.lock().unwrap();
            s.busy -= 1;
            match result {
                Ok(()) => {
                    s.unconfirmed.remove(&at);
                }
                Err(e) => {
                    s.error.get_or_insert(e);
                }
            }
            shared.wake.notify_all();
        }
    }

    fn send(&mut self, at: u64, data: Arc<[u8]>) -> io::Result<()> {
        if self.threads.is_empty() {
            for _ in 0..WRITES_IN_FLIGHT {
                let (shared, file) = (Arc::clone(&self.shared), Arc::clone(&self.file));
                match std::thread::Builder::new().name("smb-write".into()).spawn(move || Self::work(&shared, &file)) {
                    Ok(t) => self.threads.push(t),
                    Err(_) if !self.threads.is_empty() => break,
                    Err(e) => return Err(e),
                }
            }
        }
        let mut s = self.shared.state.lock().unwrap();
        while s.error.is_none() && s.queue.len() + s.busy >= self.threads.len() {
            s = self.shared.wake.wait(s).unwrap();
        }
        if let Some(e) = s.error.take() {
            self.broken |= super::retryable(&e);
            return Err(e);
        }
        s.unconfirmed.insert(at, Arc::clone(&data));
        s.queue.push_back((at, data));
        self.shared.wake.notify_all();
        Ok(())
    }

    /// Waits until nothing is in flight; the first error, if any.
    fn drain(&mut self) -> io::Result<()> {
        let mut s = self.shared.state.lock().unwrap();
        while s.error.is_none() && (s.busy > 0 || !s.queue.is_empty()) {
            s = self.shared.wake.wait(s).unwrap();
        }
        if s.error.is_some() {
            // the blocks queued are not sent any more; wait for those in flight
            while s.busy > 0 {
                s = self.shared.wake.wait(s).unwrap();
            }
        }
        match s.error.take() {
            Some(e) => {
                self.broken |= super::retryable(&e);
                Err(e)
            }
            None => Ok(()),
        }
    }

    fn stop_threads(&mut self) {
        self.shared.state.lock().unwrap().stop = true;
        self.shared.wake.notify_all();
    }
}

impl Write for SmbWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = buf.len().min(self.block);
        if n == 0 {
            return Ok(0);
        }
        let data: Arc<[u8]> = buf[..n].into();
        let at = self.pos;
        match self.held.take() {
            None if self.threads.is_empty() => self.held = Some((at, data)),
            held => {
                if let Some((h_at, h_data)) = held {
                    // handed over by an earlier write: given back if it cannot go now
                    if let Err(e) = self.send(h_at, Arc::clone(&h_data)) {
                        self.shared.state.lock().unwrap().unconfirmed.insert(h_at, h_data);
                        return Err(e);
                    }
                }
                self.send(at, data)?;
            }
        }
        self.pos += n as u64;
        Ok(n)
    }

    /// Sends what is held back and waits until the server confirmed every block.
    fn flush(&mut self) -> io::Result<()> {
        if let Some((at, data)) = self.held.take() {
            if self.threads.is_empty() {
                if let Err(e) = write_at(&self.file, at, &data) {
                    self.broken |= super::retryable(&e);
                    self.shared.state.lock().unwrap().unconfirmed.insert(at, data);
                    return Err(e);
                }
            } else if let Err(e) = self.send(at, Arc::clone(&data)) {
                self.shared.state.lock().unwrap().unconfirmed.insert(at, data);
                return Err(e);
            }
        }
        self.drain()
    }
}

impl Writer for SmbWriter {
    fn finish(mut self: Box<Self>) -> io::Result<()> {
        Write::flush(&mut *self)?;
        self.stop_threads();
        for t in std::mem::take(&mut self.threads) {
            let _ = t.join();
        }
        let flushed = self.file.flush().map_err(map_io);
        let closed = self.file.close().map_err(map);
        // closed (or tried): Drop has nothing left to do
        self.broken = true;
        flushed?;
        closed
    }

    fn unconfirmed(&mut self) -> Vec<(u64, Arc<[u8]>)> {
        let _ = self.drain();
        let mut s = self.shared.state.lock().unwrap();
        s.queue.clear();
        let mut out: Vec<_> = std::mem::take(&mut s.unconfirmed).into_iter().collect();
        out.extend(self.held.take());
        out.sort_by_key(|(at, _)| *at);
        out
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

impl Drop for SmbWriter {
    fn drop(&mut self) {
        self.stop_threads();
        if !self.broken {
            let _ = self.file.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_times() {
        assert_eq!(unix(filetime(1_700_000_000)), 1_700_000_000);
        assert_eq!(unix(FileTime::ZERO), 0);
    }

    #[test]
    fn a_files_own_errors_are_not_the_connections() {
        let status = |s: u32| map(smb::Error::UnexpectedMessageStatus(s));
        // lock conflict, sharing violation, access denied, a status not known here
        for s in [0xC000_0054, 0xC000_0043, 0xC000_0022, 0xC000_0185, 0xC000_9999] {
            assert!(!super::super::retryable(&status(s)), "{s:#x}");
        }
        assert_eq!(status(0xC000_0054).raw_os_error(), Some(libc::EBUSY));
        assert_eq!(status(0xC000_0022).raw_os_error(), Some(libc::EACCES));
        // the session or the handle gone, the connection stopped, no answer in time
        assert!(super::super::retryable(&status(0xC000_0203)));
        assert!(super::super::retryable(&map(smb::Error::ConnectionStopped)));
        let timeout = map(smb::Error::OperationTimeout(smb::error::TimedOutTask::ReceiveNextMessage, REMOTE_TIMEOUT));
        assert!(super::super::retryable(&timeout));
        assert!(rynas_files::fsops::describe(&timeout).contains("stopped answering"));
        // through the crate's io::Error (the "rynas patch" keeps the error inside)
        let wrapped = io::Error::other(smb::Error::UnexpectedMessageStatus(0xC000_0054));
        assert_eq!(map_io(wrapped).raw_os_error(), Some(libc::EBUSY));
        assert!(super::super::retryable(&map_io(io::Error::other(smb::Error::ConnectionStopped))));
        // an io::Error of our own stays as it is
        assert_eq!(map_io(errno(libc::ENOSPC)).raw_os_error(), Some(libc::ENOSPC));
    }

    /// A server on the loopback that takes the connection and every byte sent, and never
    /// answers: what a server behind a broken VPN looks like.
    fn silent_server() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let _ = io::copy(&mut s, &mut io::sink());
                });
            }
        });
        port
    }

    #[test]
    fn a_server_that_stops_answering_fails_in_time() {
        let conn = Conn { host: "127.0.0.1".into(), port: silent_server(), user: "u".into(), password: "p".into(), ..Default::default() };
        let smb = Smb::connect_with(&conn, Duration::from_millis(800)).unwrap();
        let threads = || std::fs::read_dir("/proc/self/task").unwrap().count();
        let before = threads();
        let start = std::time::Instant::now();
        let e = smb.list(&["share".into()]).unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(5), "took {:?}", start.elapsed());
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(super::super::retryable(&e));
        assert_eq!(rynas_files::fsops::io_code(&e), rynas_api_types::error_codes::REMOTE_UNREACHABLE);
        assert!(rynas_files::fsops::describe(&e).contains("stopped answering"), "{}", rynas_files::fsops::describe(&e));
        // and the next call does not hang either
        let start = std::time::Instant::now();
        assert!(smb.stat(&["share".into(), "f".into()]).is_err());
        assert!(start.elapsed() < Duration::from_secs(5));
        drop(smb);
        // no call left behind on a thread of its own (the crate's two loops of the
        // connection end with it; other tests run threads too, so this is loose)
        std::thread::sleep(Duration::from_millis(500));
        assert!(threads() <= before + 4, "threads before {before}, after {}", threads());
    }

    /// A message that arrives with a pause in the middle longer than the receive loop's
    /// poll: before the "rynas patch" in smb-transport the bytes read before the pause were
    /// dropped and the stream lost its framing for good.
    #[test]
    fn a_pause_inside_a_message_loses_nothing() {
        use smb_transport::{SmbTransport, TcpTransport};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
        let sent = body.clone();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            for _ in 0..2 {
                let mut msg = vec![0u8];
                msg.extend_from_slice(&(sent.len() as u32).to_be_bytes()[1..]);
                msg.extend_from_slice(&sent);
                s.write_all(&msg[..2]).unwrap();
                std::thread::sleep(Duration::from_millis(300));
                s.write_all(&msg[2..1000]).unwrap();
                std::thread::sleep(Duration::from_millis(300));
                s.write_all(&msg[1000..]).unwrap();
                // and an idle gap between messages
                std::thread::sleep(Duration::from_millis(300));
            }
        });
        let mut t = Box::new(TcpTransport::new(Duration::from_secs(5)));
        t.connect("test", addr).unwrap();
        let (mut r, _w) = t.split().unwrap();
        r.set_read_timeout(Duration::from_millis(100)).unwrap();
        let mut got = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while got.len() < 2 && std::time::Instant::now() < deadline {
            match r.receive() {
                Ok(m) => got.push(m),
                // idle between messages: the loop polls again
                Err(smb_transport::TransportError::IoError(e)) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("{e}"),
            }
        }
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|m| *m == body));
    }

    /// The tests against a real Samba on the loopback (the benchmark's, see
    /// `packaging/bench-smb.sh`): `RYNAS_TEST_SMBD=127.0.0.1:4455/share/user/password`.
    /// Skipped without it.
    fn local_samba() -> Option<(Conn, String)> {
        let spec = std::env::var("RYNAS_TEST_SMBD").ok()?;
        let mut it = spec.splitn(4, '/');
        let (addr, share, user, password) = (it.next()?, it.next()?, it.next()?, it.next()?);
        let (host, port) = addr.rsplit_once(':')?;
        Some((Conn { host: host.into(), port: port.parse().ok()?, user: user.into(), password: password.into(), ..Default::default() }, share.into()))
    }

    /// A TCP relay in front of the server: `rate` bytes per second from the server (0: no
    /// limit), and `frozen` stops passing anything on.
    fn relay(to: (String, u16), rate: u64, frozen: Arc<AtomicBool>) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for client in listener.incoming().flatten() {
                let server = std::net::TcpStream::connect((to.0.as_str(), to.1)).unwrap();
                let pump = |mut from: std::net::TcpStream, mut to: std::net::TcpStream, rate: u64, frozen: Arc<AtomicBool>| {
                    std::thread::spawn(move || {
                        let mut buf = vec![0u8; 64 * 1024];
                        loop {
                            while frozen.load(Ordering::Relaxed) {
                                std::thread::sleep(Duration::from_millis(50));
                            }
                            let n = match from.read(&mut buf) {
                                Ok(0) | Err(_) => return,
                                Ok(n) => n,
                            };
                            if rate > 0 {
                                std::thread::sleep(Duration::from_secs_f64(n as f64 / rate as f64));
                            }
                            if to.write_all(&buf[..n]).is_err() {
                                return;
                            }
                        }
                    });
                };
                pump(client.try_clone().unwrap(), server.try_clone().unwrap(), 0, Arc::clone(&frozen));
                pump(server, client, rate, Arc::clone(&frozen));
            }
        });
        port
    }

    fn copy_up(smb: &Smb, path: &[String], data: &[u8]) {
        let mut w = smb.writer(path, None).unwrap();
        w.write_all(data).unwrap();
        w.finish().unwrap();
    }

    /// A relay in front of the server that flips one bit of the byte at offset `at` of what
    /// the server sends (`u64::MAX`: never, the control).
    fn relay_flip(to: (String, u16), at: u64) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for client in listener.incoming().flatten() {
                let server = std::net::TcpStream::connect((to.0.as_str(), to.1)).unwrap();
                let (client2, server2) = (client.try_clone().unwrap(), server.try_clone().unwrap());
                std::thread::spawn(move || {
                    let _ = std::io::copy(&mut &client2, &mut &server2);
                });
                std::thread::spawn(move || {
                    let (mut from, mut to, mut seen) = (server, client, 0u64);
                    let mut buf = vec![0u8; 64 * 1024];
                    loop {
                        let n = match from.read(&mut buf) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        if at >= seen && at < seen + n as u64 {
                            buf[(at - seen) as usize] ^= 0x10;
                        }
                        seen += n as u64;
                        if to.write_all(&buf[..n]).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        port
    }

    /// When the test server is set up for it, what it negotiated
    /// (`RYNAS_TEST_SMBD_SIGNING=AesGmac|AesCmac`, `RYNAS_TEST_SMBD_CIPHER=Aes128Gcm|…`).
    /// `RYNAS_TEST_SMBD_GMAC=ring|rustcrypto` picks the AES-GMAC implementation instead of
    /// letting the CPU decide, so both can face the real server on any machine.
    fn assert_negotiated(smb: &Smb, share: &str) {
        match std::env::var("RYNAS_TEST_SMBD_GMAC").as_deref() {
            Ok("ring") => smb::crypto::force_gmac_backend(Some(smb::crypto::GmacBackend::Ring)),
            Ok("rustcrypto") => smb::crypto::force_gmac_backend(Some(smb::crypto::GmacBackend::RustCrypto)),
            _ => {}
        }
        smb.tree(share).unwrap();
        let conn = smb.client.get_connection(&smb.server).unwrap();
        let info = conn.conn_info().unwrap();
        if let Ok(want) = std::env::var("RYNAS_TEST_SMBD_SIGNING") {
            assert_eq!(format!("{:?}", info.negotiation.signing_algo), format!("Some({want})"));
        }
        if let Ok(want) = std::env::var("RYNAS_TEST_SMBD_CIPHER") {
            assert_eq!(format!("{:?}", info.negotiation.encryption_cipher), format!("Some({want})"));
        }
    }

    /// With signing or encryption forced on the server: a server response with one bit
    /// flipped on the way is refused, never delivered as data.
    #[test]
    fn local_samba_tampered_answer_is_rejected() {
        let Some((conn, share)) = local_samba() else { return eprintln!("RYNAS_TEST_SMBD not set: skipped") };
        if std::env::var("RYNAS_TEST_SMBD_SIGNING").is_err() && std::env::var("RYNAS_TEST_SMBD_CIPHER").is_err() {
            return eprintln!("RYNAS_TEST_SMBD_SIGNING/_CIPHER not set: skipped");
        }
        let path = vec![share.clone(), format!("rynas-tamper-{}.bin", std::process::id())];
        let data: Vec<u8> = (0..8u32 << 20).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        let smb = Smb::connect(&conn).unwrap();
        assert_negotiated(&smb, &share);
        copy_up(&smb, &path, &data);
        // the control: the same relay, nothing flipped
        let port = relay_flip((conn.host.clone(), conn.port), u64::MAX);
        let clean = Smb::connect(&Conn { host: "127.0.0.1".into(), port, ..conn.clone() }).unwrap();
        let mut back = Vec::new();
        clean.reader(&path).unwrap().read_to_end(&mut back).unwrap();
        assert!(back == data, "the copy through the relay differs");
        // a bit of a READ response, 3 MiB into what the server sends
        let port = relay_flip((conn.host.clone(), conn.port), 3 << 20);
        let tampered = Smb::connect_with(&Conn { host: "127.0.0.1".into(), port, ..conn.clone() }, Duration::from_secs(5)).unwrap();
        let mut back = Vec::new();
        let res = tampered.reader(&path).and_then(|mut r| r.read_to_end(&mut back));
        let e = res.expect_err("a tampered answer was accepted");
        eprintln!("tampered answer refused: {e}");
        assert!(back.len() < data.len() && back[..] == data[..back.len()], "the tampered bytes reached the caller");
        drop(tampered);
        drop(clean);
        smb.remove(&path, false).unwrap();
    }

    #[test]
    fn local_samba_round_trip_and_seek() {
        let Some((conn, share)) = local_samba() else { return eprintln!("RYNAS_TEST_SMBD not set: skipped") };
        let smb = Smb::connect(&conn).unwrap();
        let path = vec![share.clone(), format!("rynas-test-{}.bin", std::process::id())];
        let data: Vec<u8> = (0..(21u32 << 20) + 12345).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        copy_up(&smb, &path, &data);
        // blocks handed over, then plain reads, then a seek back
        let mut r = smb.reader(&path).unwrap();
        assert_eq!(r.len(), data.len() as u64);
        let mut back = Vec::new();
        let mut buf = Vec::new();
        for _ in 0..5 {
            let n = r.next_block(&mut buf).unwrap();
            back.extend_from_slice(&buf[..n]);
        }
        let mut small = [0u8; 1000];
        let n = r.read(&mut small).unwrap();
        back.extend_from_slice(&small[..n]);
        r.read_to_end(&mut back).unwrap();
        assert!(back == data, "the copy differs");
        r.seek(SeekFrom::Start(7 << 20)).unwrap();
        let n = r.next_block(&mut buf).unwrap();
        assert_eq!(&buf[..n], &data[7 << 20..(7 << 20) + n]);
        drop(r);
        // a writer that goes on from an offset (a resumed upload) and gets blocks again
        let mut w = smb.writer(&path, Some(1 << 20)).unwrap();
        w.resend(vec![(0, Arc::from(&data[..1 << 20]))]).unwrap();
        w.write_all(&data[1 << 20..2 << 20]).unwrap();
        w.finish().unwrap();
        smb.remove(&path, false).unwrap();
    }

    #[test]
    fn local_samba_slow_but_answering_is_waited_for() {
        let Some((conn, share)) = local_samba() else { return eprintln!("RYNAS_TEST_SMBD not set: skipped") };
        let path = vec![share.clone(), format!("rynas-slow-{}.bin", std::process::id())];
        let data = vec![7u8; 8 << 20];
        copy_up(&Smb::connect(&conn).unwrap(), &path, &data);
        // 3 MiB/s with a watchdog of 1.5 s: 8 reads of 1 MiB in flight each wait far more
        // than that for their turn, and the connection keeps answering
        let frozen = Arc::new(AtomicBool::new(false));
        let port = relay((conn.host.clone(), conn.port), 3 << 20, Arc::clone(&frozen));
        let slow = Conn { host: "127.0.0.1".into(), port, ..conn.clone() };
        let smb = Smb::connect_with(&slow, Duration::from_millis(1500)).unwrap();
        let start = std::time::Instant::now();
        let mut r = smb.reader(&path).unwrap();
        let mut back = Vec::new();
        r.read_to_end(&mut back).unwrap();
        assert!(back == data);
        assert!(start.elapsed() > Duration::from_millis(2000), "the relay did not slow it down: {:?}", start.elapsed());
        // then the server stops answering: the read fails within the watchdog
        r.seek(SeekFrom::Start(0)).unwrap();
        frozen.store(true, Ordering::Relaxed);
        let start = std::time::Instant::now();
        let e = r.read_to_end(&mut Vec::new()).unwrap_err();
        assert!(super::super::retryable(&e), "{e}");
        assert!(start.elapsed() < Duration::from_secs(6), "{:?}", start.elapsed());
        drop(r);
        drop(smb);
        Smb::connect(&conn).unwrap().remove(&path, false).unwrap();
    }
}
