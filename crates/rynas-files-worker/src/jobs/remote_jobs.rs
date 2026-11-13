//! Copies, moves and deletes where a side is a remote connection. The same
//! engine as between shared folders, with each side either the NAS or a
//! server: no reflinks there, a move between two places is a copy and a
//! delete, and deleting on a server is for good (a server has no recycle bin rynas manages).
//!
//! Paths stay `PathBuf`s; a remote one is `/:<id>/…` and goes to its
//! connection.

use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

use crate::jobs::writeback::WriteBehind;
use std::path::{Path, PathBuf};

use super::{CHUNK, Cancelled, ConflictInfo, Decision, Engine, Op, Policy, Side, Step, part_name};
use crate::fsops::describe;
use crate::remote::{self, Connect, Fs};
use rynas_api_types::error_codes as c;

/// What a copy needs to know of an item, on either side.
#[derive(Clone, Copy)]
struct Meta {
    dir: bool,
    file: bool,
    size: u64,
    mtime: i64,
}

impl Meta {
    fn side(&self) -> Side {
        Side { size: self.size, mtime: self.mtime, dir: self.dir }
    }
}

/// One side of a job.
enum End {
    Local,
    Remote(RemoteEnd),
}

/// Makes a new connection to a job's server: the engine's, and the cancel watchdog's for
/// the clean-up when the engine is stuck (`super::watch_cancel`).
pub(super) type Connector = std::sync::Arc<dyn Fn() -> io::Result<Box<dyn Fs>> + Send + Sync>;

/// A server side: its connection, made again when it breaks (see [`End::call`]).
struct RemoteEnd {
    connect: Connector,
    fs: RefCell<Box<dyn Fs>>,
}

impl RemoteEnd {
    fn new(connect: Connector) -> io::Result<Self> {
        let fs = RefCell::new(connect()?);
        Ok(Self { connect, fs })
    }
}

/// What a copy reads from: a local file or a remote one, both able to go on from an offset.
trait Source: Read + Seek {
    /// the file as it was opened (remote files): compared before going on after a reconnect
    fn ident(&self) -> Option<remote::Ident> {
        None
    }
    /// The next bytes into `buf[..n]` (a remote reader swaps in its own buffer: no copy).
    fn next_block(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
        if buf.len() < CHUNK {
            buf.resize(CHUNK, 0);
        }
        self.read(buf)
    }
}

impl Source for File {
    fn ident(&self) -> Option<remote::Ident> {
        let m = self.metadata().ok()?;
        Some(remote::Ident { size: m.len(), mtime: m.mtime(), change: m.ctime() })
    }
}

impl Source for Box<dyn remote::Reader> {
    fn ident(&self) -> Option<remote::Ident> {
        Some((**self).ident())
    }
    fn next_block(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
        (**self).next_block(buf)
    }
}

/// The error of a file that changed on the server while it was copied, found when the copy
/// went on over a new connection: that file fails (seeking into another file would mix two
/// versions), copying it again starts from its first byte.
fn changed() -> io::Error {
    rynas_files::fsops::coded(io::ErrorKind::Other, c::CONFLICT, t!("the file changed on the server during the copy (size or time differ): copy it again"))
}

/// The error of a file whose reading ended before (or went past) the size it had when it was
/// opened: a server that answered "end of file" early, a file that shrank or grew. That file
/// fails: the temporary copy goes, the existing target stays, a move keeps its source.
fn size_changed(got: u64, want: u64) -> io::Error {
    rynas_files::fsops::coded(io::ErrorKind::Other, c::CONFLICT, t!("the file changed during the copy ({} of {} bytes read): copy it again", got, want))
}

/// How often a job looks at pause and cancel while the server finishes a file.
const FINISH_POLL: std::time::Duration = std::time::Duration::from_millis(200);
/// A server taking longer than this to finish a file shows in the progress (`method`).
const FINISH_SHOWN_AFTER: std::time::Duration = std::time::Duration::from_secs(1);

/// Tries of a file's copy on a new connection, after the one before broke or stopped
/// answering; a try that moves data starts the count again.
const RECONNECTS: u32 = 2;

fn parts(path: &Path) -> Vec<String> {
    remote::split_path(path).map(|(_, p)| p).unwrap_or_default()
}

fn local_meta(m: &fs::Metadata) -> Meta {
    Meta { dir: m.is_dir(), file: m.is_file(), size: if m.is_file() { m.len() } else { 0 }, mtime: m.mtime() }
}

fn remote_meta(e: &remote::Entry) -> Meta {
    Meta { dir: e.kind == 'd', file: e.kind == 'f', size: e.size, mtime: e.mtime }
}

impl End {
    fn open(path: &Path, conns: &[remote::Conn]) -> io::Result<End> {
        let Some((id, _)) = remote::split_path(path) else { return Ok(End::Local) };
        let conn = conns.iter().find(|c| c.id == id).ok_or_else(|| io::Error::other(t!("no such connection: was it deleted?")))?;
        let conn = conn.clone();
        Ok(End::Remote(RemoteEnd::new(std::sync::Arc::new(move || conn.connect()))?))
    }

    /// A new connection in place of the one that broke. The old one goes on a thread of
    /// its own: closing it may wait (bounded by its watchdog), the job does not.
    fn reconnect(&self) -> io::Result<()> {
        let End::Remote(r) = self else { return Err(io::Error::other("local")) };
        let fresh = (r.connect)()?;
        let old = r.fs.replace(fresh);
        let _ = std::thread::Builder::new().name("remote-drop".into()).spawn(move || drop(old));
        Ok(())
    }

    fn connector(&self) -> Option<Connector> {
        match self {
            End::Remote(r) => Some(std::sync::Arc::clone(&r.connect)),
            End::Local => None,
        }
    }

    /// Runs `f` on the server, again on a new connection when the connection broke or
    /// stopped answering ([`RECONNECTS`] times). `f` is told whether it runs again: a
    /// request that may have been carried out before the break (a folder made, a file
    /// removed) takes its "already done" answer as done.
    fn call<T>(&self, f: impl Fn(&dyn Fs, bool) -> io::Result<T>) -> io::Result<T> {
        let End::Remote(r) = self else { return Err(io::Error::other("local")) };
        let mut tries = 0;
        loop {
            let result = f(&**r.fs.borrow(), tries > 0);
            match result {
                Err(e) if remote::retryable(&e) && tries < RECONNECTS => {
                    tries += 1;
                    eprintln!("rynas-files-worker: {}: connecting again ({tries} of {RECONNECTS})", describe(&e));
                    if self.reconnect().is_err() {
                        return Err(e);
                    }
                }
                other => return other,
            }
        }
    }

    fn is_remote(&self) -> bool {
        matches!(self, End::Remote(_))
    }

    fn stat(&self, p: &Path) -> io::Result<Meta> {
        match self {
            End::Local => fs::symlink_metadata(p).map(|m| local_meta(&m)),
            End::Remote(_) => self.call(|fs, _| fs.stat(&parts(p))).map(|e| remote_meta(&e)),
        }
    }

    /// The items of a folder, sorted by name.
    fn list(&self, p: &Path) -> io::Result<Vec<(String, Meta)>> {
        let mut items: Vec<(String, Meta)> = match self {
            End::Local => rynas_files::confine::read_dir(p)?
                .flatten()
                .filter_map(|e| Some((e.file_name().into_string().ok()?, local_meta(&e.metadata().ok()?))))
                .collect(),
            End::Remote(_) => self.call(|fs, _| fs.list(&parts(p)))?.iter().map(|e| (e.name.clone(), remote_meta(e))).collect(),
        };
        items.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(items)
    }

    fn reader(&self, p: &Path) -> io::Result<Box<dyn Source>> {
        Ok(match self {
            End::Local => Box::new(rynas_files::confine::open(p)?),
            End::Remote(_) => Box::new(self.call(|fs, _| fs.reader(&parts(p)))?),
        })
    }

    /// `p` from byte `at` on (a remote copy going on after a reconnect); a new file with
    /// `None`.
    fn writer_at(&self, p: &Path, at: Option<u64>) -> io::Result<Box<dyn remote::Writer>> {
        match (self, at) {
            (End::Remote(_), Some(_)) => self.call(|fs, _| fs.writer(&parts(p), at)),
            _ => self.writer(p),
        }
    }

    fn writer(&self, p: &Path) -> io::Result<Box<dyn remote::Writer>> {
        Ok(match self {
            End::Local => {
                local_guard(p, false)?;
                let file = OpenOptions::new().write(true).create_new(true).mode(0o666).open(p)?;
                rynas_files::confine::check_made(&file, p)?;
                Box::new(LocalWriter { file, behind: WriteBehind::new() })
            }
            End::Remote(_) => self.call(|fs, _| fs.writer(&parts(p), None))?,
        })
    }

    fn mkdir(&self, p: &Path) -> io::Result<()> {
        match self {
            End::Local => local_guard(p, false).and_then(|()| fs::create_dir(p)).and_then(|()| rynas_files::confine::made_dir(p)),
            End::Remote(_) => self.call(|fs, again| match fs.mkdir(&parts(p)) {
                Err(e) if again && e.raw_os_error() == Some(libc::EEXIST) => Ok(()),
                other => other,
            }),
        }
    }

    fn remove(&self, p: &Path, dir: bool) -> io::Result<()> {
        match self {
            End::Local if dir => local_guard(p, true).and_then(|()| fs::remove_dir(p)),
            End::Local => local_guard(p, true).and_then(|()| fs::remove_file(p)),
            End::Remote(_) => self.call(|fs, again| match fs.remove(&parts(p), dir) {
                Err(e) if again && e.raw_os_error() == Some(libc::ENOENT) => Ok(()),
                other => other,
            }),
        }
    }

    /// Never replaces.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        match self {
            End::Local => local_guard(from, true).and_then(|()| local_guard(to, false)).and_then(|()| crate::fsops::rename_noreplace(from, to)),
            End::Remote(_) => self.call(|fs, again| match fs.rename(&parts(from), &parts(to)) {
                // done before the connection broke: the source is gone, the target there
                Err(e) if again && e.raw_os_error() == Some(libc::ENOENT) && fs.stat(&parts(to)).is_ok() => Ok(()),
                // the same, where the client looks at the target first (SFTP)
                Err(e) if again && e.raw_os_error() == Some(libc::EEXIST) && fs.stat(&parts(from)).is_err_and(|e| e.raw_os_error() == Some(libc::ENOENT)) => Ok(()),
                other => other,
            }),
        }
    }

    /// Puts `from` in the place of `to` in one step where that exists (a local rename, a
    /// server's). `Ok(false)`: this server cannot, nothing changed.
    fn replace(&self, from: &Path, to: &Path) -> io::Result<bool> {
        match self {
            End::Local => local_guard(to, false).and_then(|()| fs::rename(from, to)).map(|()| true),
            End::Remote(_) => self.call(|fs, again| match fs.replace(&parts(from), &parts(to)) {
                Err(e) if again && e.raw_os_error() == Some(libc::ENOENT) && fs.stat(&parts(to)).is_ok() => Ok(true),
                other => other,
            }),
        }
    }

    fn set_mtime(&self, p: &Path, mtime: i64) {
        match self {
            End::Local => {
                let Ok(path) = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()) else { return };
                let times = [libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT }, libc::timespec { tv_sec: mtime as _, tv_nsec: 0 }];
                // SAFETY: a C string and two timespecs
                unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
            }
            End::Remote(r) => r.fs.borrow().set_mtime(&parts(p), mtime),
        }
    }

    fn space(&self, p: &Path) -> Option<(u64, u64)> {
        match self {
            End::Local => crate::fsops::space(p),
            End::Remote(r) => r.fs.borrow().space(&parts(p)),
        }
    }

    /// A free name in `dir`: "name (1).ext", "name (2).ext"…
    fn unique(&self, dir: &Path, name: &str, tag: &str) -> PathBuf {
        let (stem, ext) = match name.rfind('.') {
            Some(i) if i > 0 => (&name[..i], &name[i..]),
            _ => (name, ""),
        };
        let candidate = dir.join(format!("{stem}{tag}{ext}"));
        if self.stat(&candidate).is_err() {
            return candidate;
        }
        (1..).map(|n| dir.join(format!("{stem}{tag} ({n}){ext}"))).find(|p| self.stat(p).is_err()).unwrap()
    }
}

/// The final guard of a local write in a job with a server at the other end: `p` is made in
/// its folder (`removing`: `p` itself goes), never in or above a PC's backup folder
/// (`confine::refuse_pc_write`).
fn local_guard(p: &Path, removing: bool) -> io::Result<()> {
    if removing {
        rynas_files::confine::refuse_pc_write(p, true)
    } else {
        rynas_files::confine::refuse_pc_write(p.parent().unwrap_or(p), false)
    }
}

struct LocalWriter {
    file: File,
    behind: WriteBehind,
}

impl Write for LocalWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.file.write(buf)?;
        self.behind.wrote(&self.file, n)?;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl remote::Writer for LocalWriter {
    fn finish(self: Box<Self>) -> io::Result<()> {
        self.behind.finish(&self.file)
    }
}

/// The two sides of a transfer.
struct Ends {
    src: End,
    dst: End,
}

/// What a copy to a server has left there so far: the temporary file being written and the
/// folders it made. A cancel the engine cannot honour in time (stuck in a request) removes
/// them on a connection of its own (`super::watch_cancel`).
#[derive(Default)]
pub(super) struct RemoteLeft {
    pub(super) connect: Option<Connector>,
    pub(super) part: Option<Vec<String>>,
    pub(super) dirs: Vec<Vec<String>>,
    /// an existing file moved aside while the copy takes its place: (hidden name, its name)
    pub(super) aside: Option<(Vec<String>, Vec<String>)>,
}

impl RemoteLeft {
    /// Removes the temporary file and the folders made (those still empty: a folder with
    /// something in it stays), deepest first; waits at most `limit`, logs what it did.
    pub(super) fn clean(self, limit: std::time::Duration) {
        let Some(connect) = self.connect else { return };
        if self.part.is_none() && self.dirs.is_empty() && self.aside.is_none() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let (part, dirs, aside) = (self.part, self.dirs, self.aside);
        let spawned = std::thread::Builder::new().name("remote-clean".into()).spawn(move || {
            let report = (|| -> io::Result<String> {
                let fs = connect()?;
                let mut done = Vec::new();
                // the old file back first: the part is only worthless once it is
                let mut restored = true;
                if let Some((aside, target)) = &aside {
                    if fs.stat(target).is_ok() {
                        let _ = fs.remove(aside, false);
                    } else if let Err(e) = fs.rename(aside, target) {
                        restored = false;
                        done.push(format!("cannot put {} back: {}", target.join("/"), describe(&e)));
                    }
                }
                if let Some(part) = part.as_ref().filter(|_| restored) {
                    match fs.remove(part, false) {
                        Ok(()) => done.push(format!("removed {}", part.join("/"))),
                        Err(e) => done.push(format!("cannot remove {}: {}", part.join("/"), describe(&e))),
                    }
                }
                let empty = dirs.iter().rev().filter(|d| fs.remove(d, true).is_ok()).count();
                if empty > 0 {
                    done.push(format!("removed {empty} empty folders it made"));
                }
                Ok(done.join(", "))
            })();
            let _ = tx.send(report);
        });
        if spawned.is_err() {
            return;
        }
        match rx.recv_timeout(limit) {
            Ok(Ok(report)) if !report.is_empty() => eprintln!("rynas-files-worker: cancelled copy, on the server: {report}"),
            Ok(Ok(_)) => {}
            Ok(Err(e)) => eprintln!("rynas-files-worker: cancelled copy: cannot connect to clean up on the server: {}", describe(&e)),
            Err(_) => eprintln!("rynas-files-worker: cancelled copy: the clean-up on the server did not finish in {} s", limit.as_secs()),
        }
    }
}

impl Engine {
    pub(super) fn remote_run(&mut self) -> Step<()> {
        match self.spec.op {
            Op::Copy | Op::Move => self.r_transfer(),
            Op::Delete => self.r_delete(),
            // the temporary files of a copy to a server that was stopped hard
            Op::Cleanup => {
                self.r_cleanup();
                Ok(())
            }
            Op::Compress | Op::Extract => {
                self.fail(c::INVALID_VALUE, t!("archives: copy the files to the NAS first"));
                Ok(())
            }
        }
    }

    fn connect(&mut self, path: &Path) -> Option<End> {
        match End::open(path, &self.spec.remotes) {
            Ok(end) => Some(end),
            Err(e) => {
                self.fail(c::REMOTE_UNREACHABLE, t!("cannot connect to the server: {}", describe(&e)));
                None
            }
        }
    }

    fn r_transfer(&mut self) -> Step<()> {
        let Some(dest) = self.spec.dest.clone() else {
            self.fail(c::INVALID_VALUE, t!("no destination"));
            return Ok(());
        };
        let sources = self.spec.sources.clone();
        let moving = self.spec.op == Op::Move;
        let Some(src) = self.connect(&sources[0]) else { return Ok(()) };
        let Some(dst) = self.connect(&dest) else { return Ok(()) };
        let ends = Ends { src, dst };
        // what a cancel removes from the server when the engine cannot (`watch_cancel`)
        self.shared.remote.lock().unwrap().connect = ends.dst.connector();

        // a local destination: written as in a local copy, never in a PC's backup folder
        if !remote::is_remote(&dest) && !self.can_write(&dest) {
            return Ok(());
        }
        match ends.dst.stat(&dest) {
            Ok(m) if m.dir => {}
            Ok(_) => {
                self.fail(c::INVALID_VALUE, t!("the destination is not a folder"));
                return Ok(());
            }
            Err(e) => {
                self.fail(crate::fsops::io_code(&e), t!("destination: {}", describe(&e)));
                return Ok(());
            }
        }
        for s in &sources {
            if dest.starts_with(s) && ends.src.stat(s).is_ok_and(|m| m.dir) {
                self.fail(c::INVALID_VALUE, t!("“{}” cannot be copied or moved into itself", self.display(s)));
                return Ok(());
            }
        }

        // a move inside one server is a rename where the server can
        let same_server = remote::split_path(&sources[0]).map(|x| x.0) == remote::split_path(&dest).map(|x| x.0) && remote::is_remote(&dest);
        let mut slow = Vec::new();
        if moving && same_server {
            self.p.phase = "move".into();
            self.p.method = "rename".into();
            self.p.files_total = sources.len() as u64;
            for s in sources {
                self.checkpoint()?;
                let Some(name) = s.file_name() else { continue };
                let target = dest.join(name);
                self.p.current = self.display(&s);
                match ends.dst.rename(&s, &target) {
                    Ok(()) => {
                        self.p.files_done += 1;
                        self.p.instant += 1;
                    }
                    Err(_) => slow.push(s),
                }
            }
            if slow.is_empty() {
                return Ok(());
            }
            self.p.files_total = self.p.files_done;
        } else {
            slow = sources;
        }

        self.r_scan(&ends, &slow)?;
        if let Some((free, _)) = ends.dst.space(&dest)
            && free < self.p.bytes_total {
                self.fail(c::INSUFFICIENT_STORAGE, t!("not enough space: {} needed, {} free", super::human(self.p.bytes_total), super::human(free)));
                return Ok(());
            }
        self.p.phase = if moving { "move" } else { "copy" }.into();
        self.p.method = "copy".into();
        let result = (|| {
            for s in &slow {
                let Some(name) = s.file_name().and_then(|n| n.to_str()).map(String::from) else { continue };
                self.r_item(&ends, s, None, &dest, &name, moving, true)?;
            }
            Ok(())
        })();
        if result.is_err() {
            // the folders this job made and left empty go; remove_dir keeps the others
            for d in std::mem::take(&mut self.made_dirs).iter().rev() {
                let _ = ends.dst.remove(d, true);
            }
        }
        self.p.current.clear();
        self.p.current_size = 0;
        self.p.current_done = 0;
        result
    }

    fn r_scan(&mut self, ends: &Ends, sources: &[PathBuf]) -> Step<()> {
        self.p.phase = "scan".into();
        self.p.scanning = true;
        let mut stack: Vec<(PathBuf, Option<Meta>)> = sources.iter().map(|s| (s.clone(), None)).collect();
        let mut seen = 0u64;
        while let Some((path, meta)) = stack.pop() {
            seen += 1;
            if seen.is_multiple_of(64) {
                self.checkpoint()?;
            }
            let Some(meta) = meta.or_else(|| ends.src.stat(&path).ok()) else { continue };
            if meta.dir {
                self.p.dirs += 1;
                if let Ok(items) = ends.src.list(&path) {
                    stack.extend(items.into_iter().map(|(n, m)| (path.join(n), Some(m))));
                }
            } else {
                self.p.files_total += 1;
                self.p.bytes_total += meta.size;
            }
        }
        self.p.scanning = false;
        self.emit_progress(true);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn r_item(&mut self, ends: &Ends, src: &Path, meta: Option<Meta>, dest_dir: &Path, name: &str, moving: bool, top: bool) -> Step<()> {
        self.checkpoint()?;
        let meta = match meta.map(Ok).unwrap_or_else(|| ends.src.stat(src)) {
            Ok(m) => m,
            Err(e) if ends.src.is_remote() && remote::retryable(&e) => return Err(self.lost(&e)),
            Err(e) => {
                self.error(src, &e);
                return Ok(());
            }
        };
        let mut target = dest_dir.join(name);
        if top && !moving && target == src {
            target = ends.dst.unique(dest_dir, name, &t!(" - Copy"));
        }
        self.p.current = self.display(src);
        self.p.current_size = meta.size;
        self.p.current_done = 0;
        let existing = ends.dst.stat(&target).ok();
        if meta.dir {
            return self.r_dir(ends, src, meta, &target, existing, moving);
        }
        if !meta.file {
            self.error_msg(src, t!("link or special file: skipped"));
            self.r_skip(meta);
            return Ok(());
        }
        let mut replace = false;
        if let Some(old) = existing {
            match self.r_decide(ends, src, meta, &target, old)? {
                Decision::Skip => {
                    self.r_skip(meta);
                    return Ok(());
                }
                Decision::Rename(other) => target = other,
                Decision::Overwrite => {
                    if old.dir {
                        self.error_msg(src, t!("a folder with the same name is in its place"));
                        self.r_skip(meta);
                        return Ok(());
                    }
                    replace = true;
                }
            }
        }
        let start = (self.p.bytes_done, self.p.verify_bytes);
        let phase = self.p.phase.clone();
        match self.r_file(ends, src, meta, &target, replace) {
            Ok(()) => {
                self.p.files_done += 1;
                if moving
                    && let Err(e) = ends.src.remove(src, false) {
                        self.error_msg(src, t!("copied, but the original cannot be deleted: {}", describe(&e)));
                    }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Err(Cancelled),
            // the connection is gone even after new tries: the next items would only wait
            // for it as well
            Err(e) if (ends.src.is_remote() || ends.dst.is_remote()) && remote::retryable(&e) => return Err(self.lost(&e)),
            Err(e) => {
                self.error(src, &e);
                self.p.files_done += 1;
                self.file_failed(start, meta.size, phase);
            }
        }
        Ok(())
    }

    fn r_skip(&mut self, meta: Meta) {
        self.p.skipped += 1;
        self.p.files_done += 1;
        self.p.bytes_done += meta.size;
        self.unverified(meta.size);
    }

    fn r_skip_tree(&mut self, ends: &Ends, root: &Path) {
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for (n, m) in ends.src.list(&dir).unwrap_or_default() {
                if m.dir {
                    stack.push(dir.join(n));
                } else {
                    self.r_skip(m);
                }
            }
        }
    }

    fn r_dir(&mut self, ends: &Ends, src: &Path, meta: Meta, target: &Path, existing: Option<Meta>, moving: bool) -> Step<()> {
        let mut target = target.to_path_buf();
        match existing {
            Some(old) if old.dir => {} // merge into it
            Some(old) => match self.r_decide(ends, src, meta, &target, old)? {
                Decision::Skip => {
                    self.r_skip_tree(ends, src);
                    return Ok(());
                }
                Decision::Rename(other) => target = other,
                Decision::Overwrite => {
                    if let Err(e) = ends.dst.remove(&target, false) {
                        self.error(src, &e);
                        self.r_skip_tree(ends, src);
                        return Ok(());
                    }
                }
            },
            None => {}
        }
        let created = ends.dst.stat(&target).is_err();
        if created {
            if let Err(e) = ends.dst.mkdir(&target) {
                self.error(src, &e);
                self.r_skip_tree(ends, src);
                return Ok(());
            }
            self.made_dirs.push(target.clone());
            if ends.dst.is_remote() {
                self.shared.remote.lock().unwrap().dirs.push(parts(&target));
            }
        }
        let children = match ends.src.list(src) {
            Ok(c) => c,
            Err(e) => {
                self.error(src, &e);
                return Ok(());
            }
        };
        for (name, m) in children {
            self.r_item(ends, &src.join(&name), Some(m), &target, &name, moving, false)?;
        }
        if created {
            ends.dst.set_mtime(&target, meta.mtime);
        }
        if moving {
            // a skipped or failed child keeps the source folder: that is right
            let _ = ends.src.remove(src, true);
        }
        Ok(())
    }

    fn r_decide(&mut self, ends: &Ends, src: &Path, meta: Meta, target: &Path, old: Meta) -> Step<Decision> {
        let dir = target.parent().unwrap_or(Path::new("/")).to_path_buf();
        let name = target.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        let policy = match self.apply_all.unwrap_or(self.spec.conflict) {
            Policy::Ask => {
                self.conflicts += 1;
                let info = ConflictInfo {
                    id: self.conflicts,
                    name: name.clone(),
                    path: self.display(src),
                    source: meta.side(),
                    target: old.side(),
                    rename_to: ends.dst.unique(&dir, &name, "").file_name().unwrap_or_default().to_string_lossy().into_owned(),
                };
                self.ask_info(info)?
            }
            p => p,
        };
        Ok(match policy {
            Policy::Skip => Decision::Skip,
            Policy::Overwrite => Decision::Overwrite,
            Policy::Rename | Policy::Ask => Decision::Rename(ends.dst.unique(&dir, &name, "")),
            Policy::Newer => {
                if meta.mtime > old.mtime {
                    Decision::Overwrite
                } else {
                    Decision::Skip
                }
            }
        })
    }

    fn r_file(&mut self, ends: &Ends, src: &Path, meta: Meta, target: &Path, replace: bool) -> io::Result<()> {
        let dir = target.parent().unwrap_or(Path::new("/"));
        let part = dir.join(part_name(target.file_name().unwrap_or_default()));
        let mut input = ends.src.reader(src)?;
        let _ = ends.dst.remove(&part, false);
        // a cancel that cannot wait for the copy to notice removes it (`watch_cancel`): a
        // local file directly, a remote one over a connection of the watchdog's own (known
        // before it is made: the request that makes it may be the one that hangs)
        if ends.dst.is_remote() {
            self.shared.remote.lock().unwrap().part = Some(parts(&part));
        }
        let mut output = match ends.dst.writer(&part) {
            Ok(w) => w,
            Err(e) => {
                self.shared.remote.lock().unwrap().part = None;
                return Err(e);
            }
        };
        if !ends.dst.is_remote() {
            *self.shared.part.lock().unwrap() = Some(part.clone());
        }
        let result = self.r_fill(ends, src, &part, &mut input, &mut output);
        drop(input);
        let mut keep_part = false;
        let result = result.and_then(|hash| {
            self.r_finish(ends, output)?;
            if let Some(hash) = hash {
                self.r_verify(ends, &part, meta.size, hash)?;
            }
            ends.dst.set_mtime(&part, meta.mtime);
            if replace { self.r_replace(ends, &part, target, &mut keep_part) } else { ends.dst.rename(&part, target) }
        });
        if result.is_err() && !keep_part {
            let _ = ends.dst.remove(&part, false);
        }
        *self.shared.part.lock().unwrap() = None;
        self.shared.remote.lock().unwrap().part = None;
        result
    }

    /// Waits for a server to finish the file (for SFTP an `fsync` that may take long) on a
    /// thread of its own, so a pause or a cancel works meanwhile; a wait that lasts shows in
    /// the progress.
    fn r_finish(&mut self, ends: &Ends, output: Box<dyn remote::Writer>) -> io::Result<()> {
        if !ends.dst.is_remote() {
            return output.finish();
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new().name("remote-finish".into()).spawn(move || {
            let _ = tx.send(output.finish());
        })?;
        let started = std::time::Instant::now();
        let mut method = None;
        let result = loop {
            match rx.recv_timeout(FINISH_POLL) {
                Ok(result) => break result,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break Err(io::Error::other("the file's closing ended unexpectedly")),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
            if method.is_none() && started.elapsed() >= FINISH_SHOWN_AFTER {
                method = Some(std::mem::replace(&mut self.p.method, "flush".into()));
                self.emit_progress(true);
            }
            if self.checkpoint().is_err() {
                break Err(io::Error::from(io::ErrorKind::Interrupted));
            }
        };
        if let Some(method) = method {
            self.p.method = method;
        }
        result
    }

    /// Puts the finished `part` in the place of `target`: in one step where the side can,
    /// otherwise through a hidden name, without ever deleting the only copy. `keep_part` set:
    /// the old file could not be put back, the new copy stays (both named in the error).
    fn r_replace(&mut self, ends: &Ends, part: &Path, target: &Path, keep_part: &mut bool) -> io::Result<()> {
        if ends.dst.replace(part, target)? {
            return Ok(());
        }
        let name = target.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let aside = target.parent().unwrap_or(Path::new("/")).join(remote::aside_name(name));
        // left by a copy stopped hard, the file it stands for being there
        if ends.dst.stat(target).is_ok() {
            let _ = ends.dst.remove(&aside, false);
        }
        // known before it happens, like the part: a cancel that cannot wait puts it back
        self.shared.remote.lock().unwrap().aside = Some((parts(&aside), parts(target)));
        let swapped = remote::swap_in::<Path>(part, target, &aside, |a, b| ends.dst.rename(a, b), |a| ends.dst.remove(a, false));
        self.shared.remote.lock().unwrap().aside = None;
        match swapped {
            Ok(None) => Ok(()),
            Ok(Some(e)) => {
                self.error_msg(target, t!("copied, but the old file could not be removed: it stays as “{}”: {}", self.display(&aside), describe(&e)));
                Ok(())
            }
            Err(failed) if failed.keep_part => {
                *keep_part = true;
                let e = failed.error;
                let msg = t!("the file could not be replaced: the old one is kept as “{}”, the new one as “{}”: {}", self.display(&aside), self.display(part), describe(&e));
                Err(rynas_files::fsops::coded(e.kind(), crate::fsops::io_code(&e), msg))
            }
            Err(failed) => Err(failed.error),
        }
    }

    fn r_fill(&mut self, ends: &Ends, src: &Path, part: &Path, input: &mut Box<dyn Source>, output: &mut Box<dyn remote::Writer>) -> io::Result<Option<u128>> {
        let mut buf = std::mem::take(&mut self.buf);
        let result = self.r_stream(ends, src, part, input, output, &mut buf);
        self.buf = buf;
        result
    }

    /// Streams the bytes; with verification on, returns their hash. When a server side
    /// breaks or stops answering, the copy goes on from where it was over a new connection
    /// ([`RECONNECTS`] tries in a row): a source only when it is still the same file
    /// ([`remote::Ident`]), a destination with the blocks it had not confirmed sent again.
    /// Blocks pass from the reader to the writer without a copy where the reader can.
    fn r_stream(&mut self, ends: &Ends, src: &Path, part: &Path, input: &mut Box<dyn Source>, output: &mut Box<dyn remote::Writer>, buf: &mut Vec<u8>) -> io::Result<Option<u128>> {
        let mut hash = self.spec.verify.then(xxhash_rust::xxh3::Xxh3::new);
        let ident = input.ident();
        // bytes of this file read and handed to the writer so far
        let mut offset = 0u64;
        let mut tries = 0u32;
        loop {
            let n = match input.next_block(buf) {
                Ok(n) => n,
                Err(e) if self.r_reconnect(&ends.src, &e, &mut tries)? => {
                    let fresh = ends.src.reader(src)?;
                    if fresh.ident() != ident {
                        return Err(changed());
                    }
                    *input = fresh;
                    input.seek(SeekFrom::Start(offset))?;
                    continue;
                }
                Err(e) => return Err(e),
            };
            if n == 0 {
                // the end of the reading is not the end of the file: a server answering "end
                // of file" early, or a file that shrank or grew, is never a finished copy
                if let Some(ident) = ident
                    && offset != ident.size
                {
                    return Err(size_changed(offset, ident.size));
                }
                // everything confirmed by the server before the file counts as copied
                while let Err(e) = output.flush() {
                    self.r_recover(ends, part, output, offset, e, &mut tries)?;
                }
                return Ok(hash.map(|h| h.digest128()));
            }
            let mut done = 0;
            while done < n {
                match output.write(&buf[done..n]) {
                    Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
                    Ok(k) => done += k,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    // writes go to explicit offsets: on from what the writer took
                    Err(e) => self.r_recover(ends, part, output, offset + done as u64, e, &mut tries)?,
                }
            }
            tries = 0;
            offset += n as u64;
            if let Some(h) = hash.as_mut() {
                h.update(&buf[..n]);
            }
            self.advance(n as u64);
            if self.checkpoint().is_err() {
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
        }
    }

    /// After a write to the destination failed with `e`: a new connection, a writer that
    /// goes on at `at`, and the blocks the old one had not got confirmed sent again. `e`
    /// back when that is not worth it or not possible.
    fn r_recover(&mut self, ends: &Ends, part: &Path, output: &mut Box<dyn remote::Writer>, at: u64, mut e: io::Error, tries: &mut u32) -> io::Result<()> {
        loop {
            if !self.r_reconnect(&ends.dst, &e, tries)? {
                return Err(e);
            }
            let pending = output.unconfirmed();
            *output = ends.dst.writer_at(part, Some(at))?;
            match output.resend(pending) {
                Ok(()) => return Ok(()),
                Err(again) => e = again,
            }
        }
    }

    /// After `e` on `end`: whether a new connection is worth it and was made. `Err` when the
    /// job was cancelled meanwhile; `Ok(false)` gives `e` back to the caller (not a
    /// connection error, a local side, tries used up, or the server out of reach).
    fn r_reconnect(&mut self, end: &End, e: &io::Error, tries: &mut u32) -> io::Result<bool> {
        if !end.is_remote() || !remote::retryable(e) || *tries >= RECONNECTS {
            return Ok(false);
        }
        *tries += 1;
        if self.checkpoint().is_err() {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        Ok(end.reconnect().is_ok())
    }

    /// Stops the job: a server side is out of reach. `Cancelled` carries it up; the job
    /// ends failed with this error (`run`).
    fn lost(&mut self, e: &io::Error) -> Cancelled {
        self.fail(c::REMOTE_UNREACHABLE, describe(e));
        Cancelled
    }

    fn r_verify(&mut self, ends: &Ends, part: &Path, size: u64, want: u128) -> io::Result<()> {
        let phase = std::mem::replace(&mut self.p.phase, "verify".into());
        self.p.current_done = 0;
        self.p.current_size = size;
        let mut back = ends.dst.reader(part)?;
        let mut check = xxhash_rust::xxh3::Xxh3::new();
        let mut buf = std::mem::take(&mut self.buf);
        let mut read = 0u64;
        let mut tries = 0u32;
        let result = loop {
            let n = match back.next_block(&mut buf) {
                Ok(n) => n,
                Err(e) => match self.r_reconnect(&ends.dst, &e, &mut tries) {
                    Ok(true) => match ends.dst.reader(part).and_then(|mut b| b.seek(SeekFrom::Start(read)).map(|_| b)) {
                        Ok(b) => {
                            back = b;
                            continue;
                        }
                        Err(e) => break Err(e),
                    },
                    Ok(false) => break Err(e),
                    Err(cancelled) => break Err(cancelled),
                },
            };
            if n == 0 {
                break Ok(());
            }
            tries = 0;
            read += n as u64;
            check.update(&buf[..n]);
            self.p.verify_bytes += n as u64;
            self.p.current_done += n as u64;
            if self.checkpoint().is_err() {
                break Err(io::Error::from(io::ErrorKind::Interrupted));
            }
        };
        self.buf = buf;
        result?;
        self.p.phase = phase;
        if check.digest128() != want {
            return Err(io::Error::other(t!("verification failed: the copy differs from the original")));
        }
        Ok(())
    }

    fn r_delete(&mut self) -> Step<()> {
        let sources = self.spec.sources.clone();
        let Some(end) = self.connect(&sources[0]) else { return Ok(()) };
        self.p.phase = "delete".into();
        self.p.method = "delete".into();
        self.p.scanning = true;
        // (path, is a folder, its items already removed)
        let mut todo: Vec<(PathBuf, bool, bool)> = Vec::new();
        let mut total = 0u64;
        let mut stack: Vec<(PathBuf, Option<Meta>)> = sources.iter().map(|s| (s.clone(), None)).collect();
        // count first, so the bar means something; deepest last
        while let Some((path, meta)) = stack.pop() {
            total += 1;
            if total.is_multiple_of(64) {
                self.p.files_total = total;
                self.checkpoint()?;
            }
            let Some(meta) = meta.or_else(|| end.stat(&path).ok()) else { continue };
            todo.push((path.clone(), meta.dir, false));
            if meta.dir {
                match end.list(&path) {
                    Ok(items) => stack.extend(items.into_iter().map(|(n, m)| (path.join(n), Some(m)))),
                    Err(e) => self.error(&path, &e),
                }
            }
        }
        self.p.scanning = false;
        self.p.files_total = todo.len() as u64;
        // children come after their folder: remove from the end
        while let Some((path, dir, _)) = todo.pop() {
            self.checkpoint()?;
            self.p.current = self.display(&path);
            if let Err(e) = end.remove(&path, dir)
                && !(dir && e.raw_os_error() == Some(libc::ENOTEMPTY)) {
                    self.error(&path, &e);
                }
            self.p.files_done += 1;
        }
        Ok(())
    }

    /// The leftovers of a copy to a server that was stopped hard (rynasd signalled the
    /// worker after a cancel, or restarted): ".name.rynas-part" files at the top of the
    /// destination and in the trees of the job's items. The folders the job made are not
    /// known any more and stay. Nothing else is touched.
    fn r_cleanup(&mut self) {
        let Some(dest) = self.spec.dest.clone() else { return };
        let end = match End::open(&dest, &self.spec.remotes) {
            Ok(end) if end.is_remote() => end,
            Ok(_) => return,
            Err(e) => {
                eprintln!("rynas-files-worker: clean-up of {}: cannot connect: {}", self.display(&dest), describe(&e));
                return;
            }
        };
        let mut stack = vec![(dest.clone(), false)];
        for src in &self.spec.sources {
            if let Some(name) = src.file_name() {
                stack.push((dest.join(name), true));
            }
        }
        self.r_sweep(&end, stack);
    }

    /// Goes through the folders of a clean-up (each with whether to go into its subfolders):
    /// removes the temporary files, puts back what was set aside.
    fn r_sweep(&self, end: &End, mut stack: Vec<(PathBuf, bool)>) {
        let (mut seen, mut removed) = (0usize, 0usize);
        while let Some((dir, deep)) = stack.pop() {
            if seen > 100_000 {
                break;
            }
            let Ok(items) = end.list(&dir) else { continue };
            // the parts that stay: the old file they replace could not be put back
            let mut keep = std::collections::HashSet::new();
            for (name, m) in items {
                seen += 1;
                if !m.dir
                    && let Some(original) = remote::aside_of(&name)
                {
                    // sorted by name, so before its part: the file a stopped copy set aside
                    // goes back to its place, unless a file is there again
                    let (aside, target) = (dir.join(&name), dir.join(original));
                    let done = if end.stat(&target).is_ok() { end.remove(&aside, false) } else { end.rename(&aside, &target) };
                    if let Err(e) = done {
                        eprintln!("rynas-files-worker: clean-up: cannot put {} back on the server: {}", self.display(&target), describe(&e));
                        keep.insert(super::part_name(std::ffi::OsStr::new(original)));
                    }
                } else if !m.dir && super::part_leftover(name.as_bytes()) && keep.contains(std::ffi::OsStr::new(&name)) {
                    eprintln!("rynas-files-worker: clean-up: keeping {}: it is the only complete copy", self.display(&dir.join(&name)));
                } else if !m.dir && super::part_leftover(name.as_bytes()) {
                    match end.remove(&dir.join(&name), false) {
                        Ok(()) => removed += 1,
                        Err(e) => eprintln!("rynas-files-worker: clean-up: cannot remove {} on the server: {}", self.display(&dir.join(&name)), describe(&e)),
                    }
                } else if deep && m.dir {
                    stack.push((dir.join(&name), true));
                }
            }
        }
        if removed > 0 {
            eprintln!("rynas-files-worker: clean-up: removed {removed} temporary files of a stopped copy on the server");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::Shared;
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    /// What a reader of the fake server does once the reading reaches `at`.
    #[derive(Clone, Copy)]
    enum Fault {
        /// the connection breaks
        Breaks(u64),
        /// the server refuses the file (a lock, a disk error): that file's error only
        Refuses(u64),
        /// the reading ends here, without an error: a server's early "end of file"
        Ends(u64),
    }

    /// A remote file; `ident` is what the open reported.
    struct FlakyReader {
        data: Arc<Vec<u8>>,
        pos: u64,
        fault: Option<Fault>,
        ident: remote::Ident,
    }

    impl Read for FlakyReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.fault {
                Some(Fault::Breaks(at)) if self.pos >= at => return Err(remote::stalled(std::time::Duration::from_secs(60))),
                Some(Fault::Refuses(at)) if self.pos >= at => return Err(remote::errno(libc::EBUSY)),
                Some(Fault::Ends(at)) if self.pos >= at => return Ok(0),
                _ => {}
            }
            let from = (self.pos as usize).min(self.data.len());
            let n = buf.len().min(self.data.len() - from).min(1 << 20);
            buf[..n].copy_from_slice(&self.data[from..from + n]);
            self.pos += n as u64;
            Ok(n)
        }
    }

    impl Seek for FlakyReader {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            let SeekFrom::Start(n) = pos else { unimplemented!() };
            self.pos = n;
            Ok(n)
        }
    }

    impl remote::Reader for FlakyReader {
        fn len(&self) -> u64 {
            self.data.len() as u64
        }
        fn ident(&self) -> remote::Ident {
            self.ident
        }
    }

    /// The files written on the fake server, shared by its connections.
    type Disk = Arc<Mutex<BTreeMap<Vec<String>, Vec<u8>>>>;

    /// A writer that takes blocks without the server confirming them yet (like the SMB
    /// writer's blocks in flight); `confirm` of them are confirmed, then the connection
    /// breaks when `break_after` bytes were handed over.
    struct LaggyWriter {
        disk: Disk,
        path: Vec<String>,
        pos: u64,
        pending: Vec<(u64, Arc<[u8]>)>,
        break_after: Option<u64>,
    }

    impl LaggyWriter {
        fn put(&self, at: u64, data: &[u8]) {
            let mut disk = self.disk.lock().unwrap();
            let file = disk.entry(self.path.clone()).or_default();
            let end = at as usize + data.len();
            if file.len() < end {
                file.resize(end, 0);
            }
            file[at as usize..end].copy_from_slice(data);
        }
    }

    impl Write for LaggyWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.break_after.is_some_and(|b| self.pos >= b) {
                return Err(remote::lost(io::ErrorKind::ConnectionReset, "reset"));
            }
            let n = buf.len().min(256 * 1024);
            // the block before this one is confirmed when the next comes: one in flight
            for (at, data) in std::mem::take(&mut self.pending) {
                self.put(at, &data);
            }
            self.pending.push((self.pos, buf[..n].into()));
            self.pos += n as u64;
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.break_after.is_some() {
                return Err(remote::lost(io::ErrorKind::ConnectionReset, "reset"));
            }
            for (at, data) in std::mem::take(&mut self.pending) {
                self.put(at, &data);
            }
            Ok(())
        }
    }

    impl remote::Writer for LaggyWriter {
        fn finish(mut self: Box<Self>) -> io::Result<()> {
            self.flush()
        }
        fn unconfirmed(&mut self) -> Vec<(u64, Arc<[u8]>)> {
            std::mem::take(&mut self.pending)
        }
        fn resend(&mut self, blocks: Vec<(u64, Arc<[u8]>)>) -> io::Result<()> {
            for (at, data) in blocks {
                self.put(at, &data);
            }
            Ok(())
        }
    }

    /// A fake server; the `nth` connection (from 1) gets the reader faults, idents and
    /// writer breaks of `faults(nth)`.
    struct FakeFs {
        data: Arc<Vec<u8>>,
        fault: Option<Fault>,
        ident: remote::Ident,
        break_after: Option<u64>,
        disk: Disk,
        removed: Arc<Mutex<Vec<Vec<String>>>>,
        /// the writer blocks forever (a server that stopped answering)
        hang: bool,
        rename: Renames,
    }

    /// How the fake server's renames go.
    #[derive(Clone, Copy, Default, PartialEq)]
    enum Renames {
        /// all work; no replace in one step
        #[default]
        Plain,
        /// the server replaces in one step
        Replaces,
        /// the temporary file cannot take the target's name
        PartFails,
        /// neither the temporary file nor the old file can get the target's name
        BothFail,
        /// the old file cannot be removed once the new one is in place
        AsideStays,
    }

    impl Fs for FakeFs {
        fn list(&self, _: &[String]) -> io::Result<Vec<remote::Entry>> {
            Ok(Vec::new())
        }
        fn stat(&self, path: &[String]) -> io::Result<remote::Entry> {
            let name = path.last().cloned().unwrap_or_default();
            if self.disk.lock().unwrap().contains_key(path) || name == "big.iso" {
                return Ok(remote::Entry { name, kind: 'f', size: self.data.len() as u64, mtime: 0, mode: 0o644 });
            }
            if name.contains('.') { Err(remote::errno(libc::ENOENT)) } else { Ok(remote::Entry { name, kind: 'd', size: 0, mtime: 0, mode: 0o755 }) }
        }
        fn reader(&self, _: &[String]) -> io::Result<Box<dyn remote::Reader>> {
            // a size the test did not set: the file's real one
            let ident = remote::Ident { size: if self.ident.size == 0 { self.data.len() as u64 } else { self.ident.size }, ..self.ident };
            Ok(Box::new(FlakyReader { data: Arc::clone(&self.data), pos: 0, fault: self.fault, ident }))
        }
        fn writer(&self, path: &[String], at: Option<u64>) -> io::Result<Box<dyn remote::Writer>> {
            if self.hang {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
            if at.is_none() {
                self.disk.lock().unwrap().insert(path.to_vec(), Vec::new());
            }
            Ok(Box::new(LaggyWriter { disk: Arc::clone(&self.disk), path: path.to_vec(), pos: at.unwrap_or(0), pending: Vec::new(), break_after: self.break_after }))
        }
        fn mkdir(&self, _: &[String]) -> io::Result<()> {
            Ok(())
        }
        fn rename(&self, from: &[String], to: &[String]) -> io::Result<()> {
            let is = |p: &[String], tag: &str| p.last().is_some_and(|n| n.ends_with(tag));
            let refused = match self.rename {
                Renames::PartFails => is(from, ".rynas-part"),
                Renames::BothFail => is(from, ".rynas-part") || is(from, ".rynas-aside"),
                _ => false,
            };
            if refused {
                return Err(remote::errno(libc::EIO));
            }
            let mut disk = self.disk.lock().unwrap();
            let data = disk.remove(from).ok_or_else(|| remote::errno(libc::ENOENT))?;
            disk.insert(to.to_vec(), data);
            Ok(())
        }
        fn replace(&self, from: &[String], to: &[String]) -> io::Result<bool> {
            if self.rename != Renames::Replaces {
                return Ok(false);
            }
            let mut disk = self.disk.lock().unwrap();
            let data = disk.remove(from).ok_or_else(|| remote::errno(libc::ENOENT))?;
            disk.insert(to.to_vec(), data);
            Ok(true)
        }
        fn remove(&self, path: &[String], _: bool) -> io::Result<()> {
            self.removed.lock().unwrap().push(path.to_vec());
            if self.rename == Renames::AsideStays && path.last().is_some_and(|n| n.ends_with(".rynas-aside")) {
                return Err(remote::errno(libc::EACCES));
            }
            self.disk.lock().unwrap().remove(path);
            Ok(())
        }
        fn space(&self, _: &[String]) -> Option<(u64, u64)> {
            None
        }
        fn set_mtime(&self, _: &[String], _: i64) {}
    }

    #[derive(Clone, Copy, Default)]
    struct Faults {
        read: Option<Fault>,
        ident: remote::Ident,
        break_after: Option<u64>,
        hang: bool,
        rename: Renames,
    }

    struct Server {
        connect: Connector,
        connects: Arc<AtomicU32>,
        data: Arc<Vec<u8>>,
        disk: Disk,
        removed: Arc<Mutex<Vec<Vec<String>>>>,
    }

    fn server(faults: fn(u32) -> Faults) -> Server {
        let data: Arc<Vec<u8>> = Arc::new((0..10u32 << 20).map(|i| (i % 251) as u8).collect());
        let connects = Arc::new(AtomicU32::new(0));
        let disk: Disk = Arc::default();
        let removed: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let (d, n, k, r) = (Arc::clone(&data), Arc::clone(&connects), Arc::clone(&disk), Arc::clone(&removed));
        let connect: Connector = Arc::new(move || -> io::Result<Box<dyn Fs>> {
            let f = faults(n.fetch_add(1, Ordering::SeqCst) + 1);
            Ok(Box::new(FakeFs { data: Arc::clone(&d), fault: f.read, ident: f.ident, break_after: f.break_after, disk: Arc::clone(&k), removed: Arc::clone(&r), hang: f.hang, rename: f.rename }))
        });
        Server { connect, connects, data, disk, removed }
    }

    fn from_server(s: &Server) -> Ends {
        Ends { src: End::Remote(RemoteEnd::new(Arc::clone(&s.connect)).unwrap()), dst: End::Local }
    }

    fn to_server(s: &Server) -> Ends {
        Ends { src: End::Local, dst: End::Remote(RemoteEnd::new(Arc::clone(&s.connect)).unwrap()) }
    }

    fn engine(dest: &Path) -> Engine {
        let spec = serde_json::from_value(serde_json::json!({ "op": "copy", "sources": [], "dest": dest })).unwrap();
        Engine::new(spec, Arc::new(Shared::new(0)))
    }

    fn meta(size: usize) -> Meta {
        Meta { dir: false, file: true, size: size as u64, mtime: 0 }
    }

    #[test]
    fn a_broken_connection_goes_on_from_where_it_stopped() {
        let dir = tempfile::tempdir().unwrap();
        // the first connection breaks after 5 MiB, the second one works
        let s = server(|nth| Faults { read: (nth == 1).then_some(Fault::Breaks(5 << 20)), ..Default::default() });
        let ends = from_server(&s);
        let mut e = engine(dir.path());
        let target = dir.path().join("big.iso");
        e.r_file(&ends, Path::new("/:c/share/big.iso"), meta(s.data.len()), &target, false).unwrap();
        assert_eq!(s.connects.load(Ordering::SeqCst), 2);
        assert!(fs::read(&target).unwrap() == *s.data, "the copy differs");
        assert_eq!(e.p.bytes_done, s.data.len() as u64);
        assert!(!dir.path().join(".big.iso.rynas-part").exists());
    }

    #[test]
    fn a_server_gone_for_good_fails_the_job_and_leaves_no_part() {
        let dir = tempfile::tempdir().unwrap();
        let s = server(|_| Faults { read: Some(Fault::Breaks(2 << 20)), ..Default::default() });
        let ends = from_server(&s);
        let mut e = engine(dir.path());
        let r = e.r_item(&ends, Path::new("/:c/share/big.iso"), Some(meta(s.data.len())), dir.path(), "big.iso", false, false);
        assert!(r.is_err(), "the job stops");
        assert_eq!(s.connects.load(Ordering::SeqCst), 1 + RECONNECTS);
        let (code, msg) = e.fatal.clone().expect("a fatal error");
        assert_eq!(code, c::REMOTE_UNREACHABLE);
        assert!(msg.contains("stopped answering"), "{msg}");
        assert!(!dir.path().join(".big.iso.rynas-part").exists());
        assert!(!dir.path().join("big.iso").exists());
        assert!(e.shared.part.lock().unwrap().is_none());
    }

    #[test]
    fn a_file_the_server_refuses_fails_alone() {
        let dir = tempfile::tempdir().unwrap();
        // a lock conflict half way: no new connection, no lost job, just that file
        let s = server(|_| Faults { read: Some(Fault::Refuses(3 << 20)), ..Default::default() });
        let ends = from_server(&s);
        let mut e = engine(dir.path());
        let r = e.r_item(&ends, Path::new("/:c/share/big.iso"), Some(meta(s.data.len())), dir.path(), "big.iso", false, false);
        assert!(r.is_ok(), "the job goes on with the next item");
        assert!(e.fatal.is_none());
        assert_eq!(e.p.errors, 1);
        assert_eq!(s.connects.load(Ordering::SeqCst), 1);
        assert!(!dir.path().join(".big.iso.rynas-part").exists());
    }

    #[test]
    fn a_download_never_lands_in_a_pc_folder() {
        let dir = tempfile::tempdir().unwrap();
        let pc = dir.path().join("pcs/laptop");
        fs::create_dir_all(pc.join("@.20261113T1543")).unwrap();
        fs::write(pc.join("keep.txt"), b"x").unwrap();
        let s = server(|_| Faults::default());
        let ends = from_server(&s);
        rynas_files::confine::set_guard(Some(vec![rynas_files::confine::PcRoot::of(&pc).unwrap()]));
        // a file into the PC's folder (or a folder in it): that item refused, nothing made
        let mut e = engine(&pc);
        let r = e.r_item(&ends, Path::new("/:c/share/big.iso"), Some(meta(s.data.len())), &pc.join("@.20261113T1543"), "big.iso", false, false);
        assert!(r.is_ok());
        assert_eq!(e.p.errors, 1);
        assert!(!pc.join("@.20261113T1543/big.iso").exists() && !pc.join("@.20261113T1543/.big.iso.rynas-part").exists());
        // the local operations themselves: no folder made, nothing removed or renamed there
        for r in [End::Local.mkdir(&pc.join("new")), End::Local.remove(&pc.join("keep.txt"), false), End::Local.rename(&pc.join("keep.txt"), &dir.path().join("out.txt"))] {
            assert_eq!(crate::fsops::io_code(&r.unwrap_err()), c::READ_ONLY);
        }
        assert!(End::Local.remove(&dir.path().join("pcs"), true).is_err(), "the folder that holds it stays");
        assert!(pc.join("keep.txt").exists() && !pc.join("new").exists());
        // beside it, the download is a download
        let mut e = engine(dir.path());
        assert!(e.r_item(&ends, Path::new("/:c/share/big.iso"), Some(meta(s.data.len())), dir.path(), "big.iso", false, false).is_ok());
        assert!(dir.path().join("big.iso").is_file());
        rynas_files::confine::set_guard(None);
    }

    #[test]
    fn a_source_that_changed_is_not_resumed() {
        let dir = tempfile::tempdir().unwrap();
        // the connection breaks; on the new one the file has another change time
        let s = server(|nth| Faults {
            read: (nth == 1).then_some(Fault::Breaks(4 << 20)),
            ident: remote::Ident { size: 10 << 20, mtime: 7, change: if nth == 1 { 100 } else { 200 } },
            ..Default::default()
        });
        let ends = from_server(&s);
        let mut e = engine(dir.path());
        let r = e.r_item(&ends, Path::new("/:c/share/big.iso"), Some(meta(s.data.len())), dir.path(), "big.iso", false, false);
        assert!(r.is_ok(), "only that file fails");
        assert!(e.fatal.is_none());
        assert_eq!(e.p.errors, 1);
        assert!(!dir.path().join("big.iso").exists(), "no copy mixing two versions");
        assert!(!dir.path().join(".big.iso.rynas-part").exists());
    }

    #[test]
    fn an_upload_sends_again_what_the_server_had_not_confirmed() {
        let src_dir = tempfile::tempdir().unwrap();
        let s = server(|nth| Faults { break_after: (nth == 1).then_some(3 << 20), ..Default::default() });
        let local = src_dir.path().join("big.iso");
        fs::write(&local, &*s.data).unwrap();
        let ends = to_server(&s);
        let mut e = engine(Path::new("/:c/share"));
        let target = Path::new("/:c/share/big.iso");
        e.r_file(&ends, &local, meta(s.data.len()), target, false).unwrap();
        assert_eq!(s.connects.load(Ordering::SeqCst), 2);
        let disk = s.disk.lock().unwrap();
        let copy = disk.get(&vec!["share".to_string(), "big.iso".to_string()]).expect("the file is there");
        assert!(*copy == *s.data, "the upload differs ({} of {} bytes)", copy.len(), s.data.len());
        assert!(!disk.keys().any(|k| k.last().unwrap().ends_with(".rynas-part")));
    }

    /// The disk key of a file in the fake share.
    fn on_share(name: &str) -> Vec<String> {
        vec!["share".to_string(), name.to_string()]
    }

    /// A move of "big.iso" over a file already on the fake server, renames per `rename`.
    fn upload_over_old(rename: Renames) -> (Server, tempfile::TempDir, Engine, Ends) {
        let faults = match rename {
            Renames::Plain => (|_| Faults { rename: Renames::Plain, ..Default::default() }) as fn(u32) -> Faults,
            Renames::Replaces => |_| Faults { rename: Renames::Replaces, ..Default::default() },
            Renames::PartFails => |_| Faults { rename: Renames::PartFails, ..Default::default() },
            Renames::BothFail => |_| Faults { rename: Renames::BothFail, ..Default::default() },
            Renames::AsideStays => |_| Faults { rename: Renames::AsideStays, ..Default::default() },
        };
        let s = server(faults);
        s.disk.lock().unwrap().insert(on_share("big.iso"), b"old".to_vec());
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("big.iso"), &*s.data).unwrap();
        let ends = to_server(&s);
        let mut e = engine(Path::new("/:c/share"));
        e.apply_all = Some(Policy::Overwrite);
        (s, dir, e, ends)
    }

    fn upload(e: &mut Engine, ends: &Ends, dir: &Path) {
        assert!(e.r_item(ends, &dir.join("big.iso"), None, Path::new("/:c/share"), "big.iso", true, false).is_ok());
    }

    #[test]
    fn an_early_end_of_file_fails_that_file_and_keeps_both_sides() {
        let dir = tempfile::tempdir().unwrap();
        let s = server(|_| Faults { read: Some(Fault::Ends(2 << 20)), ..Default::default() });
        let target = dir.path().join("big.iso");
        fs::write(&target, b"old").unwrap();
        let ends = from_server(&s);
        let mut e = engine(dir.path());
        e.apply_all = Some(Policy::Overwrite);
        assert!(e.r_item(&ends, Path::new("/:c/share/big.iso"), Some(meta(s.data.len())), dir.path(), "big.iso", true, false).is_ok());
        assert_eq!(e.p.errors, 1, "that file fails");
        assert!(e.fatal.is_none(), "the job goes on");
        assert_eq!(fs::read(&target).unwrap(), b"old", "the old file is untouched");
        assert!(!dir.path().join(".big.iso.rynas-part").exists());
        assert!(!s.removed.lock().unwrap().contains(&on_share("big.iso")), "a move keeps its source");
    }

    #[test]
    fn an_early_end_verified_is_still_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let s = server(|_| Faults { read: Some(Fault::Ends(1 << 20)), ..Default::default() });
        let ends = from_server(&s);
        let mut e = engine(dir.path());
        e.spec.verify = true;
        let target = dir.path().join("big.iso");
        let err = e.r_file(&ends, Path::new("/:c/share/big.iso"), meta(s.data.len()), &target, false).unwrap_err();
        assert!(err.to_string().contains("changed"), "{err}");
        assert!(!target.exists());
    }

    #[test]
    fn a_local_file_that_shrinks_during_an_upload_is_not_a_success() {
        // a source whose reading ends before the size it was opened with
        struct Short(std::io::Cursor<Vec<u8>>);
        impl Read for Short {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.0.read(buf)
            }
        }
        impl Seek for Short {
            fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
                self.0.seek(pos)
            }
        }
        impl Source for Short {
            fn ident(&self) -> Option<remote::Ident> {
                Some(remote::Ident { size: 2_000, mtime: 1, change: 1 })
            }
        }
        let s = server(|_| Faults::default());
        let ends = to_server(&s);
        let mut e = engine(Path::new("/:c/share"));
        let part = Path::new("/:c/share/.big.iso.rynas-part");
        let mut input: Box<dyn Source> = Box::new(Short(std::io::Cursor::new(vec![7; 1_000])));
        let mut output = ends.dst.writer(part).unwrap();
        let err = e.r_fill(&ends, Path::new("/x"), part, &mut input, &mut output).unwrap_err();
        assert_eq!(rynas_files::fsops::io_code(&err), c::CONFLICT, "{err}");
    }

    #[test]
    fn a_complete_local_upload_passes_the_size_check() {
        let s = server(|_| Faults::default());
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("big.iso"), &*s.data).unwrap();
        let ends = to_server(&s);
        let mut e = engine(Path::new("/:c/share"));
        e.r_file(&ends, &dir.path().join("big.iso"), meta(s.data.len()), Path::new("/:c/share/big.iso"), false).unwrap();
        assert!(s.disk.lock().unwrap().get(&on_share("big.iso")).is_some_and(|d| *d == *s.data));
    }

    #[test]
    fn overwriting_goes_through_the_servers_own_replace_when_it_has_one() {
        let (s, dir, mut e, ends) = upload_over_old(Renames::Replaces);
        upload(&mut e, &ends, dir.path());
        assert_eq!(e.p.errors, 0);
        assert!(s.disk.lock().unwrap().get(&on_share("big.iso")).is_some_and(|d| *d == *s.data));
        assert!(!s.removed.lock().unwrap().iter().any(|p| p.last().unwrap().contains("rynas-aside")), "no hidden name needed");
    }

    #[test]
    fn overwriting_without_replace_goes_through_a_hidden_name() {
        let (s, dir, mut e, ends) = upload_over_old(Renames::Plain);
        upload(&mut e, &ends, dir.path());
        assert_eq!(e.p.errors, 0);
        let disk = s.disk.lock().unwrap();
        assert!(disk.get(&on_share("big.iso")).is_some_and(|d| *d == *s.data));
        assert!(!disk.keys().any(|k| k.last().unwrap().contains("rynas-")), "{:?}", disk.keys());
        assert!(!dir.path().join("big.iso").exists(), "the move took the source");
    }

    #[test]
    fn a_new_copy_that_cannot_take_the_name_puts_the_old_file_back() {
        let (s, dir, mut e, ends) = upload_over_old(Renames::PartFails);
        upload(&mut e, &ends, dir.path());
        assert_eq!(e.p.errors, 1);
        let disk = s.disk.lock().unwrap();
        assert_eq!(disk.get(&on_share("big.iso")).unwrap(), b"old", "the old file is back");
        assert!(!disk.keys().any(|k| k.last().unwrap().contains("rynas-")), "{:?}", disk.keys());
        assert!(dir.path().join("big.iso").exists(), "a move that failed keeps its source");
    }

    #[test]
    fn when_the_old_file_cannot_come_back_the_new_copy_stays() {
        let (s, dir, mut e, ends) = upload_over_old(Renames::BothFail);
        upload(&mut e, &ends, dir.path());
        assert_eq!(e.p.errors, 1);
        let disk = s.disk.lock().unwrap();
        // both versions are there: the old one hidden, the new one under the part's name
        assert_eq!(disk.get(&on_share(".big.iso.rynas-aside")).unwrap(), b"old");
        assert!(disk.get(&on_share(".big.iso.rynas-part")).is_some_and(|d| *d == *s.data), "the new copy is kept");
        assert!(!s.removed.lock().unwrap().iter().filter(|p| p.last().unwrap() == ".big.iso.rynas-part").nth(1).is_some(), "only the sweep before the copy removed the part's name");
        assert!(dir.path().join("big.iso").exists(), "the source stays too");
    }

    #[test]
    fn a_new_copy_in_place_with_the_old_one_stuck_is_a_warning() {
        let (s, dir, mut e, ends) = upload_over_old(Renames::AsideStays);
        upload(&mut e, &ends, dir.path());
        assert_eq!(e.p.errors, 1);
        let disk = s.disk.lock().unwrap();
        assert!(disk.get(&on_share("big.iso")).is_some_and(|d| *d == *s.data));
        assert_eq!(disk.get(&on_share(".big.iso.rynas-aside")).unwrap(), b"old");
        assert!(!dir.path().join("big.iso").exists(), "copied: the move took the source");
    }

    #[test]
    fn a_cancel_that_cannot_wait_puts_the_old_file_back() {
        let s = server(|_| Faults::default());
        s.disk.lock().unwrap().insert(on_share(".o.iso.rynas-aside"), b"old".to_vec());
        s.disk.lock().unwrap().insert(on_share(".o.iso.rynas-part"), b"new".to_vec());
        let left = RemoteLeft {
            connect: Some(Arc::clone(&s.connect)),
            part: Some(on_share(".o.iso.rynas-part")),
            dirs: Vec::new(),
            aside: Some((on_share(".o.iso.rynas-aside"), on_share("o.iso"))),
        };
        left.clean(std::time::Duration::from_secs(5));
        let disk = s.disk.lock().unwrap();
        assert_eq!(disk.get(&on_share("o.iso")).unwrap(), b"old");
        assert!(!disk.contains_key(&on_share(".o.iso.rynas-part")));
        assert!(!disk.contains_key(&on_share(".o.iso.rynas-aside")));
    }

    #[test]
    fn the_clean_up_restores_what_a_stopped_copy_set_aside() {
        // listing names files for the clean-up: a server with its folder contents
        struct Listing(Arc<Mutex<Vec<remote::Entry>>>, FakeFs);
        impl Fs for Listing {
            fn list(&self, _: &[String]) -> io::Result<Vec<remote::Entry>> {
                Ok(self.0.lock().unwrap().clone())
            }
            fn stat(&self, p: &[String]) -> io::Result<remote::Entry> {
                if self.1.disk.lock().unwrap().contains_key(p) { self.1.stat(p) } else { Err(remote::errno(libc::ENOENT)) }
            }
            fn reader(&self, p: &[String]) -> io::Result<Box<dyn remote::Reader>> {
                self.1.reader(p)
            }
            fn writer(&self, p: &[String], at: Option<u64>) -> io::Result<Box<dyn remote::Writer>> {
                self.1.writer(p, at)
            }
            fn mkdir(&self, p: &[String]) -> io::Result<()> {
                self.1.mkdir(p)
            }
            fn rename(&self, a: &[String], b: &[String]) -> io::Result<()> {
                self.1.rename(a, b)
            }
            fn remove(&self, p: &[String], d: bool) -> io::Result<()> {
                self.1.remove(p, d)
            }
            fn space(&self, _: &[String]) -> Option<(u64, u64)> {
                None
            }
            fn set_mtime(&self, _: &[String], _: i64) {}
        }
        let disk: Disk = Arc::default();
        disk.lock().unwrap().insert(on_share(".big.iso.rynas-aside"), b"old".to_vec());
        disk.lock().unwrap().insert(on_share(".big.iso.rynas-part"), b"new".to_vec());
        let entry = |n: &str| remote::Entry { name: n.into(), kind: 'f', size: 3, mtime: 0, mode: 0o644 };
        let listing = Arc::new(Mutex::new(vec![entry(".big.iso.rynas-aside"), entry(".big.iso.rynas-part")]));
        let conn = RemoteEnd {
            connect: Arc::new(|| Err(remote::errno(libc::ECONNREFUSED))),
            fs: RefCell::new(Box::new(Listing(listing, FakeFs { data: Arc::new(Vec::new()), fault: None, ident: Default::default(), break_after: None, disk: Arc::clone(&disk), removed: Arc::default(), hang: false, rename: Renames::Plain }))),
        };
        let end = End::Remote(conn);
        engine(Path::new("/:c/share")).r_sweep(&end, vec![(PathBuf::from("/:c/share"), false)]);
        let disk = disk.lock().unwrap();
        assert_eq!(disk.get(&on_share("big.iso")).unwrap(), b"old");
        assert!(!disk.contains_key(&on_share(".big.iso.rynas-aside")));
        assert!(!disk.contains_key(&on_share(".big.iso.rynas-part")));
    }

    #[test]
    fn a_cancel_that_cannot_wait_cleans_the_server() {
        // the writer of the temporary file hangs (a server that stopped answering): what
        // the watchdog knows lets it remove the part and the folders on a new connection
        let s = server(|nth| Faults { hang: nth == 1, ..Default::default() });
        let src_dir = tempfile::tempdir().unwrap();
        let local = src_dir.path().join("big.iso");
        fs::write(&local, b"data").unwrap();
        let shared = Arc::new(Shared::new(0));
        let spec = serde_json::from_value(serde_json::json!({ "op": "copy", "sources": [local], "dest": "/:c/share/new" })).unwrap();
        let mut e = Engine::new(spec, Arc::clone(&shared));
        let connect = Arc::clone(&s.connect);
        std::thread::spawn(move || {
            let ends = Ends { src: End::Local, dst: End::Remote(RemoteEnd::new(connect).unwrap()) };
            shared_setup(&mut e, &ends);
            let _ = e.r_file(&ends, &local, meta(4), Path::new("/:c/share/new/big.iso"), false);
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while shared.remote.lock().unwrap().part.is_none() {
            assert!(std::time::Instant::now() < deadline, "the part was never recorded");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let left = std::mem::take(&mut *shared.remote.lock().unwrap());
        assert_eq!(left.dirs, vec![vec!["share".to_string(), "new".to_string()]]);
        left.clean(std::time::Duration::from_secs(5));
        let removed = s.removed.lock().unwrap().clone();
        assert!(removed.contains(&vec!["share".to_string(), "new".to_string(), ".big.iso.rynas-part".to_string()]), "{removed:?}");
        assert_eq!(removed.last().unwrap(), &vec!["share".to_string(), "new".to_string()]);
    }

    /// What `r_transfer` and `r_dir` record before a file of a new folder is copied.
    fn shared_setup(e: &mut Engine, ends: &Ends) {
        e.shared.remote.lock().unwrap().connect = ends.dst.connector();
        e.shared.remote.lock().unwrap().dirs.push(parts(Path::new("/:c/share/new")));
    }

    #[test]
    fn retried_requests_take_already_done_as_done() {
        // a remove whose answer was lost: on the new connection the file is gone
        struct Gone(Arc<AtomicU32>);
        impl Fs for Gone {
            fn list(&self, _: &[String]) -> io::Result<Vec<remote::Entry>> {
                Ok(Vec::new())
            }
            fn stat(&self, _: &[String]) -> io::Result<remote::Entry> {
                Err(remote::errno(libc::ENOENT))
            }
            fn reader(&self, _: &[String]) -> io::Result<Box<dyn remote::Reader>> {
                Err(remote::errno(libc::ENOENT))
            }
            fn writer(&self, _: &[String], _: Option<u64>) -> io::Result<Box<dyn remote::Writer>> {
                Err(remote::errno(libc::ENOENT))
            }
            fn mkdir(&self, _: &[String]) -> io::Result<()> {
                if self.0.load(Ordering::SeqCst) == 1 { Err(remote::stalled(std::time::Duration::from_secs(1))) } else { Err(remote::errno(libc::EEXIST)) }
            }
            fn rename(&self, _: &[String], _: &[String]) -> io::Result<()> {
                Err(remote::errno(libc::ENOENT))
            }
            fn remove(&self, _: &[String], _: bool) -> io::Result<()> {
                if self.0.load(Ordering::SeqCst) == 1 { Err(remote::lost(io::ErrorKind::ConnectionReset, "reset")) } else { Err(remote::errno(libc::ENOENT)) }
            }
            fn space(&self, _: &[String]) -> Option<(u64, u64)> {
                None
            }
            fn set_mtime(&self, _: &[String], _: i64) {}
        }
        let n = Arc::new(AtomicU32::new(0));
        let k = Arc::clone(&n);
        let end = End::Remote(RemoteEnd::new(Arc::new(move || -> io::Result<Box<dyn Fs>> {
            k.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Gone(Arc::clone(&k))))
        }))
        .unwrap());
        end.remove(Path::new("/:c/share/a.txt"), false).unwrap();
        assert_eq!(n.load(Ordering::SeqCst), 2);
        // a first answer of "not there" is an error, as always
        assert_eq!(end.remove(Path::new("/:c/share/a.txt"), false).unwrap_err().raw_os_error(), Some(libc::ENOENT));
        // a stat that fails for the connection gets the new connection too
        n.store(0, Ordering::SeqCst);
        let end2 = End::Remote(RemoteEnd::new(Arc::new(|| -> io::Result<Box<dyn Fs>> { Err(remote::errno(libc::ECONNREFUSED)) })).unwrap_or_else(|_| {
            // a server that refuses: the job's own connect fails
            RemoteEnd { connect: Arc::new(|| Err(remote::errno(libc::ECONNREFUSED))), fs: RefCell::new(Box::new(Gone(Arc::new(AtomicU32::new(1))))) }
        }));
        let e = end2.mkdir(Path::new("/:c/share/d")).unwrap_err();
        assert!(remote::retryable(&e), "the connection's error is kept when no new connection can be made");
    }
}
