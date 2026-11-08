//! SMB 2/3 servers (Windows, Samba, other NAS), with the smb crate.

use std::collections::HashSet;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use smb::{
    Client, ClientConfig, CreateOptions, FileAccessMask, FileAttributes, FileBasicInformation, FileCreateArgs, FileDirectoryInformation,
    FileDispositionInformation, FileFsFullSizeInformation, FileRenameInformation, IoctlBuffer, PipeTransceiveRequest, Resource,
    UncPath,
};

use smb::binrw_util::prelude::FileTime;
use smb::resource::GetLen;

use super::{Conn, Entry, Fs, REMOTE_TIMEOUT, Reader, Writer, errno, srvsvc, timed};

/// Largest single read or write: servers offer 1–8 MiB (fewer when they say so, see
/// [`SmbFile::new`]). Each costs 16 credits with large MTU.
const BLOCK: usize = 1024 * 1024;
/// blocks read at once: 64 credits of the 128 the client asks the server to keep granted,
/// and few enough bytes in flight that a slow link still answers each read in time
const AHEAD: usize = 4;
/// 1601-01-01 to 1970-01-01, in seconds
const EPOCH_DIFF: u64 = 11_644_473_600;
/// How long the crate waits for a connection or for one answer. Below [`REMOTE_TIMEOUT`],
/// so its own error comes first when it can give one.
const CRATE_TIMEOUT: Duration = Duration::from_secs(30);
/// Closing a handle on a connection that is gone must not hold up the job.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Smb {
    client: Arc<Client>,
    server: String,
    user: String,
    password: String,
    /// shares already connected, and the IPC$ of the share list
    trees: Mutex<HashSet<String>>,
    /// longest wait for one request ([`REMOTE_TIMEOUT`])
    limit: Duration,
}

impl Smb {
    pub fn connect(conn: &Conn) -> io::Result<Self> {
        Self::connect_with(conn, REMOTE_TIMEOUT)
    }

    /// With `limit` for each request instead of [`REMOTE_TIMEOUT`] (tests).
    fn connect_with(conn: &Conn, limit: Duration) -> io::Result<Self> {
        let mut config = ClientConfig { dfs: false, ..Default::default() };
        config.connection.timeout = Some(CRATE_TIMEOUT);
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
        Ok(Self { client: Arc::new(Client::new(config)), server: conn.host.trim().to_string(), user, password: conn.password.clone(), trees: Mutex::new(HashSet::new()), limit })
    }

    /// A blocking call of the crate, bounded by the request limit (see [`timed`]).
    fn call<T: Send + 'static>(&self, f: impl FnOnce(&Client) -> io::Result<T> + Send + 'static) -> io::Result<T> {
        let client = Arc::clone(&self.client);
        timed(self.limit, move || f(&client))
    }

    fn tree(&self, share: &str) -> io::Result<UncPath> {
        let unc = UncPath::from_str(&format!(r"\\{}\{}", self.server, share)).map_err(|_| errno(libc::EINVAL))?;
        let mut trees = self.trees.lock().unwrap();
        if !trees.contains(share) {
            let (at, user, password) = (unc.clone(), self.user.clone(), self.password.clone());
            self.call(move |c| c.share_connect(&at, &user, password).map_err(map))?;
            super::tune_sockets();
            trees.insert(share.to_string());
        }
        Ok(unc)
    }

    fn unc(&self, path: &[String]) -> io::Result<UncPath> {
        let share = path.first().ok_or_else(|| errno(libc::EISDIR))?;
        Ok(self.tree(share)?.with_path(&path[1..].join("\\")))
    }

    /// Opens `path` (`args` made on the call's thread) and runs `f` on what opened, all in
    /// one bounded call; the handle is closed after, whatever `f` returned.
    fn with<T: Send + 'static>(
        &self,
        path: &[String],
        args: impl FnOnce() -> FileCreateArgs + Send + 'static,
        f: impl FnOnce(&Resource) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let unc = self.unc(path)?;
        self.call(move |c| {
            let r = c.create_file(&unc, &args()).map_err(map)?;
            let out = f(&r);
            close(r);
            out
        })
    }

    fn shares(&self) -> io::Result<Vec<Entry>> {
        {
            let mut trees = self.trees.lock().unwrap();
            if !trees.contains("IPC$") {
                let (server, user, password) = (self.server.clone(), self.user.clone(), self.password.clone());
                self.call(move |c| c.ipc_connect(&server, &user, password).map_err(map))?;
                super::tune_sockets();
                trees.insert("IPC$".into());
            }
        }
        let server = self.server.clone();
        let resp = self.call(move |c| {
            let pipe = c.open_pipe(&server, "srvsvc").map_err(map)?;
            let call = |data: Vec<u8>, max: u32| {
                pipe.handle().fsctl_with_options(PipeTransceiveRequest::from(IoctlBuffer::from(data)), max).map(|r| r.to_vec()).map_err(map)
            };
            let resp = call(srvsvc::bind(), 4096).and_then(|_| call(srvsvc::share_enum(&server), 1 << 20));
            let _ = pipe.handle().close();
            resp
        })?;
        let list = srvsvc::stub(&resp).and_then(|s| srvsvc::parse(&s)).map_err(io::Error::other)?;
        // disk shares only: no printers, no IPC$, no C$ and ADMIN$
        Ok(list
            .into_iter()
            .filter(|(_, kind, _)| *kind == 0)
            .map(|(name, _, _)| Entry { name, kind: 'd', size: 0, mtime: 0, mode: 0o755 })
            .collect())
    }

    /// The largest read and write the server takes in one request.
    fn max_io(&self) -> (usize, usize) {
        let client = Arc::clone(&self.client);
        let server = self.server.clone();
        timed(CLOSE_TIMEOUT, move || {
            let conn = client.get_connection(&server).map_err(map)?;
            Ok(conn.conn_info().map(|i| (i.negotiation.max_read_size as usize, i.negotiation.max_write_size as usize)))
        })
        .ok()
        .flatten()
        .map(|(r, w)| (r.clamp(64 * 1024, BLOCK), w.clamp(64 * 1024, BLOCK)))
        .unwrap_or((64 * 1024, 64 * 1024))
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

/// The server's status codes as the errno the messages know.
fn map(e: smb::Error) -> io::Error {
    let status = match &e {
        smb::Error::ReceivedErrorMessage(s, _) | smb::Error::UnexpectedMessageStatus(s) => *s,
        smb::Error::IoError(io) => return io::Error::new(io.kind(), io.to_string()),
        // the connection's own failures: worth a new connection (`super::retryable`)
        smb::Error::OperationTimeout(..) => return io::Error::new(io::ErrorKind::TimedOut, e.to_string()),
        smb::Error::ConnectionStopped | smb::Error::Cancelled(_) => return io::Error::new(io::ErrorKind::ConnectionAborted, e.to_string()),
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
        0xC000_0043 | 0xC000_0054 => libc::EBUSY,
        0xC000_00BA => libc::EISDIR,
        0xC000_0103 => libc::ENOTDIR,
        0xC000_007F | 0xC000_0044 => libc::ENOSPC,
        0xC000_00A2 => libc::EROFS,
        0xC000_0033 => libc::EINVAL,
        0xC000_00D4 => libc::EXDEV,
        _ => return io::Error::other(e.to_string()),
    };
    errno(code)
}

fn close(r: Resource) {
    let _ = match &r {
        Resource::File(f) => f.close(),
        Resource::Directory(d) => d.close(),
        Resource::Pipe(p) => p.close(),
    };
}

impl Fs for Smb {
    fn list(&self, dir: &[String]) -> io::Result<Vec<Entry>> {
        if dir.is_empty() {
            return self.shares();
        }
        self.with(dir, || FileCreateArgs::make_open_existing(FileAccessMask::new().with_generic_read(true)), |r| {
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
        let name = name.clone();
        let found = self.with(parent, || FileCreateArgs::make_open_existing(FileAccessMask::new().with_generic_read(true)), move |r| {
            let Resource::Directory(d) = r else { return Err(errno(libc::ENOTDIR)) };
            let found = d.query::<FileDirectoryInformation>(&name).map_err(map).and_then(|mut it| match it.next() {
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
        let (file, len) = self.call(move |c| {
            match c.create_file(&unc, &FileCreateArgs::make_open_existing(FileAccessMask::new().with_generic_read(true))).map_err(map)? {
                Resource::File(file) => match file.get_len().map_err(map) {
                    Ok(len) => Ok((file, len)),
                    Err(e) => {
                        let _ = file.close();
                        Err(e)
                    }
                },
                other => {
                    close(other);
                    Err(errno(libc::EISDIR))
                }
            }
        })?;
        Ok(Box::new(SmbFile::new(file, len, 0, self.max_io(), self.limit)))
    }

    fn writer(&self, path: &[String], at: Option<u64>) -> io::Result<Box<dyn Writer>> {
        let unc = self.unc(path)?;
        let file = self.call(move |c| {
            let args = match at {
                None => FileCreateArgs::make_overwrite(FileAttributes::new(), CreateOptions::new().with_non_directory_file(true)),
                Some(_) => FileCreateArgs::make_open_existing(FileAccessMask::new().with_generic_read(true).with_generic_write(true)),
            };
            match c.create_file(&unc, &args).map_err(map)? {
                Resource::File(file) => Ok(file),
                other => {
                    close(other);
                    Err(errno(libc::EISDIR))
                }
            }
        })?;
        Ok(Box::new(SmbFile::new(file, 0, at.unwrap_or(0), self.max_io(), self.limit)))
    }

    fn mkdir(&self, path: &[String]) -> io::Result<()> {
        if path.len() < 2 {
            return Err(errno(libc::EACCES));
        }
        self.with(path, || FileCreateArgs::make_create_new(FileAttributes::new().with_directory(true), CreateOptions::new().with_directory_file(true)), |_| Ok(()))
    }

    fn rename(&self, from: &[String], to: &[String]) -> io::Result<()> {
        if from.len() < 2 || to.len() < 2 {
            return Err(errno(libc::EACCES));
        }
        if from[0] != to[0] {
            return Err(errno(libc::EXDEV));
        }
        let target = to[1..].join("\\");
        self.with(from, || FileCreateArgs::make_open_existing(FileAccessMask::new().with_delete(true).with_file_read_attributes(true)), move |r| {
            let info = FileRenameInformation { replace_if_exists: false.into(), root_directory: 0, file_name: target.as_str().into() };
            match r {
                Resource::File(f) => f.set_info(info),
                Resource::Directory(d) => d.set_info(info),
                Resource::Pipe(_) => Ok(()),
            }
            .map_err(map)
        })
    }

    fn remove(&self, path: &[String], dir: bool) -> io::Result<()> {
        if path.len() < 2 {
            return Err(errno(libc::EACCES));
        }
        let args = move || {
            let options = if dir { CreateOptions::new().with_directory_file(true) } else { CreateOptions::new().with_non_directory_file(true) };
            FileCreateArgs { options, ..FileCreateArgs::make_open_existing(FileAccessMask::new().with_delete(true).with_file_read_attributes(true)) }
        };
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
            .with(std::slice::from_ref(share), || FileCreateArgs::make_open_existing(FileAccessMask::new().with_file_read_attributes(true)), |r| match r {
                Resource::Directory(d) => d.query_fs_info::<FileFsFullSizeInformation>().map_err(map),
                _ => Err(errno(libc::ENOTDIR)),
            })
            .ok()?;
        let unit = i.sectors_per_allocation_unit as u64 * i.bytes_per_sector as u64;
        Some((i.caller_available_allocation_units * unit, i.total_allocation_units * unit))
    }

    fn set_mtime(&self, path: &[String], mtime: i64) {
        let _ = self.with(path, || FileCreateArgs::make_open_existing(FileAccessMask::new().with_file_write_attributes(true)), move |r| {
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

/// An open file, read or written in blocks the server takes. The position
/// is kept here: the crate's own Seek does not move it. Every request is bounded
/// ([`timed`]); a failed one comes back as a connection error, for the job to reconnect
/// and go on from the position (`super::retryable`).
struct SmbFile {
    file: Option<Arc<smb::File>>,
    len: u64,
    pos: u64,
    /// read ahead: the bytes from `ahead_at` on, already fetched
    ahead: Vec<u8>,
    ahead_at: u64,
    /// largest read and write the server takes
    read_block: usize,
    write_block: usize,
    /// longest wait for one batch of requests ([`REMOTE_TIMEOUT`])
    limit: Duration,
}

/// A read or write that failed on an open file: the server's errors about a file come at
/// open time, so this is the connection's (the crate reports them all as `Other`).
fn broken(e: io::Error) -> io::Error {
    if e.kind() == io::ErrorKind::Other && e.get_ref().is_some_and(|r| !r.is::<rynas_files::fsops::Coded>()) {
        io::Error::new(io::ErrorKind::ConnectionAborted, e.to_string())
    } else {
        e
    }
}

impl SmbFile {
    fn new(file: smb::File, len: u64, pos: u64, (read_block, write_block): (usize, usize), limit: Duration) -> Self {
        Self { file: Some(Arc::new(file)), len, pos, ahead: Vec::new(), ahead_at: 0, read_block, write_block, limit }
    }

    fn file(&self) -> io::Result<Arc<smb::File>> {
        self.file.clone().ok_or_else(|| errno(libc::EBADF))
    }

    /// The next blocks from the position, several requests in flight at
    /// once: one at a time a request's round trip caps the speed.
    fn fetch(&mut self) -> io::Result<()> {
        let file = self.file()?;
        let block = self.read_block;
        let left = self.len - self.pos;
        let blocks = (left.div_ceil(block as u64) as usize).clamp(1, AHEAD);
        let (start, len) = (self.pos, self.len);
        let parts: Vec<io::Result<Vec<u8>>> = timed(self.limit, move || {
            Ok(std::thread::scope(|s| {
                let handles: Vec<_> = (0..blocks)
                    .map(|i| {
                        let at = start + (i * block) as u64;
                        let want = (block as u64).min(len.saturating_sub(at)) as usize;
                        let file = &file;
                        s.spawn(move || {
                            let mut b = vec![0u8; want];
                            let mut got = 0;
                            while got < want {
                                let n = file.read_block(&mut b[got..], at + got as u64, None, false)?;
                                if n == 0 {
                                    break;
                                }
                                got += n;
                            }
                            b.truncate(got);
                            Ok(b)
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(io::Error::other("read thread")))).collect()
            }))
        })?;
        self.ahead.clear();
        self.ahead_at = start;
        for part in parts {
            let part = part.map_err(broken)?;
            let short = part.len() < block;
            self.ahead.extend_from_slice(&part);
            if short {
                break;
            }
        }
        if self.ahead.is_empty() {
            // the file shrank under us
            self.len = self.pos;
        }
        Ok(())
    }
}

impl Read for SmbFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len {
            return Ok(0);
        }
        let have = self.pos >= self.ahead_at && self.pos < self.ahead_at + self.ahead.len() as u64;
        if !have {
            self.fetch()?;
        }
        let from = (self.pos - self.ahead_at) as usize;
        let n = buf.len().min(self.ahead.len().saturating_sub(from));
        buf[..n].copy_from_slice(&self.ahead[from..from + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for SmbFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let to = match pos {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::End(n) => self.len as i64 + n,
            SeekFrom::Current(n) => self.pos as i64 + n,
        };
        if to < 0 {
            return Err(errno(libc::EINVAL));
        }
        self.pos = to as u64;
        Ok(self.pos)
    }
}

impl Write for SmbFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = buf.len().min(self.write_block);
        let file = self.file()?;
        let data = buf[..n].to_vec();
        let at = self.pos;
        let done = timed(self.limit, move || file.write_block(&data, at, None)).map_err(broken)?;
        self.pos += done as u64;
        Ok(done)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Reader for SmbFile {
    fn len(&self) -> u64 {
        self.len
    }
}

impl Writer for SmbFile {
    fn finish(mut self: Box<Self>) -> io::Result<()> {
        let file = self.file.take().ok_or_else(|| errno(libc::EBADF))?;
        timed(self.limit, move || {
            let flushed = file.flush();
            let closed = file.close().map_err(map);
            flushed?;
            closed
        })
        .map_err(broken)
    }
}

impl Drop for SmbFile {
    fn drop(&mut self) {
        // a connection that stopped answering must not hold up the job here either
        if let Some(f) = self.file.take() {
            let _ = timed(CLOSE_TIMEOUT, move || f.close().map_err(map));
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
    }
}
