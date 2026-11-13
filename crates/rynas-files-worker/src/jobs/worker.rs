//! The process that moves the data. The daemon starts one per job, already
//! running as the user who asked, and reads its progress from stdout; the
//! commands (pause, cancel, an answer to a conflict) arrive on stdin.
//!
//! Copies go through a hidden temporary name beside the destination and
//! are renamed into place once complete, so an interrupted copy never
//! leaves a half file under the real name. On btrfs a copy is first tried
//! as a reflink clone, which shares the data and takes no time; moves are
//! renames whenever source and destination are the same subvolume.

use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, BufRead, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rynas_files::confine::{self, Jail};
use rynas_files::jobs::protocol::{Command, ConflictInfo, Event, Op, Policy, Progress, RecycleRoot, Side, Spec, Status};
use crate::fsops::{describe, rename_noreplace};
use crate::jobs::writeback::WriteBehind;
use rynas_api_types::error_codes as c;

#[path = "remote_jobs.rs"]
mod remote_jobs;

const EMIT_EVERY: Duration = Duration::from_millis(150);
const CHUNK: usize = 4 * 1024 * 1024;
/// Suffix of the hidden names copies are written under (see `part_name`).
const PART_TAG: &str = "rynas-part";
/// The same, as SynoFile named them: cleaned up as well.
const LEGACY_PART_TAG: &str = "synofile-part";
/// FICLONE, _IOW(0x94, 9, int): same as BTRFS_IOC_CLONE, in every Linux kernel.
const FICLONE: libc::c_ulong = 0x4004_9409;

#[derive(Default)]
struct Control {
    paused: bool,
    cancel: bool,
    limit: u64,
    answer: Option<(u64, Policy, bool)>,
    limit_changed: bool,
}

struct Shared {
    state: Mutex<Control>,
    wake: Condvar,
    /// the local temporary file being written now, for a cancel that cannot wait
    part: Mutex<Option<PathBuf>>,
    /// the same on a server, and the folders the job made there
    remote: Mutex<remote_jobs::RemoteLeft>,
    /// set by whoever reports the end first: the engine, or the cancel watchdog
    ended: Mutex<bool>,
}

impl Shared {
    fn new(limit: u64) -> Self {
        Self {
            state: Mutex::new(Control { limit, ..Default::default() }),
            wake: Condvar::new(),
            part: Mutex::new(None),
            remote: Mutex::new(remote_jobs::RemoteLeft::default()),
            ended: Mutex::new(false),
        }
    }

    /// Whether the caller is the one to report the end of the job.
    fn claim_end(&self) -> bool {
        !std::mem::replace(&mut *self.ended.lock().unwrap(), true)
    }
}

/// How long a cancel waits for the engine to stop by itself (it does between two chunks)
/// before the process ends anyway: a call blocked on a server must not keep the job alive.
const CANCEL_GRACE: Duration = Duration::from_secs(3);
/// How long the cancel watchdog spends removing a copy's leftovers on a server (connecting
/// included) before the process ends anyway; rynasd's clean-up run catches what is left.
const REMOTE_CLEANUP: Duration = Duration::from_secs(10);

/// After a cancel (or the daemon gone): gives the engine [`CANCEL_GRACE`] to stop, then
/// removes the temporary file it was writing, reports the job cancelled and ends the
/// process. rynasd also signals a worker that outlives a cancel (`rynas_files::jobs`).
fn watch_cancel(shared: Arc<Shared>) {
    let deadline = Instant::now() + CANCEL_GRACE;
    while Instant::now() < deadline {
        if *shared.ended.lock().unwrap() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !shared.claim_end() {
        return;
    }
    if let Some(part) = shared.part.lock().unwrap().take() {
        let _ = fs::remove_file(part);
    }
    // a copy to a server: its temporary file and empty folders, over a new connection
    // (the engine's may be the one that hangs); bounded, logged
    std::mem::take(&mut *shared.remote.lock().unwrap()).clean(REMOTE_CLEANUP);
    emit(&Event::Done { status: Status::Cancelled, msg: t!("cancelled").to_string(), code: String::new() });
    std::process::exit(0);
}

/// Stops the whole job; per-item failures are recorded and skipped.
struct Cancelled;

type Step<T> = Result<T, Cancelled>;

pub fn run() -> std::process::ExitCode {
    let stdin = io::stdin();
    let mut first = String::new();
    if stdin.lock().read_line(&mut first).is_err() {
        return std::process::ExitCode::FAILURE;
    }
    let spec: Spec = match serde_json::from_str(&first) {
        Ok(spec) => spec,
        Err(e) => {
            emit(&Event::Done { status: Status::Failed, msg: t!("invalid request: {}", e), code: c::INTERNAL.into() });
            return std::process::ExitCode::FAILURE;
        }
    };
    // a job's SMB connections say once what they negotiated (rynasd's journal)
    crate::remote::smb::log_connections();
    crate::remote::sftp::log_connections();
    // every local path inside its own share, links included (rynas_files::confine)
    let _jail = Jail::enter();
    // an administrator's job never writes in a PC's backup folder (confine::refuse_pc_write)
    confine::set_guard(spec.pc_roots.clone());
    let (spec, refused) = match confine_spec(spec) {
        Ok(confined) => confined,
        Err(e) => {
            emit(&Event::Done { status: Status::Failed, msg: describe(&e), code: crate::fsops::io_code(&e).into() });
            return std::process::ExitCode::SUCCESS;
        }
    };
    let shared = Arc::new(Shared::new(spec.limit));
    let reader = Arc::clone(&shared);
    std::thread::spawn(move || {
        let watch = |shared: &Arc<Shared>| {
            let shared = Arc::clone(shared);
            std::thread::spawn(move || watch_cancel(shared));
        };
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let Ok(cmd) = serde_json::from_str::<Command>(&line) else { continue };
            let mut s = reader.state.lock().unwrap();
            match cmd {
                Command::Pause => s.paused = true,
                Command::Resume => s.paused = false,
                Command::Cancel => {
                    if !s.cancel {
                        watch(&reader);
                    }
                    s.cancel = true;
                }
                Command::Limit { bps } => {
                    s.limit = bps;
                    s.limit_changed = true;
                }
                Command::Answer { id, action, all } => s.answer = Some((id, action, all)),
            }
            reader.wake.notify_all();
        }
        // the daemon went away: nobody can answer or watch, stop cleanly
        let mut s = reader.state.lock().unwrap();
        if !s.cancel {
            watch(&reader);
        }
        s.cancel = true;
        reader.wake.notify_all();
    });

    let mut engine = Engine::new(spec, Arc::clone(&shared));
    for (path, e) in refused {
        engine.error(&path, &e);
    }
    let outcome = engine.run();
    // a cancel watchdog that already reported the end is ending the process
    if !shared.claim_end() {
        return std::process::ExitCode::SUCCESS;
    }
    let cancelled = shared.state.lock().unwrap().cancel;
    let (status, msg) = match outcome {
        Ok(()) => (Status::Ok, engine.summary()),
        // stopped by an error (a server out of reach), not by the user
        Err(Cancelled) if engine.fatal.is_some() && !cancelled => (Status::Failed, String::new()),
        Err(Cancelled) => {
            // deepest first; remove_dir leaves any folder that holds something
            for dir in engine.made_dirs.iter().rev() {
                let _ = fs::remove_dir(dir);
            }
            (Status::Cancelled, t!("cancelled").to_string())
        }
    };
    let status = if status == Status::Ok && engine.fatal.is_some() { Status::Failed } else { status };
    let (code, msg) = match engine.fatal.take() {
        Some((code, msg)) => (code.to_string(), msg),
        None => (String::new(), msg),
    };
    engine.emit_progress(true);
    emit(&Event::Done { status, msg, code });
    std::process::ExitCode::SUCCESS
}

/// The job's local paths as real paths inside their shares ([`Spec::roots`]): a source is
/// the item itself (a link stays a link: copied, moved, deleted as one), the destination is
/// followed. Sources a link leads out of the share are left out, each with its error; a
/// destination out of the share fails the job. Remote paths stay as they are.
fn confine_spec(mut spec: Spec) -> io::Result<(Spec, Vec<(PathBuf, io::Error)>)> {
    let roots: Vec<_> = spec.roots.iter().filter_map(|p| Jail::admit(p).ok()).collect();
    let resolve = |path: &Path, follow: bool| -> io::Result<PathBuf> {
        let (root, rel) = roots
            .iter()
            .filter_map(|r| Some((r, r.rel_of(path)?)))
            .max_by_key(|(_, rel)| std::cmp::Reverse(rel.as_os_str().len()))
            .ok_or_else(confine::outside)?;
        root.resolve(rel, follow)
    };
    let mut refused = Vec::new();
    let mut sources = Vec::new();
    for src in std::mem::take(&mut spec.sources) {
        // a clean-up only takes the sources' names, below the destination
        if crate::remote::is_remote(&src) || spec.op == Op::Cleanup {
            sources.push(src);
            continue;
        }
        match resolve(&src, false) {
            Ok(real) => sources.push(real),
            Err(e) => refused.push((src, e)),
        }
    }
    spec.sources = sources;
    if let Some(dest) = spec.dest.take() {
        spec.dest = Some(if crate::remote::is_remote(&dest) { dest } else { resolve(&dest, true)? });
    }
    Ok((spec, refused))
}

fn emit(event: &Event) {
    let mut out = io::stdout().lock();
    let _ = serde_json::to_writer(&mut out, event);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

struct Engine {
    spec: Spec,
    shared: Arc<Shared>,
    p: Progress,
    last_emit: Instant,
    apply_all: Option<Policy>,
    conflicts: u64,
    /// folder the sources live in: progress names are relative to it
    base: PathBuf,
    window: (Instant, u64),
    buf: Vec<u8>,
    clone_works: Option<bool>,
    /// what stopped the job: an error code and the message
    fatal: Option<(&'static str, String)>,
    /// folders this job made: on cancel the ones left empty go
    made_dirs: Vec<PathBuf>,
}

impl Engine {
    fn new(spec: Spec, shared: Arc<Shared>) -> Self {
        let base = spec.sources.first().and_then(|s| s.parent()).map(Path::to_path_buf).unwrap_or_default();
        Self {
            spec,
            shared,
            p: Progress::default(),
            last_emit: Instant::now() - EMIT_EVERY,
            apply_all: None,
            conflicts: 0,
            base,
            window: (Instant::now(), 0),
            buf: Vec::new(),
            clone_works: None,
            fatal: None,
            made_dirs: Vec::new(),
        }
    }

    fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.base).unwrap_or(path).to_string_lossy().into_owned()
    }

    fn emit_progress(&mut self, force: bool) {
        if force || self.last_emit.elapsed() >= EMIT_EVERY {
            self.last_emit = Instant::now();
            emit(&Event::Progress(self.p.clone()));
        }
    }

    fn error(&mut self, path: &Path, e: &io::Error) {
        self.error_code(path, crate::fsops::io_code(e), describe(e));
    }

    fn error_msg(&mut self, path: &Path, msg: String) {
        self.error_code(path, c::INTERNAL, msg);
    }

    fn error_code(&mut self, path: &Path, code: &'static str, msg: String) {
        self.p.errors += 1;
        emit(&Event::Error { path: self.display(path), msg, code: code.into() });
    }

    /// Whether `src` is the top of a read-only snapshot, which is never moved nor deleted
    /// ([`crate::fsops::refuse_snapshot`]): the error is counted for it.
    fn snapshot_refused(&mut self, src: &Path) -> bool {
        match crate::fsops::refuse_snapshot(src) {
            Ok(()) => false,
            Err(e) => {
                self.error_code(src, c::READ_ONLY, e.message);
                self.p.files_done += 1;
                true
            }
        }
    }

    /// An item that failed while being written to `target`. When the user may not write
    /// into the destination folder, the error names that place (`<destination>/…`, the way
    /// the file manager shows it) instead of the source item, so the interface can point at
    /// the permissions of the shared folder there.
    fn write_error(&mut self, src: &Path, target: &Path, e: &io::Error) {
        let code = crate::fsops::io_code(e);
        if code == c::PERMISSION_DENIED
            && let Some(dir) = target.parent()
            && !crate::fsops::access(dir, libc::W_OK | libc::X_OK)
        {
            self.p.errors += 1;
            let msg = t!("you are not allowed to write to the folder “{}”", self.dest_display(dir));
            emit(&Event::Error { path: self.dest_display(target), msg, code: code.into() });
            return;
        }
        self.error(src, e);
    }

    /// A path under the destination, as `<destination folder name>/<path below it>`.
    fn dest_display(&self, p: &Path) -> String {
        let Some(dest) = &self.spec.dest else { return p.display().to_string() };
        let top = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| dest.display().to_string());
        match p.strip_prefix(dest) {
            Ok(rel) if rel.as_os_str().is_empty() => top,
            Ok(rel) => format!("{top}/{}", rel.display()),
            Err(_) => p.display().to_string(),
        }
    }

    /// Stops the job with this error.
    fn fail(&mut self, code: &'static str, msg: String) {
        self.fatal = Some((code, msg));
    }

    /// Between two chunks: honours pause, cancel and the speed limit, and
    /// reports progress now and then.
    fn checkpoint(&mut self) -> Step<()> {
        let mut paused_here = false;
        {
            let mut s = self.shared.state.lock().unwrap();
            loop {
                if s.cancel {
                    return Err(Cancelled);
                }
                if !s.paused {
                    break;
                }
                if !self.p.paused {
                    self.p.paused = true;
                    drop(s);
                    self.emit_progress(true);
                    s = self.shared.state.lock().unwrap();
                }
                paused_here = true;
                s = self.shared.wake.wait_timeout(s, Duration::from_millis(500)).unwrap().0;
            }
            if s.limit_changed {
                s.limit_changed = false;
                paused_here = true;
            }
        }
        if paused_here {
            self.p.paused = false;
            self.window = (Instant::now(), 0);
            self.emit_progress(true);
        }
        self.throttle()?;
        self.emit_progress(false);
        Ok(())
    }

    fn throttle(&mut self) -> Step<()> {
        loop {
            let limit = self.shared.state.lock().unwrap().limit;
            if limit == 0 {
                return Ok(());
            }
            let due = Duration::from_secs_f64(self.window.1 as f64 / limit as f64);
            let elapsed = self.window.0.elapsed();
            if due <= elapsed {
                // a fresh window every few seconds keeps a change of limit quick to take effect
                if elapsed > Duration::from_secs(3) {
                    self.window = (Instant::now(), 0);
                }
                return Ok(());
            }
            std::thread::sleep((due - elapsed).min(Duration::from_millis(200)));
            if self.shared.state.lock().unwrap().cancel {
                return Err(Cancelled);
            }
            self.emit_progress(false);
        }
    }

    fn advance(&mut self, n: u64) {
        self.p.bytes_done += n;
        self.p.current_done += n;
        self.window.1 += n;
    }

    /// `n` bytes counted as done that the verification will never read (skipped, renamed):
    /// with verification on they count towards its share too, so it reaches the end.
    fn unverified(&mut self, n: u64) {
        if self.spec.verify {
            self.p.verify_skip += n;
        }
    }

    /// A file of `len` bytes failed after `bytes_done` and `verify_bytes` were `start`: it
    /// counts as handled, whatever part of it was copied or verified, so the bar and the
    /// verify share still reach their end. The phase it was in (the verify pass) is undone.
    fn file_failed(&mut self, start: (u64, u64), len: u64, phase: String) {
        self.p.bytes_done = self.p.bytes_done.max(start.0 + len);
        let verified = self.p.verify_bytes.saturating_sub(start.1);
        self.unverified(len.saturating_sub(verified));
        self.p.phase = phase;
    }

    fn run(&mut self) -> Step<()> {
        // a local destination is cleaned whatever the sources were (a copy from a server
        // cancelled mid-file leaves its temporary file here)
        if self.spec.op == Op::Cleanup && self.spec.dest.as_deref().is_some_and(|d| !crate::remote::is_remote(d)) {
            self.cleanup();
            return Ok(());
        }
        if self.spec.sources.iter().chain(&self.spec.dest).any(|p| crate::remote::is_remote(p)) {
            return self.remote_run();
        }
        match self.spec.op {
            Op::Delete => self.delete(),
            Op::Copy | Op::Move => self.transfer(),
            Op::Compress => self.compress(),
            Op::Extract => self.extract(),
            Op::Cleanup => {
                self.cleanup();
                Ok(())
            }
        }
    }

    /// The leftovers of a job the restart cut short: ".name.rynas-part"
    /// files and ".rynas-x-…" extraction folders, in the destination and
    /// in the trees it was copying into. Browser uploads abandoned there for
    /// a day go too. Nothing else is touched.
    fn cleanup(&mut self) {
        let Some(dest) = self.spec.dest.clone() else { return };
        let clean = |dir: &Path, deep: bool, stack: &mut Vec<PathBuf>, seen: &mut usize| {
            let Ok(items) = confine::read_dir(dir) else { return };
            for item in items.flatten() {
                *seen += 1;
                let name = item.file_name();
                let Ok(kind) = item.file_type() else { continue };
                if leftover(&name, &kind, || item.metadata().ok(), std::time::SystemTime::now()) {
                    let _ = if kind.is_dir() { fs::remove_dir_all(item.path()) } else { fs::remove_file(item.path()) };
                } else if deep && kind.is_dir() {
                    stack.push(item.path());
                }
            }
        };
        let mut seen = 0usize;
        let mut stack = Vec::new();
        // the destination at its top level, the trees of the job's items in full
        clean(&dest, false, &mut stack, &mut seen);
        for src in &self.spec.sources {
            if let Some(name) = src.file_name() {
                stack.push(dest.join(name));
            }
        }
        while let Some(dir) = stack.pop() {
            if seen > 500_000 {
                return;
            }
            clean(&dir, true, &mut stack, &mut seen);
        }
    }

    fn summary(&self) -> String {
        let mut parts = vec![];
        if self.p.skipped > 0 {
            parts.push(t!("{} skipped", self.p.skipped));
        }
        if self.p.errors > 0 {
            parts.push(t!("{} errors", self.p.errors));
        }
        if parts.is_empty() { t!("complete") } else { t!("complete: {}", parts.join(", ")) }
    }

    // ---- copy and move ----

    fn transfer(&mut self) -> Step<()> {
        let Some(dest) = self.spec.dest.clone() else {
            self.fail(c::INVALID_VALUE, t!("no destination"));
            return Ok(());
        };
        let moving = self.spec.op == Op::Move;
        let sources = self.spec.sources.clone();

        if !self.can_write(&dest) {
            return Ok(());
        }
        // refuse a folder into itself before touching anything
        let dest_real = fs::canonicalize(&dest).unwrap_or_else(|_| dest.clone());
        for src in &sources {
            let src_real = fs::canonicalize(src).unwrap_or_else(|_| src.clone());
            if fs::symlink_metadata(src).is_ok_and(|m| m.is_dir()) && dest_real.starts_with(&src_real) {
                self.fail(c::INVALID_VALUE, t!("“{}” cannot be copied or moved into itself", self.display(src)));
                return Ok(());
            }
        }

        // moves inside one subvolume are renames: no scan, no data
        let mut slow = Vec::new();
        if moving {
            self.p.phase = "move".into();
            self.p.method = "rename".into();
            self.p.files_total = sources.len() as u64;
            for src in sources {
                self.checkpoint()?;
                let Some(name) = src.file_name().map(OsStr::to_os_string) else { continue };
                let target = dest.join(&name);
                if src == target {
                    self.p.skipped += 1;
                    self.p.files_done += 1;
                    continue;
                }
                // a PC's backup folder, or one that holds it, never moves away
                if let Err(e) = confine::refuse_pc_write(&src, true) {
                    self.error(&src, &e);
                    self.p.files_done += 1;
                    continue;
                }
                if self.snapshot_refused(&src) {
                    continue;
                }
                self.p.current = self.display(&src);
                match rename_noreplace(&src, &target) {
                    Ok(()) => {
                        self.p.files_done += 1;
                        self.p.instant += 1;
                    }
                    Err(e) if e.raw_os_error() == Some(libc::EXDEV) || e.kind() == io::ErrorKind::AlreadyExists => {
                        slow.push(src)
                    }
                    Err(e) => {
                        self.write_error(&src, &target, &e);
                        self.p.files_done += 1;
                    }
                }
            }
            if slow.is_empty() {
                return Ok(());
            }
            // what is left is counted again by the scan
            self.p.files_total = self.p.files_done;
        } else {
            slow = sources;
        }

        self.scan(&slow)?;
        if !self.room_for(&slow, &dest) {
            return Ok(());
        }
        self.p.phase = if moving { "move" } else { "copy" }.into();
        for src in slow {
            let Some(name) = src.file_name().map(OsStr::to_os_string) else { continue };
            self.item(&src, &dest, &name, moving, true)?;
        }
        self.p.current.clear();
        self.p.current_size = 0;
        self.p.current_done = 0;
        Ok(())
    }

    fn scan(&mut self, sources: &[PathBuf]) -> Step<()> {
        self.p.phase = "scan".into();
        self.p.scanning = true;
        let mut seen = 0u64;
        walk(sources, |_, meta| {
            seen += 1;
            if seen.is_multiple_of(512) {
                self.checkpoint()?;
            }
            if meta.is_dir() {
                self.p.dirs += 1;
            } else {
                self.p.files_total += 1;
                if meta.is_file() {
                    self.p.bytes_total += meta.len();
                }
            }
            Ok(())
        })?;
        self.p.scanning = false;
        self.emit_progress(true);
        Ok(())
    }

    /// Fails the job at once when the user may not write into `dest`: else
    /// every item would fail on its own, after the whole scan.
    fn can_write(&mut self, dest: &Path) -> bool {
        // never into a PC's backup folder, whatever path leads there
        if let Err(e) = confine::refuse_pc_write(dest, false) {
            self.fail(crate::fsops::io_code(&e), describe(&e));
            return false;
        }
        if crate::fsops::access(dest, libc::W_OK | libc::X_OK) {
            return true;
        }
        let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| dest.display().to_string());
        self.fail(c::PERMISSION_DENIED, t!("you are not allowed to write to the folder “{}”", name));
        false
    }

    /// Fails the job early when the data cannot fit. A copy inside one
    /// btrfs mount may become clones that take no room, so it is let go.
    fn room_for(&mut self, sources: &[PathBuf], dest: &Path) -> bool {
        let Some((free, _)) = crate::fsops::space(dest) else { return true };
        if free >= self.p.bytes_total {
            return true;
        }
        let mounts = fs::read_to_string("/proc/self/mounts").unwrap_or_default();
        let target = mount_of(&mounts, dest);
        let all_clonable = sources.iter().all(|s| {
            let m = mount_of(&mounts, s);
            m.is_some() && m == target && target.as_ref().is_some_and(|t| t.1 == "btrfs")
        });
        if all_clonable && !self.spec.verify {
            return true;
        }
        self.fail(c::INSUFFICIENT_STORAGE, t!("not enough space: {} needed, {} free",
            human(self.p.bytes_total),
            human(free)
        ));
        false
    }

    /// One item of the tree: `src` lands in `dest_dir` as `name`.
    fn item(&mut self, src: &Path, dest_dir: &Path, name: &OsStr, moving: bool, top: bool) -> Step<()> {
        self.checkpoint()?;
        let meta = match fs::symlink_metadata(src) {
            Ok(meta) => meta,
            Err(e) => {
                self.error(src, &e);
                return Ok(());
            }
        };
        // the final guard: the folder written in (a folder merged into may be a link or a
        // mount elsewhere) is never a PC's backup, and a move never takes one away
        if let Err(e) = confine::refuse_pc_write(dest_dir, false).and_then(|()| if moving { confine::refuse_pc_write(src, true) } else { Ok(()) }) {
            self.error(src, &e);
            self.skip_tree(src);
            return Ok(());
        }
        let mut target = dest_dir.join(name);
        if top && !moving && target == src {
            // copy onto itself: keep both, the way every file manager does
            target = unique_name(dest_dir, name, &t!(" - Copy"));
        }
        self.p.current = self.display(src);
        self.p.current_size = if meta.is_file() { meta.len() } else { 0 };
        self.p.current_done = 0;

        let existing = fs::symlink_metadata(&target).ok();
        if meta.is_dir() {
            return self.dir(src, &meta, &target, existing, moving);
        }
        let mut replace = false;
        if let Some(old) = existing {
            match self.decide(src, &meta, &target, &old)? {
                Decision::Skip => {
                    self.skip(&meta);
                    return Ok(());
                }
                Decision::Rename(other) => target = other,
                Decision::Overwrite => {
                    if old.is_dir() {
                        self.error_msg(src, t!("a folder with the same name is in its place"));
                        self.skip(&meta);
                        return Ok(());
                    }
                    replace = true;
                }
            }
        }
        if moving {
            match fs::rename(src, &target) {
                Ok(()) => {
                    self.p.method = "rename".into();
                    self.p.instant += 1;
                    self.p.files_done += 1;
                    let len = if meta.is_file() { meta.len() } else { 0 };
                    self.advance(len);
                    self.unverified(len);
                    return Ok(());
                }
                Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {}
                Err(e) => {
                    self.write_error(src, &target, &e);
                    self.skip(&meta);
                    return Ok(());
                }
            }
        }
        let t = meta.file_type();
        let start = (self.p.bytes_done, self.p.verify_bytes);
        let phase = self.p.phase.clone();
        let result = if t.is_file() {
            self.file(src, &meta, &target)
        } else if t.is_symlink() {
            fs::read_link(src).and_then(|link| {
                if replace {
                    let _ = fs::remove_file(&target);
                }
                std::os::unix::fs::symlink(link, &target).and_then(|()| confine::adopt_link(&target))
            })
        } else {
            Err(io::Error::other(t!("special file (device, socket or pipe): skipped")))
        };
        match result {
            Ok(()) => {
                self.p.files_done += 1;
                if moving
                    && let Err(e) = fs::remove_file(src) {
                        self.error_msg(src, t!("copied, but the original cannot be deleted: {}", describe(&e)));
                    }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Err(Cancelled),
            Err(e) => {
                self.write_error(src, &target, &e);
                self.p.files_done += 1;
                self.file_failed(start, if t.is_file() { meta.len() } else { 0 }, phase);
            }
        }
        Ok(())
    }

    fn skip(&mut self, meta: &Metadata) {
        self.p.skipped += 1;
        self.p.files_done += 1;
        if meta.is_file() {
            // skipped bytes count as handled, so the bar and the time left stay true
            self.p.bytes_done += meta.len();
            self.unverified(meta.len());
        }
    }

    fn dir(&mut self, src: &Path, meta: &Metadata, target: &Path, existing: Option<Metadata>, moving: bool) -> Step<()> {
        let mut target = target.to_path_buf();
        match existing {
            Some(old) if old.is_dir() => {} // merge into it
            Some(old) => match self.decide(src, meta, &target, &old)? {
                Decision::Skip => {
                    self.skip_tree(src);
                    return Ok(());
                }
                Decision::Rename(other) => target = other,
                Decision::Overwrite => {
                    if let Err(e) = fs::remove_file(&target) {
                        self.write_error(src, &target, &e);
                        self.skip_tree(src);
                        return Ok(());
                    }
                }
            },
            None => {
                if moving {
                    match fs::rename(src, &target) {
                        Ok(()) => {
                            // the scan counted it: credit the whole tree at once
                            self.p.method = "rename".into();
                            self.credit_tree(&target);
                            return Ok(());
                        }
                        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {}
                        Err(e) => {
                            self.write_error(src, &target, &e);
                            self.skip_tree(src);
                            return Ok(());
                        }
                    }
                }
            }
        }
        let created = fs::symlink_metadata(&target).is_err();
        if created {
            // a private folder (a PC's backup): closed to group and others as its source is
            // while it fills, then no more owner bits than the source either
            let private = self.private(src);
            let made = fs::create_dir(&target).and_then(|()| confine::made_dir(&target)).and_then(|()| {
                if private { File::open(&target).and_then(|d| keep_private(&d, meta.mode(), Some(meta.gid()), 0o700)) } else { Ok(()) }
            });
            if let Err(e) = made {
                self.write_error(src, &target, &e);
                self.skip_tree(src);
                return Ok(());
            }
            self.made_dirs.push(target.clone());
        }
        let mut children: Vec<OsString> = match confine::read_dir(src) {
            Ok(rd) => rd.flatten().map(|e| e.file_name()).collect(),
            Err(e) => {
                self.error(src, &e);
                return Ok(());
            }
        };
        children.sort();
        for child in children {
            self.item(&src.join(&child), &target, &child, moving, false)?;
        }
        if created {
            if self.private(src)
                && let Err(e) = File::open(&target).and_then(|d| keep_private(&d, meta.mode(), Some(meta.gid()), 0))
            {
                self.write_error(src, &target, &e);
            }
            set_times(&target, meta);
        }
        if moving {
            // a skipped or failed child keeps the source folder: that is right
            let _ = fs::remove_dir(src);
        }
        Ok(())
    }

    /// Whether `src` lies in a place whose copies stay as private as it ([`Spec::private_roots`]).
    fn private(&self, src: &Path) -> bool {
        self.spec.private_roots.iter().any(|r| src.starts_with(r))
    }

    fn walk_counts(path: &Path) -> (u64, u64) {
        let (mut files, mut bytes) = (0, 0);
        let _ = walk::<()>(&[path.to_path_buf()], |_, m| {
            if !m.is_dir() {
                files += 1;
                if m.is_file() {
                    bytes += m.len();
                }
            }
            Ok(())
        });
        (files, bytes)
    }

    fn credit_tree(&mut self, path: &Path) {
        let (files, bytes) = Self::walk_counts(path);
        self.p.files_done += files;
        self.p.instant += files;
        self.p.bytes_done += bytes;
        self.unverified(bytes);
    }

    fn skip_tree(&mut self, path: &Path) {
        let (files, bytes) = Self::walk_counts(path);
        self.p.files_done += files;
        self.p.skipped += files;
        self.p.bytes_done += bytes;
        self.unverified(bytes);
    }

    fn decide(&mut self, src: &Path, meta: &Metadata, target: &Path, old: &Metadata) -> Step<Decision> {
        let dir = target.parent().unwrap_or(Path::new("/"));
        let name = target.file_name().unwrap_or_default();
        let policy = match self.apply_all.unwrap_or(self.spec.conflict) {
            Policy::Ask => self.ask(src, meta, target, old)?,
            p => p,
        };
        Ok(match policy {
            Policy::Skip => Decision::Skip,
            Policy::Overwrite => Decision::Overwrite,
            Policy::Rename | Policy::Ask => Decision::Rename(unique_name(dir, name, "")),
            Policy::Newer => {
                if meta.mtime() > old.mtime() {
                    Decision::Overwrite
                } else {
                    Decision::Skip
                }
            }
        })
    }

    /// Asks the user through the daemon, and waits.
    fn ask(&mut self, src: &Path, meta: &Metadata, target: &Path, old: &Metadata) -> Step<Policy> {
        self.conflicts += 1;
        let id = self.conflicts;
        let dir = target.parent().unwrap_or(Path::new("/"));
        let name = target.file_name().unwrap_or_default();
        let info = ConflictInfo {
            id,
            name: name.to_string_lossy().into_owned(),
            path: self.display(src),
            source: Side { size: meta.len(), mtime: meta.mtime(), dir: meta.is_dir() },
            target: Side { size: old.len(), mtime: old.mtime(), dir: old.is_dir() },
            rename_to: unique_name(dir, name, "").file_name().unwrap_or_default().to_string_lossy().into_owned(),
        };
        self.ask_info(info)
    }

    /// Sends a conflict to the daemon and waits for the answer.
    fn ask_info(&mut self, info: ConflictInfo) -> Step<Policy> {
        let id = info.id;
        self.emit_progress(true);
        emit(&Event::Conflict(info));
        let mut s = self.shared.state.lock().unwrap();
        loop {
            if s.cancel {
                return Err(Cancelled);
            }
            if let Some((answer_id, action, all)) = s.answer.take()
                && answer_id == id {
                    drop(s);
                    if all {
                        self.apply_all = Some(action);
                    }
                    // the time spent waiting is not transfer time
                    self.window = (Instant::now(), 0);
                    return Ok(if action == Policy::Ask { Policy::Rename } else { action });
                }
            s = self.shared.wake.wait_timeout(s, Duration::from_millis(500)).unwrap().0;
        }
    }

    fn file(&mut self, src: &Path, meta: &Metadata, target: &Path) -> io::Result<()> {
        let dir = target.parent().unwrap_or(Path::new("/"));
        let part = dir.join(part_name(target.file_name().unwrap_or_default()));
        let input = confine::open(src)?;
        let _ = fs::remove_file(&part);
        let output = OpenOptions::new().write(true).create_new(true).mode(0o666).open(&part)?;
        confine::check_made(&output, &part)?;
        // out of a private place (a PC's backup): before a byte is written, no more mode bits
        // than the source has, but the owner's reading and writing while it fills and is read
        // back (a verified copy of a 0000 file), narrowed fully once it is whole
        let private = self.private(src);
        if private {
            keep_private(&output, meta.mode(), Some(meta.gid()), 0o600)?;
        }
        let result = self.fill(&input, &output, meta.len(), &part);
        let result = result.and_then(|()| {
            if private {
                keep_private(&output, meta.mode(), Some(meta.gid()), 0)?;
            }
            set_file_times(&output, meta);
            drop(output);
            fs::rename(&part, target)
        });
        if result.is_err() {
            let _ = fs::remove_file(&part);
        }
        result
    }

    fn fill(&mut self, input: &File, output: &File, size: u64, part: &Path) -> io::Result<()> {
        if self.clone_works != Some(false) && !self.spec.verify {
            // SAFETY: two open descriptors
            let rc = unsafe { libc::ioctl(output.as_raw_fd(), FICLONE as _, input.as_raw_fd()) };
            if rc == 0 {
                self.clone_works = Some(true);
                self.p.method = "clone".into();
                self.p.instant += 1;
                self.advance(size);
                return Ok(());
            }
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EXDEV) | Some(libc::EOPNOTSUPP) | Some(libc::EINVAL) | Some(libc::ENOTTY) | Some(libc::EPERM) => {
                    // other volume, or a filesystem without reflinks: stream the bytes
                    if self.clone_works.is_none() {
                        self.clone_works = Some(false);
                    }
                }
                _ => return Err(e),
            }
        }
        // SAFETY: an open descriptor; advice only
        unsafe { libc::posix_fadvise(input.as_raw_fd(), 0, 0, libc::POSIX_FADV_SEQUENTIAL) };
        self.p.method = "copy".into();
        if self.spec.verify {
            return self.fill_verified(input, output, size, part);
        }
        let mut left = size;
        let mut behind = WriteBehind::new();
        while left > 0 {
            let want = left.min(CHUNK as u64) as usize;
            // SAFETY: two open descriptors, offset taken from the input
            let n = unsafe { libc::sendfile(output.as_raw_fd(), input.as_raw_fd(), std::ptr::null_mut(), want) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if matches!(e.raw_os_error(), Some(libc::EINVAL) | Some(libc::ENOSYS)) && left == size {
                    return self.fill_plain(input, output, size);
                }
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if n == 0 {
                // the file shrank while we copied it
                break;
            }
            left -= n as u64;
            behind.wrote(output, n as usize)?;
            self.advance(n as u64);
            if self.checkpoint().is_err() {
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
        }
        behind.finish(output)
    }

    fn fill_plain(&mut self, mut input: &File, mut output: &File, _size: u64) -> io::Result<()> {
        if self.buf.len() != CHUNK {
            self.buf = vec![0; CHUNK];
        }
        let mut behind = WriteBehind::new();
        loop {
            let n = input.read(&mut self.buf)?;
            if n == 0 {
                return behind.finish(output);
            }
            output.write_all(&self.buf[..n])?;
            behind.wrote(output, n)?;
            self.advance(n as u64);
            if self.checkpoint().is_err() {
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
        }
    }

    /// Copies while hashing, then reads the copy back and compares.
    fn fill_verified(&mut self, mut input: &File, mut output: &File, size: u64, part: &Path) -> io::Result<()> {
        if self.buf.len() != CHUNK {
            self.buf = vec![0; CHUNK];
        }
        let mut hash = xxhash_rust::xxh3::Xxh3::new();
        let mut behind = WriteBehind::new();
        loop {
            let n = input.read(&mut self.buf)?;
            if n == 0 {
                break;
            }
            hash.update(&self.buf[..n]);
            output.write_all(&self.buf[..n])?;
            behind.wrote(output, n)?;
            self.advance(n as u64);
            if self.checkpoint().is_err() {
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
        }
        output.sync_data()?;
        // SAFETY: drop the copy from the page cache, so the check reads the disk
        unsafe { libc::posix_fadvise(output.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        let phase = std::mem::replace(&mut self.p.phase, "verify".into());
        self.p.current_done = 0;
        self.p.current_size = size;
        let mut check = xxhash_rust::xxh3::Xxh3::new();
        let mut back = File::open(part)?;
        loop {
            let n = back.read(&mut self.buf)?;
            if n == 0 {
                break;
            }
            check.update(&self.buf[..n]);
            self.p.verify_bytes += n as u64;
            self.p.current_done += n as u64;
            if self.checkpoint().is_err() {
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
        }
        self.p.phase = phase;
        if check.digest128() != hash.digest128() {
            return Err(io::Error::other(t!("verification failed: the copy differs from the original")));
        }
        Ok(())
    }

    // ---- archives ----

    fn compress(&mut self) -> Step<()> {
        let Some(dest) = self.spec.dest.clone() else {
            self.fail(c::INVALID_VALUE, t!("no destination"));
            return Ok(());
        };
        let sources = self.spec.sources.clone();
        let tar = self.spec.format.as_deref() == Some("tar.zst");
        let level = self.spec.level.clone().unwrap_or_else(|| "normal".into());
        let name = archive_name(self.spec.name.as_deref(), tar);
        if !self.can_write(&dest) {
            return Ok(());
        }
        self.scan(&sources)?;
        if !self.room_for(&[], &dest) {
            return Ok(());
        }
        self.p.phase = "compress".into();
        let zip_level = crate::zip::Level::parse(&level);
        self.p.method = if tar {
            "zstd"
        } else if zip_level == crate::zip::Level::Store {
            "store"
        } else {
            "deflate"
        }
        .into();
        let mut target = dest.join(&name);
        if fs::symlink_metadata(&target).is_ok() {
            target = unique_name(&dest, OsStr::new(&name), "");
        }
        let part = dest.join(part_name(target.file_name().unwrap_or_default()));
        // an archive of private files (out of a PC's backup) is the owner's alone
        let private = sources.iter().any(|s| self.private(s));
        let file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o666)
            .open(&part)
            .and_then(|f| confine::check_made(&f, &part).map(|()| f))
            .and_then(|f| if private { keep_private(&f, 0o600, None, 0).map(|()| f) } else { Ok(f) })
        {
            Ok(f) => f,
            Err(e) => {
                self.fail(crate::fsops::io_code(&e), describe(&e));
                return Ok(());
            }
        };
        let out = io::BufWriter::with_capacity(1 << 20, file);
        let result = if tar { self.write_tar_zstd(out, &sources, zstd_level(&level), &target) } else { self.write_zip(out, &sources, zip_level, &target) };
        match result {
            Ok(()) => {
                if let Err(e) = fs::rename(&part, &target) {
                    let _ = fs::remove_file(&part);
                    self.fail(crate::fsops::io_code(&e), describe(&e));
                }
                self.p.current.clear();
                Ok(())
            }
            Err(e) => {
                let _ = fs::remove_file(&part);
                if e.kind() == io::ErrorKind::Interrupted || self.shared.state.lock().unwrap().cancel {
                    return Err(Cancelled);
                }
                self.fail(crate::fsops::io_code(&e), describe(&e));
                Ok(())
            }
        }
    }

    fn write_zip(&mut self, out: io::BufWriter<File>, sources: &[PathBuf], level: crate::zip::Level, target: &Path) -> io::Result<()> {
        let mut zip = crate::zip::ZipWriter::new(out);
        self.zip_sources(&mut zip, sources, level, target)?;
        zip.finish()?.into_inner().map_err(|e| e.into_error())?;
        Ok(())
    }

    /// A tar stream through zstd, both in this process: the same walk, progress, pause,
    /// cancel and speed limit as the zip.
    fn write_tar_zstd(&mut self, out: io::BufWriter<File>, sources: &[PathBuf], level: i32, target: &Path) -> io::Result<()> {
        let mut enc = zstd::stream::write::Encoder::new(out, level)?;
        // A libzstd built without threads refuses the workers: it compresses in this thread.
        if enc.multithread(zstd_workers()).is_ok() {
            // jobs of 8 MiB: half the memory of the default at level 9 (some 110 MB with
            // three workers instead of 175), for 0.2% of size
            let _ = enc.set_parameter(zstd::zstd_safe::CParameter::JobSize(8 << 20));
        }
        enc.include_checksum(true)?;
        let mut tar = tar::Builder::new(enc);
        self.tar_sources(&mut tar, sources, target)?;
        tar.into_inner()?.finish()?.into_inner().map_err(|e| e.into_error())?;
        Ok(())
    }

    /// Like `zip_sources`: symbolic links are stored as links and never followed (out of
    /// the share or anywhere else), long names go as GNU long-name entries, every entry
    /// keeps its permissions, owner ids and modification time.
    fn tar_sources<W: Write>(&mut self, tar: &mut tar::Builder<W>, sources: &[PathBuf], target: &Path) -> io::Result<()> {
        let mut stack: Vec<PathBuf> = sources.iter().rev().cloned().collect();
        while let Some(path) = stack.pop() {
            if path == target {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(&path) else { continue };
            let rel = path.strip_prefix(&self.base).unwrap_or(&path).to_path_buf();
            self.p.current = self.display(&path);
            self.p.current_size = if meta.is_file() { meta.len() } else { 0 };
            self.p.current_done = 0;
            let mut header = tar::Header::new_gnu();
            header.set_mode(meta.mode() & 0o7777);
            header.set_mtime(meta.mtime().max(0) as u64);
            header.set_uid(u64::from(meta.uid()));
            header.set_gid(u64::from(meta.gid()));
            header.set_size(0);
            if meta.is_dir() {
                header.set_entry_type(tar::EntryType::Directory);
                tar.append_data(&mut header, &rel, io::empty())?;
                let mut children: Vec<PathBuf> = match confine::read_dir(&path) {
                    Ok(rd) => rd
                        .flatten()
                        .filter(|e| !crate::fsops::is_system_name(&e.file_name().to_string_lossy()))
                        .map(|e| e.path())
                        .collect(),
                    Err(e) => {
                        self.error(&path, &e);
                        continue;
                    }
                };
                children.sort();
                stack.extend(children.into_iter().rev());
            } else if meta.is_file() {
                let mut file = match confine::open(&path) {
                    Ok(f) => f,
                    Err(e) => {
                        self.error(&path, &e);
                        self.skip(&meta);
                        continue;
                    }
                };
                // SAFETY: an open descriptor; advice only
                unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_SEQUENTIAL) };
                header.set_entry_type(tar::EntryType::Regular);
                header.set_size(meta.len());
                let short = {
                    let progress = |n: u64| -> io::Result<()> {
                        self.advance(n);
                        self.checkpoint().map_err(|_| io::Error::other(t!("cancelled")))
                    };
                    let mut data = Exact { inner: &mut file, left: meta.len(), short: false, progress };
                    tar.append_data(&mut header, &rel, &mut data)?;
                    data.short
                };
                if short {
                    self.error_msg(&path, t!("the file got shorter while it was archived: the rest is zeros"));
                }
                self.p.files_done += 1;
            } else if meta.file_type().is_symlink() {
                let link = match fs::read_link(&path) {
                    Ok(l) => l,
                    Err(e) => {
                        self.error(&path, &e);
                        self.p.files_done += 1;
                        continue;
                    }
                };
                header.set_entry_type(tar::EntryType::Symlink);
                tar.append_link(&mut header, &rel, &link)?;
                self.p.files_done += 1;
            } else {
                // sockets, pipes, devices: not data, left out as in the zip
                self.p.files_done += 1;
            }
        }
        Ok(())
    }

    fn zip_sources<W: Write>(&mut self, zip: &mut crate::zip::ZipWriter<W>, sources: &[PathBuf], level: crate::zip::Level, target: &Path) -> io::Result<()> {
        let mut stack: Vec<PathBuf> = sources.iter().rev().cloned().collect();
        while let Some(path) = stack.pop() {
            if path == target {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(&path) else { continue };
            let name = self.display(&path);
            self.p.current = name.clone();
            self.p.current_size = if meta.is_file() { meta.len() } else { 0 };
            self.p.current_done = 0;
            let mode = meta.mode();
            if meta.is_dir() {
                zip.add_dir(&name, meta.mtime(), mode)?;
                let mut children: Vec<PathBuf> = match confine::read_dir(&path) {
                    Ok(rd) => rd
                        .flatten()
                        .filter(|e| !crate::fsops::is_system_name(&e.file_name().to_string_lossy()))
                        .map(|e| e.path())
                        .collect(),
                    Err(e) => {
                        self.error(&path, &e);
                        continue;
                    }
                };
                children.sort();
                stack.extend(children.into_iter().rev());
            } else if meta.is_file() {
                let mut file = match confine::open(&path) {
                    Ok(f) => f,
                    Err(e) => {
                        self.error(&path, &e);
                        self.skip(&meta);
                        continue;
                    }
                };
                // SAFETY: an open descriptor; advice only
                unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_SEQUENTIAL) };
                let mut progress = |n: u64| -> io::Result<()> {
                    self.advance(n);
                    self.checkpoint().map_err(|_| io::Error::from(io::ErrorKind::Interrupted))
                };
                zip.add_file(&name, meta.mtime(), mode, &mut file, level, &mut progress)?;
                self.p.files_done += 1;
            } else if meta.file_type().is_symlink() {
                let link = fs::read_link(&path).map(|l| l.to_string_lossy().into_owned()).unwrap_or_default();
                zip.add_symlink(&name, meta.mtime(), &link)?;
                self.p.files_done += 1;
            } else {
                self.p.files_done += 1;
            }
        }
        Ok(())
    }

    fn extract(&mut self) -> Step<()> {
        let (Some(archive), Some(dest)) = (self.spec.sources.first().cloned(), self.spec.dest.clone()) else {
            self.fail(c::INVALID_VALUE, t!("no destination"));
            return Ok(());
        };
        if !self.can_write(&dest) {
            return Ok(());
        }
        let size = fs::metadata(&archive).map(|m| m.len()).unwrap_or(0);
        self.p.phase = "extract".into();
        self.p.bytes_total = size;
        self.p.files_total = 0;
        let name = archive.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let format = crate::archive::format(&name);
        // GNU tar would only say that it could not run the decompressor
        if let Some(program) = crate::archive::missing(format.unwrap_or(crate::archive::Format::SevenZip)) {
            self.fail(c::TOOL_MISSING, crate::archive::tool_missing(program).message);
            return Ok(());
        }
        let tool = Tool::of(format);
        self.p.method = match tool {
            Tool::Unrar => "unrar",
            Tool::Tar(_) => "tar",
            Tool::SevenZip => "7z",
        }
        .into();
        let work = match Unpack::prepare(&dest, &name, self.spec.into_folder) {
            Ok(work) => work,
            Err(e) => {
                self.fail(crate::fsops::io_code(&e), describe(&e));
                return Ok(());
            }
        };
        let list = match self.extract_list(&archive, &work) {
            Ok(list) => list,
            Err(msg) => {
                work.discard();
                self.fail(c::INTERNAL, msg);
                return Ok(());
            }
        };
        let password = self.spec.password.clone().unwrap_or_default();
        let mut cmd = tool_command(tool, &archive, &work.tmp, list.as_deref(), password.is_empty());
        // the tool writes below its folder only, whatever links the archive holds
        let rules = super::landlock::writes_below(&work.tmp);
        if let Some(rules) = &rules {
            rules.apply(&mut cmd);
        }
        let (code, out) = match self.run_tool(cmd, tool, &archive, size, &password) {
            Ok(Ok(done)) => done,
            Ok(Err(msg)) => {
                work.discard();
                self.fail(c::INTERNAL, msg);
                return Ok(());
            }
            Err(Cancelled) => {
                work.discard();
                return Err(Cancelled);
            }
        };
        self.p.files_done = out.files;
        self.p.files_total = self.p.files_total.max(out.files);
        self.p.current.clear();
        if out.password && code != 0 {
            work.discard();
            self.fail(c::PASSWORD_REQUIRED, rynas_files::jobs::protocol::NEED_PASSWORD.into());
            return Ok(());
        }
        // 7z: 1 = warnings (some files), 2+ = fatal; unrar and tar: 0 ok
        if code == 0 || (tool == Tool::SevenZip && code == 1) {
            if code == 1 {
                self.error_msg(&archive, out.tail.last().cloned().unwrap_or_default());
            }
            self.place_extracted(&work);
            work.tidy();
            self.p.bytes_done = self.p.bytes_total;
            return Ok(());
        }
        let why = out.tail.iter().rev().find(|l| l.to_ascii_lowercase().contains("error")).or(out.tail.last()).cloned().unwrap_or_default();
        self.fail(c::INTERNAL, t!("extraction failed: {}", why));
        // an empty folder is no use to anybody
        work.discard();
        Ok(())
    }

    /// Only some items: the tools get the list of their names in a file,
    /// and the totals become theirs.
    fn extract_list(&mut self, archive: &Path, work: &Unpack) -> Result<Option<PathBuf>, String> {
        if self.spec.members.is_empty() {
            return Ok(None);
        }
        let all = crate::archive::members(archive).map_err(|e| e.message)?;
        let wanted: Vec<&crate::archive::Member> = all
            .iter()
            .filter(|m| self.spec.members.iter().any(|sel| m.path == *sel || (m.path.starts_with(sel.as_str()) && m.path.as_bytes().get(sel.len()) == Some(&b'/'))))
            .collect();
        self.p.bytes_total = wanted.iter().map(|m| m.size).sum();
        self.p.files_total = wanted.iter().filter(|m| !m.dir).count() as u64;
        let file = work.list_file();
        let text: String = wanted.iter().map(|m| format!("{}\n", m.raw)).collect();
        // a name planted beside the folder (a link) is never written through
        let _ = fs::remove_file(&file);
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&file)
            .and_then(|mut f| f.write_all(text.as_bytes()))
            .map_err(|e| describe(&e))?;
        Ok(Some(file))
    }

    /// Runs the extraction tool to the end, following its output for the
    /// progress and honouring pause and cancel. The exit code and what the
    /// tool said, or why it could not start.
    fn run_tool(&mut self, mut cmd: std::process::Command, tool: Tool, archive: &Path, size: u64, password: &str) -> Step<Result<(i32, ToolOutput), String>> {
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        use std::sync::atomic::{AtomicU64, Ordering};
        // p7zip reads UTF-8 names right only in a UTF-8 locale
        cmd.env("LC_ALL", "en_US.utf8").env("LANG", "en_US.utf8").env("PATH", "/usr/bin:/bin:/usr/local/bin");
        let tar = matches!(tool, Tool::Tar(_));
        let password_on_stdin = !tar && !password.is_empty();
        cmd.stdin(if tar || password_on_stdin { Stdio::piped() } else { Stdio::null() });
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        // Without a controlling terminal unrar's getpass() reads the
        // password from stdin instead of a tty.
        // SAFETY: setsid is async-signal-safe; a fresh child is never a group leader
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return Ok(Err(t!("extraction tool not available: {}", describe(&e)))),
        };
        if password_on_stdin
            && let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(format!("{password}\n").as_bytes());
            }
        // tar is fed through stdin, so the bytes read give the progress
        let fed = Arc::new(AtomicU64::new(0));
        // opened here, where the jail is (the feeding thread has none)
        let opened = confine::open(archive);
        let feeder = child.stdin.take().map(|mut stdin| {
            let fed = Arc::clone(&fed);
            std::thread::spawn(move || {
                let Ok(mut f) = opened else { return };
                let mut buf = vec![0u8; 1 << 20];
                while let Ok(n) = f.read(&mut buf) {
                    if n == 0 || stdin.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    fed.fetch_add(n as u64, Ordering::Relaxed);
                }
            })
        });
        let seen = Arc::new(Mutex::new(ToolOutput::default()));
        let mut readers = Vec::new();
        for stream in [child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>), child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>)].into_iter().flatten() {
            let seen = Arc::clone(&seen);
            readers.push(std::thread::spawn(move || {
                let mut stream = stream;
                let mut line = Vec::new();
                let mut byte = [0u8; 4096];
                let take = |line: &mut Vec<u8>| {
                    let text = String::from_utf8_lossy(line).trim().to_string();
                    line.clear();
                    if !text.is_empty() {
                        seen.lock().unwrap().take(&text, tool);
                    }
                };
                while let Ok(n) = stream.read(&mut byte) {
                    if n == 0 {
                        break;
                    }
                    for &b in &byte[..n] {
                        // the tools redraw their progress with \r and backspaces
                        if b == b'\n' || b == b'\r' || b == 8 {
                            take(&mut line);
                        } else {
                            line.push(b);
                        }
                    }
                }
                take(&mut line);
            }));
        }
        let pid = child.id() as libc::pid_t;
        let mut stopped = false;
        let status = loop {
            if let Ok(Some(status)) = child.try_wait() {
                break Some(status);
            }
            let (paused, cancel) = {
                let s = self.shared.state.lock().unwrap();
                (s.paused, s.cancel)
            };
            if cancel {
                // SAFETY: signalling our own child
                unsafe { libc::kill(pid, libc::SIGKILL) };
                let _ = child.wait();
                return Err(Cancelled);
            }
            if paused != stopped {
                // SAFETY: signalling our own child
                unsafe { libc::kill(pid, if paused { libc::SIGSTOP } else { libc::SIGCONT }) };
                stopped = paused;
                self.p.paused = paused;
                self.emit_progress(true);
            }
            {
                let s = seen.lock().unwrap();
                self.p.bytes_done = if tar { fed.load(Ordering::Relaxed).min(size) * self.p.bytes_total.max(1) / size.max(1) } else { self.p.bytes_total * s.percent / 100 };
                self.p.files_done = s.files;
                self.p.current = s.last.clone();
            }
            self.emit_progress(false);
            std::thread::sleep(Duration::from_millis(150));
        };
        for r in readers {
            let _ = r.join();
        }
        if let Some(f) = feeder {
            let _ = f.join();
        }
        let out = std::mem::take(&mut *seen.lock().unwrap());
        Ok(Ok((status.and_then(|st| st.code()).unwrap_or(-1), out)))
    }

    /// Into place under the conflict rule: the chosen items, or all that
    /// came out.
    fn place_extracted(&mut self, work: &Unpack) {
        let policy = self.spec.conflict;
        // in the apps' data: what the tool made (as the user) gets the folder's owner
        if let Err(e) = confine::adopt_tree(&work.tmp, &work.target) {
            self.error_msg(&work.target, describe(&e));
        }
        let picked = !self.spec.members.is_empty();
        let moves: Vec<(PathBuf, PathBuf)> = if picked {
            self.spec.members.iter().map(|m| (work.tmp.join(m), work.target.join(m.rsplit('/').next().unwrap_or(m)))).collect()
        } else {
            confine::read_dir(&work.tmp).map(|d| d.flatten().map(|e| (e.path(), work.target.join(e.file_name()))).collect()).unwrap_or_default()
        };
        for (from, to) in moves {
            // a folder merged into may lead elsewhere: never into a PC's backup
            if let Err(e) = confine::refuse_pc_write(to.parent().unwrap_or(&work.target), false) {
                self.error_msg(&to, describe(&e));
                continue;
            }
            if fs::symlink_metadata(&from).is_err() {
                // the tool said fine, yet the item is not there
                if picked {
                    self.error_msg(&to, t!("not extracted from the archive"));
                }
                continue;
            }
            if let Err(e) = merge(&from, &to, policy) {
                self.error_msg(&to, describe(&e));
            }
        }
    }

    // ---- delete ----

    fn delete(&mut self) -> Step<()> {
        let sources = self.spec.sources.clone();
        let recycle = self.spec.recycle.clone();
        let mut permanent = Vec::new();
        self.p.phase = "delete".into();
        self.p.files_total = sources.len() as u64;
        for src in sources {
            if self.snapshot_refused(&src) {
                continue;
            }
            // nothing in or above a PC's backup folder goes, whatever path names it
            if let Err(e) = confine::refuse_pc_write(&src, true) {
                self.error(&src, &e);
                self.p.files_done += 1;
                continue;
            }
            // no shares at all: the user asked to delete for good
            let bin = if recycle.is_empty() { Bin::Permanent } else { recycle_target(&recycle, &src) };
            match bin {
                Bin::Recycle(root, rel) => {
                    self.p.method = "recycle".into();
                    self.p.current = self.display(&src);
                    self.checkpoint()?;
                    if let Err(e) = to_recycle(&root, &rel, &src) {
                        self.error_msg(&src, e);
                    }
                    self.p.files_done += 1;
                }
                Bin::Missing => {
                    // the bin was asked for: deleting for good instead is not for us to decide
                    self.error_msg(
                        &src,
                        t!("the recycle bin cannot take it (reached through a symbolic link out of the shared folder?): it was not deleted"
                        ),
                    );
                    self.p.files_done += 1;
                }
                Bin::Permanent => {
                    // counted again below, with everything inside
                    self.p.files_total -= 1;
                    permanent.push(src);
                }
            }
        }
        if permanent.is_empty() {
            return Ok(());
        }
        self.p.method = "delete".into();
        // count first, so the bar means something
        self.p.scanning = true;
        let mut total = 0u64;
        walk(&permanent, |_, _| {
            total += 1;
            if total.is_multiple_of(512) {
                self.p.files_total = self.p.files_done + total;
                self.checkpoint()?;
            }
            Ok(())
        })?;
        self.p.scanning = false;
        self.p.files_total = self.p.files_done + total;
        for src in permanent {
            self.remove_tree(&src)?;
        }
        Ok(())
    }

    fn remove_tree(&mut self, root: &Path) -> Step<()> {
        // (path, children already removed)
        let mut stack = vec![(root.to_path_buf(), false)];
        while let Some((path, expanded)) = stack.pop() {
            self.checkpoint()?;
            let Ok(meta) = fs::symlink_metadata(&path) else {
                self.p.files_done += 1;
                continue;
            };
            if meta.is_dir() && !expanded {
                stack.push((path.clone(), true));
                match confine::read_dir(&path) {
                    Ok(rd) => stack.extend(rd.flatten().map(|e| (e.path(), false))),
                    Err(e) => self.error(&path, &e),
                }
                continue;
            }
            self.p.current = self.display(&path);
            let result = if meta.is_dir() { fs::remove_dir(&path) } else { fs::remove_file(&path) };
            if let Err(e) = result {
                // a folder that kept a failed child says so once, for the child
                if !(meta.is_dir() && e.raw_os_error() == Some(libc::ENOTEMPTY)) {
                    self.error(&path, &e);
                }
            }
            self.p.files_done += 1;
            if meta.is_file() {
                self.p.bytes_done += meta.len();
            }
        }
        Ok(())
    }
}

enum Decision {
    Skip,
    Overwrite,
    Rename(PathBuf),
}

/// Where a deleted item goes when the recycle bin was asked for.
#[derive(Debug, PartialEq)]
enum Bin {
    /// the share whose bin takes it, and its path inside the share
    Recycle(RecycleRoot, PathBuf),
    /// already in a recycle bin, or in a thumbnail folder (`@eaDir`): deleted
    /// for good
    Permanent,
    /// under none of the shares: left alone, never deleted for good
    Missing,
}

/// The program that unpacks an archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tool {
    SevenZip,
    Unrar,
    /// with the flag GNU tar needs for the compression
    Tar(Option<&'static str>),
}

impl Tool {
    fn of(format: Option<crate::archive::Format>) -> Self {
        match format {
            Some(crate::archive::Format::Rar) => Tool::Unrar,
            Some(crate::archive::Format::Tar(flag)) => Tool::Tar(flag),
            _ => Tool::SevenZip,
        }
    }
}

/// The command that unpacks `archive` into the folder `into`. A password
/// never goes on the command line, where `ps` shows it to everybody: 7z
/// and unrar read it from stdin (`run_tool` writes it there).
fn tool_command(tool: Tool, archive: &Path, into: &Path, list: Option<&Path>, no_password: bool) -> std::process::Command {
    match tool {
        Tool::Unrar => {
            let mut c = std::process::Command::new("unrar");
            // -y answers "use current password?" for every file after the first
            c.args(["x", "-o+", "-y"]);
            if no_password {
                // fail at once instead of asking
                c.arg("-p-");
            }
            // a list after "--" would be taken for a file name
            if let Some(l) = list {
                c.arg(OsString::from_vec([b"-n@".as_slice(), l.as_os_str().as_bytes()].concat()));
            }
            c.arg("--").arg(archive).arg(OsString::from_vec([into.as_os_str().as_bytes(), b"/"].concat()));
            c
        }
        Tool::Tar(flag) => {
            // the archive comes on stdin
            let mut c = std::process::Command::new("tar");
            c.args(["-x", "-v", "--no-same-owner", "--no-same-permissions"]);
            if let Some(flag) = flag {
                c.arg(flag);
            }
            c.arg("-f").arg("-").arg("-C").arg(into);
            if let Some(l) = list {
                c.args(["--no-recursion", "--verbatim-files-from", "-T"]).arg(l);
            }
            c
        }
        Tool::SevenZip => {
            let mut c = std::process::Command::new("7z");
            c.args(["x", "-y", "-bsp1", "-bb1", "-bse1", "-aoa", "-spd", "-scsUTF-8"]);
            if no_password {
                // an empty -p makes 7z fail at once instead of asking
                c.arg("-p");
            }
            if let Some(l) = list {
                c.arg(OsString::from_vec([b"-i@".as_slice(), l.as_os_str().as_bytes()].concat()));
            }
            c.arg(OsString::from_vec([b"-o".as_slice(), into.as_os_str().as_bytes()].concat())).arg("--").arg(archive);
            c
        }
    }
}

/// What an extraction tool said, as far as the progress cares.
#[derive(Debug, Default)]
struct ToolOutput {
    percent: u64,
    files: u64,
    /// the item in hand
    last: String,
    /// the last lines, for the error message
    tail: Vec<String>,
    /// it wants a (better) password
    password: bool,
}

impl ToolOutput {
    fn take(&mut self, text: &str, tool: Tool) {
        let low = text.to_ascii_lowercase();
        if ["wrong password", "incorrect password", "encrypted archive", "password is incorrect", "enter password"].iter().any(|w| low.contains(w)) {
            self.password = true;
        }
        // 7z: " 45% 12 - dir/file"; unrar: "Extracting  dir/file   45%"
        if let Some(pct) = text.split_whitespace().find_map(|w| w.strip_suffix('%').and_then(|n| n.parse::<u64>().ok())) {
            self.percent = self.percent.max(pct.min(100));
        }
        let file = match tool {
            // tar -v prints just the names
            Tool::Tar(_) => Some(text.to_string()).filter(|f| !f.ends_with('/')),
            // only its "Extracting" lines name files: "b.txt - use current
            // password ?" is a question
            Tool::Unrar => text
                .strip_prefix("Extracting")
                .map(|r| r.trim().trim_end_matches(|c: char| c.is_ascii_digit() || c == '%' || c == ' ' || c == '\u{8}').trim().to_string()),
            Tool::SevenZip => text.strip_prefix("- ").map(str::to_string).or_else(|| text.split_once(" - ").map(|(_, f)| f.to_string())),
        };
        if let Some(f) = file.filter(|f| !f.is_empty() && !f.starts_with("from "))
            && f != self.last {
                self.files += 1;
                self.last = f;
            }
        self.tail.push(text.to_string());
        if self.tail.len() > 12 {
            self.tail.remove(0);
        }
    }
}

/// Where an extraction happens: everything lands in a hidden folder inside
/// the target first, then moves into place under the conflict rule. The
/// same for every tool, and it lets a few items come out on their own.
struct Unpack {
    target: PathBuf,
    /// the folder made for the archive ("into a new folder")
    created: Option<PathBuf>,
    tmp: PathBuf,
}

impl Unpack {
    fn prepare(dest: &Path, name: &str, into_folder: bool) -> io::Result<Self> {
        let mut created = None;
        let target = if into_folder {
            let dir = unique_name(dest, OsStr::new(archive_stem(name)), "");
            fs::create_dir(&dir)?;
            confine::made_dir(&dir)?;
            created = Some(dir.clone());
            dir
        } else {
            dest.to_path_buf()
        };
        let tmp = target.join(format!(".rynas-x-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        if let Err(e) = fs::create_dir(&tmp) {
            if let Some(dir) = &created {
                let _ = fs::remove_dir(dir);
            }
            return Err(e);
        }
        Ok(Self { target, created, tmp })
    }

    fn list_file(&self) -> PathBuf {
        self.tmp.with_extension("list")
    }

    /// The hidden folder and the list go.
    fn tidy(&self) {
        let _ = fs::remove_dir_all(&self.tmp);
        let _ = fs::remove_file(self.list_file());
    }

    /// Nothing of the extraction stays, the folder made for it included.
    fn discard(&self) {
        self.tidy();
        if let Some(dir) = &self.created {
            let _ = fs::remove_dir_all(dir);
        }
    }
}


/// Whether a name is a copy's temporary file (".name.rynas-part"), SynoFile's names too: a
/// volume may come from a NAS that ran it.
fn part_leftover(b: &[u8]) -> bool {
    b.starts_with(b".")
        && [PART_TAG, LEGACY_PART_TAG]
            .iter()
            .any(|tag| b.strip_suffix(tag.as_bytes()).is_some_and(|rest| rest.len() > 1 && rest.ends_with(b".")))
}

/// Whether a cleanup removes this item: a job's temporary file or
/// extraction folder, or an upload's temporary file nobody wrote to for a
/// day. `meta` is asked for only when the age matters.
fn leftover(name: &OsStr, kind: &fs::FileType, meta: impl FnOnce() -> Option<Metadata>, now: std::time::SystemTime) -> bool {
    let b = name.as_bytes();
    if !b.starts_with(b".") {
        return false;
    }
    if part_leftover(b) || b.starts_with(b".rynas-x-") || b.starts_with(b".synofile-x-") {
        return true;
    }
    kind.is_file()
        && (b.ends_with(b".rynas-upload") || b.ends_with(b".synofile-upload"))
        && meta().is_some_and(|m| crate::names::abandoned(&m, now))
}

/// Every item of the trees under `roots`, folders included and symlinks
/// not followed, in no set order. `visit` may stop the walk.
fn walk<E>(roots: &[PathBuf], mut visit: impl FnMut(&Path, &Metadata) -> Result<(), E>) -> Result<(), E> {
    let mut stack = roots.to_vec();
    while let Some(path) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&path) else { continue };
        visit(&path, &meta)?;
        if meta.is_dir()
            && let Ok(rd) = confine::read_dir(&path) {
                stack.extend(rd.flatten().map(|e| e.path()));
            }
    }
    Ok(())
}

/// The share whose recycle bin takes `src`, and the path inside it.
fn recycle_target(roots: &[RecycleRoot], src: &Path) -> Bin {
    let (Some(parent), Some(name)) = (src.parent(), src.file_name()) else { return Bin::Missing };
    // a personal home is reached through a symlink: compare real paths first
    if let Ok(real_parent) = fs::canonicalize(parent) {
        let real = real_parent.join(name);
        for root in roots {
            let base = fs::canonicalize(&root.path).unwrap_or_else(|_| root.path.clone());
            if let Some(bin) = locate(root, &real, &base) {
                return bin;
            }
        }
    }
    // reached through a symlink that leaves the share: the path as given
    // still names the share, and the bin can take the item as long as it
    // lives on the share's own file system (a rename, nothing copied)
    let item_dev = fs::symlink_metadata(src).ok().map(|m| m.dev());
    for root in roots {
        let same_fs = item_dev.is_some() && fs::metadata(&root.path).ok().map(|m| m.dev()) == item_dev;
        if let Some(bin) = locate(root, src, &root.path).filter(|_| same_fs) {
            return bin;
        }
    }
    Bin::Missing
}

/// Where `path` falls under `base`, the folder of `root`.
fn locate(root: &RecycleRoot, path: &Path, base: &Path) -> Option<Bin> {
    let rel = path.strip_prefix(base).ok()?;
    if rel.as_os_str().is_empty() {
        return None;
    }
    let inside_bin = rel.components().any(|c| c.as_os_str() == crate::shares::RECYCLE || c.as_os_str() == "@eaDir");
    Some(if inside_bin { Bin::Permanent } else { Bin::Recycle(root.clone(), rel.to_path_buf()) })
}

/// Moves `src` into the share's recycle bin with the layout Samba's `vfs_recycle` uses as
/// rynas configures it (`recycle:repository = #recycle`, `keeptree`, `versions`, `touch`):
/// `<share>/#recycle/<folders of the item>/<name>`, and when that name is taken
/// `Copy #1 of <name>`, `Copy #2 of <name>`… Folders made on the way get mode 0770 (minus
/// the share's default ACL, as Samba's `directory_mode`). The access time becomes the
/// deletion time (`touch`); the move itself sets the ctime the cleanup task
/// (`smb.recycle-cleanup`) reads. A rename only: an item on another filesystem (a mount
/// inside the share) is refused, never copied nor deleted for good. Runs as the user.
pub fn to_recycle(root: &RecycleRoot, rel: &Path, src: &Path) -> Result<(), String> {
    let bin = root.path.join(crate::shares::RECYCLE);
    let target = bin.join(rel);
    let parent = target.parent().unwrap_or(&bin).to_path_buf();
    make_bin_dirs(&root.path, parent.strip_prefix(&root.path).unwrap_or(Path::new(crate::shares::RECYCLE))).map_err(|e| describe(&e))?;
    let name = target.file_name().unwrap_or_default().to_os_string();
    let mut candidate = target;
    let mut n = 1u32;
    loop {
        match rename_noreplace(src, &candidate) {
            Ok(()) => break,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists || e.raw_os_error() == Some(libc::EEXIST) || e.raw_os_error() == Some(libc::ENOTEMPTY) => {
                let mut versioned = OsString::from(format!("Copy #{n} of "));
                versioned.push(&name);
                candidate = parent.join(versioned);
                n += 1;
                if n > 10_000 {
                    return Err(describe(&e));
                }
            }
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                return Err(t!("the recycle bin is on another filesystem: it was not deleted"));
            }
            Err(e) => return Err(describe(&e)),
        }
    }
    // touch: the access time is when it went to the bin (the modification time stays)
    if let Ok(c) = CString::new(candidate.as_os_str().as_bytes()) {
        let times = [libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_NOW }, libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT }];
        // SAFETY: a valid C string and two timespecs
        unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
    }
    Ok(())
}

/// The folders of the recycle bin down to an item's: made one at a time, each a real folder
/// (a link planted as `#recycle` or below it would carry the deleted items out of the share).
fn make_bin_dirs(share: &Path, rel: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut at = share.to_path_buf();
    for part in rel.components() {
        at.push(part);
        match fs::DirBuilder::new().mode(0o770).create(&at) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        if !fs::symlink_metadata(&at)?.is_dir() {
            return Err(confine::outside());
        }
    }
    Ok(())
}

/// The file name of a new archive: the user's, with the format's extension added when
/// it does not end with one already.
fn archive_name(name: Option<&str>, tar_zstd: bool) -> String {
    let mut name = name.map(str::to_string).unwrap_or_else(|| t!("Archive"));
    let lower = name.to_ascii_lowercase();
    if tar_zstd {
        if !crate::archive::TAR_ZSTD.iter().any(|ext| lower.ends_with(ext)) {
            name.push_str(crate::archive::TAR_ZSTD[0]);
        }
    } else if !lower.ends_with(".zip") {
        name.push_str(".zip");
    }
    name
}

/// The zstd level for the dialog's choice. Measured with three workers: level 3 (zstd's
/// own default) goes faster than a hard disk writes even on a 4-core Celeron; 9 makes
/// archives some 9% smaller at a fifth of the speed; above it the gain is a few percent
/// more for many times the time (19 is some 85 times slower than 3): not offered. The
/// zip's "store" has no place in a .tar.zst; it gets the normal level.
fn zstd_level(level: &str) -> i32 {
    match level {
        "fast" => 1,
        "best" => 9,
        _ => 3,
    }
}

/// zstd's worker threads: every core but one, which stays for the file services and
/// the web interface (three on a 4-core NAS), at most four (some 36 MB each).
fn zstd_workers() -> u32 {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    cores.saturating_sub(1).clamp(1, 4) as u32
}

/// A file's data exactly as long as its tar header says: zeros after the end
/// when the file got shorter meanwhile (`short`), nothing past it when it
/// grew, so the archive stays readable. `progress` hears of every chunk and
/// may stop the archive with an error.
struct Exact<R, F> {
    inner: R,
    left: u64,
    short: bool,
    progress: F,
}

impl<R: Read, F: FnMut(u64) -> io::Result<()>> Read for Exact<R, F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 || buf.is_empty() {
            return Ok(0);
        }
        let want = buf.len().min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let mut n = if self.short { 0 } else { self.inner.read(&mut buf[..want])? };
        if n == 0 {
            self.short = true;
            buf[..want].fill(0);
            n = want;
        }
        self.left -= n as u64;
        (self.progress)(n as u64)?;
        Ok(n)
    }
}

/// "Foto.tar.gz" → "Foto", "dati.zip" → "dati"
pub fn archive_stem(name: &str) -> &str {
    let lower = name.to_ascii_lowercase();
    for ext in [".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst", ".tar.zstd", ".tar.lz", ".tgz", ".tbz2", ".tbz", ".txz", ".tzst", ".tzstd"] {
        if lower.ends_with(ext) && name.len() > ext.len() {
            return &name[..name.len() - ext.len()];
        }
    }
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

/// Hidden name a copy is written under before the rename into place.
fn part_name(name: &OsStr) -> OsString {
    crate::names::part_os(name, PART_TAG)
}

/// "name.ext" if free, else "name (1).ext", "name (2).ext"... With a `tag`,
/// "name - Copia.ext", then "name - Copia (1).ext".
/// Moves an extracted item into place: folders are merged, a clash is
/// settled by the conflict rule (asking is not possible here: "ask" keeps
/// both, like the NAS does).
fn merge(from: &Path, to: &Path, policy: Policy) -> io::Result<()> {
    let Ok(old) = fs::symlink_metadata(to) else { return fs::rename(from, to) };
    let new = fs::symlink_metadata(from)?;
    if old.is_dir() && new.is_dir() {
        // an existing folder merged into may be a mount of somewhere else
        confine::refuse_pc_write(to, false)?;
        for item in confine::read_dir(from)?.flatten() {
            merge(&item.path(), &to.join(item.file_name()), policy)?;
        }
        return Ok(());
    }
    let replace = !old.is_dir()
        && !new.is_dir()
        && match policy {
            Policy::Overwrite => true,
            Policy::Newer => new.mtime() > old.mtime(),
            _ => false,
        };
    if replace {
        return fs::rename(from, to);
    }
    if matches!(policy, Policy::Skip | Policy::Newer | Policy::Overwrite) {
        return Ok(());
    }
    let parent = to.parent().unwrap_or(Path::new("."));
    fs::rename(from, unique_name(parent, to.file_name().unwrap_or_default(), ""))
}

pub fn unique_name(dir: &Path, name: &OsStr, tag: &str) -> PathBuf {
    let text = name.to_string_lossy();
    // "a.tar.zst" → "a (1).tar.zst": a compressed tar's extension is one
    let stem = archive_stem(&text);
    let ext = &text[stem.len()..];
    // the plain name first, when it is free
    let candidate = dir.join(format!("{stem}{tag}{ext}"));
    if fs::symlink_metadata(&candidate).is_err() {
        return candidate;
    }
    (1..)
        .map(|n| dir.join(format!("{stem}{tag} ({n}){ext}")))
        .find(|p| fs::symlink_metadata(p).is_err())
        .unwrap()
}

fn timespec(sec: i64, nsec: i64) -> libc::timespec {
    libc::timespec { tv_sec: sec as _, tv_nsec: nsec as _ }
}

fn set_file_times(file: &File, meta: &Metadata) {
    let times = [timespec(meta.atime(), meta.atime_nsec()), timespec(meta.mtime(), meta.mtime_nsec())];
    // SAFETY: an open descriptor and two timespecs
    unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) };
}

fn set_times(path: &Path, meta: &Metadata) {
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else { return };
    let times = [timespec(meta.atime(), meta.atime_nsec()), timespec(meta.mtime(), meta.mtime_nsec())];
    // SAFETY: a valid C string and two timespecs
    unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
}

/// Narrows what was just made (`made`, open) to no more mode bits than its source has
/// (`source_mode`, in group `source_gid`), `keep` bits aside (the owner's, while a folder
/// fills or a file is verified): whatever the umask or a default ACL of the destination
/// gave, a copy is never more open than its original. Owner and others keep at most the
/// source's bits; the group the source's group bits when the copy has the source's group,
/// else the source's "other" bits (0644 root:root stays 0644, 0640 root:shadow becomes
/// 0600). With an ACL the group bits are its mask, so named entries narrow too. Copies to a server (SFTP, SMB) take the server's
/// modes: SMB has none, and the file manager's SFTP client sets none.
fn keep_private(made: &File, source_mode: u32, source_gid: Option<u32>, keep: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let m = made.metadata()?;
    let now = m.mode() & 0o7777;
    // a copy in another group than its source's: that group's members get what anybody
    // could do with the source (its "other" bits), never what its own group could
    let group = match source_gid {
        Some(gid) if gid != m.gid() => (source_mode & 0o007) << 3,
        _ => source_mode & 0o070,
    };
    let allowed = (source_mode & 0o700) | group | (source_mode & 0o007);
    // a setgid the folder inherited from its parent stays (new files keep the parent's group)
    let mode = (now & (allowed | keep) & 0o777) | (now & 0o2000);
    if mode != now {
        made.set_permissions(fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// The mount a path lives on: (mount point, filesystem type), longest match.
fn mount_of(mounts: &str, path: &Path) -> Option<(PathBuf, String)> {
    mounts
        .lines()
        .filter_map(|line| {
            let mut f = line.split(' ');
            let (_, point, kind) = (f.next()?, f.next()?, f.next()?);
            let point = PathBuf::from(point.replace("\\040", " "));
            path.starts_with(&point).then(|| (point, kind.to_string()))
        })
        .max_by_key(|(p, _)| p.as_os_str().len())
}

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[u]).replace('.', ",") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_names() {
        let dir = std::env::temp_dir().join(format!("rynas-files-unique-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.txt"), "").unwrap();
        assert_eq!(unique_name(&dir, OsStr::new("a.txt"), ""), dir.join("a (1).txt"));
        fs::write(dir.join("a (1).txt"), "").unwrap();
        assert_eq!(unique_name(&dir, OsStr::new("a.txt"), ""), dir.join("a (2).txt"));
        assert_eq!(unique_name(&dir, OsStr::new("a.txt"), " - Copia"), dir.join("a - Copia.txt"));
        assert_eq!(unique_name(&dir, OsStr::new(".bashrc"), ""), dir.join(".bashrc"));
        fs::write(dir.join(".bashrc"), "").unwrap();
        assert_eq!(unique_name(&dir, OsStr::new(".bashrc"), ""), dir.join(".bashrc (1)"));
        fs::write(dir.join("b.tar.zst"), "").unwrap();
        assert_eq!(unique_name(&dir, OsStr::new("b.tar.zst"), ""), dir.join("b (1).tar.zst"));
        assert_eq!(unique_name(&dir, OsStr::new("b.tar.zst"), " - Copia"), dir.join("b - Copia.tar.zst"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn part_names_stay_short() {
        let long = "è".repeat(120);
        let part = part_name(OsStr::new(&long));
        assert!(part.len() <= 255);
        assert!(part.to_str().is_some());
        assert_eq!(part_name(OsStr::new("a.txt")), OsString::from(".a.txt.rynas-part"));
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rynas-files-worker-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn delete_spec(sources: Vec<PathBuf>, recycle: Vec<RecycleRoot>) -> Spec {
        serde_json::from_value(serde_json::json!({ "op": "delete", "sources": sources, "recycle": recycle })).unwrap()
    }

    fn engine(spec: Spec) -> Engine {
        Engine::new(spec, Arc::new(Shared::new(0)))
    }

    #[test]
    fn a_refused_write_names_the_destination() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("denied");
        let (src, dest) = (dir.join("src"), dir.join("sistema"));
        fs::create_dir_all(src.join("a")).unwrap();
        fs::write(src.join("a/f.txt"), b"x").unwrap();
        fs::create_dir_all(dest.join("a")).unwrap();
        let spec: Spec = serde_json::from_value(serde_json::json!({ "op": "copy", "sources": [src.join("a")], "dest": dest })).unwrap();
        let mut e = engine(spec);
        assert_eq!(e.dest_display(&dest), "sistema");
        assert_eq!(e.dest_display(&dest.join("a/f.txt")), "sistema/a/f.txt");
        assert_eq!(e.dest_display(Path::new("/elsewhere/x")), "/elsewhere/x");
        // a folder below the destination the user may not write to (root writes anyway)
        if unsafe { libc::geteuid() } != 0 {
            fs::set_permissions(dest.join("a"), fs::Permissions::from_mode(0o500)).unwrap();
            let denied = io::Error::from_raw_os_error(libc::EACCES);
            e.write_error(&src.join("a/f.txt"), &dest.join("a/f.txt"), &denied);
            assert_eq!(e.p.errors, 1);
            fs::set_permissions(dest.join("a"), fs::Permissions::from_mode(0o755)).unwrap();
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn copies_out_of_a_private_place_stay_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("private");
        let backup = dir.join("pc-backups/laptop");
        let src = backup.join("@.20261113T1543/etc");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("shadow"), b"root:secret").unwrap();
        fs::write(src.join("hostname"), b"laptop").unwrap();
        fs::set_permissions(src.join("shadow"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(src.join("hostname"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o750)).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().mode() & 0o777;
        let copy = |dest: &Path, private: bool| {
            fs::create_dir_all(dest).unwrap();
            let roots: Vec<PathBuf> = if private { vec![backup.clone()] } else { Vec::new() };
            let spec: Spec = serde_json::from_value(serde_json::json!({ "op": "copy", "sources": [src], "dest": dest, "private_roots": roots })).unwrap();
            let mut e = engine(spec);
            assert!(e.run().is_ok());
            assert_eq!(e.p.errors, 0);
        };
        let out = dir.join("out");
        copy(&out, true);
        // never more bits than the source: the secret stays the owner's, the folder closed to others
        assert_eq!(mode(&out.join("etc/shadow")) & !0o600, 0);
        assert_eq!(mode(&out.join("etc/hostname")) & !0o644, 0);
        assert_eq!(mode(&out.join("etc")) & !0o750, 0);
        assert_eq!(fs::read(out.join("etc/shadow")).unwrap(), b"root:secret");
        // an ordinary copy takes the destination's usual modes (the umask), as before
        let plain = dir.join("plain");
        copy(&plain, false);
        let umask = {
            // SAFETY: read and put back at once
            let m = unsafe { libc::umask(0o022) };
            unsafe { libc::umask(m) };
            m
        };
        assert_eq!(mode(&plain.join("etc/shadow")), 0o666 & !umask);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn private_modes_follow_the_group_the_owner_and_setgid() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("narrow");
        let mode = |p: &Path| fs::metadata(p).unwrap().mode() & 0o7777;
        let made = |name: &str, m: u32| {
            let p = dir.join(name);
            fs::write(&p, b"x").unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(m)).unwrap();
            p
        };
        let gid = |p: &Path| fs::metadata(p).unwrap().gid();
        // same group as the source: its group bits stay
        let p = made("same", 0o664);
        keep_private(&File::open(&p).unwrap(), 0o640, Some(gid(&p)), 0).unwrap();
        assert_eq!(mode(&p), 0o640);
        // another group: its bits are what anybody could do with the source
        let other = |name: &str, source: u32| {
            let p = made(name, 0o664);
            keep_private(&File::open(&p).unwrap(), source, Some(gid(&p).wrapping_add(1)), 0).unwrap();
            mode(&p)
        };
        // /etc/hostname 0644 root:root: readable by the share's users as before
        assert_eq!(other("hostname", 0o644), 0o644);
        // /etc/shadow-like 0640 root:shadow, a 0600 key: the owner's only
        assert_eq!(other("shadow", 0o640), 0o600);
        assert_eq!(other("key", 0o600), 0o600);
        // never more than what would be made (0664 here, the umask's)
        assert_eq!(other("open", 0o777), 0o664);
        // a source nobody may read (0200, read by the administrator's powers): the copy keeps
        // the owner's reading and writing while it is verified, then the source's bits
        let p = made("verified", 0o644);
        keep_private(&File::open(&p).unwrap(), 0o200, Some(gid(&p)), 0o600).unwrap();
        assert_eq!(mode(&p), 0o600);
        assert_eq!(fs::read(&p).unwrap(), b"x");
        keep_private(&File::open(&p).unwrap(), 0o200, Some(gid(&p)), 0).unwrap();
        assert_eq!(mode(&p), 0o200);
        // a folder's setgid (from its parent) stays
        let d = dir.join("sgid");
        fs::create_dir(&d).unwrap();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o2775)).unwrap();
        if mode(&d) & 0o2000 != 0 {
            keep_private(&File::open(&d).unwrap(), 0o750, Some(gid(&d)), 0).unwrap();
            assert_eq!(mode(&d), 0o2750);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn jobs_never_write_in_a_pc_folder() {
        use std::os::unix::fs::MetadataExt;
        let dir = scratch("guard");
        let src = dir.join("src/photos");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.jpg"), b"jpg").unwrap();
        let dest = dir.join("share");
        // dest/photos exists: the copy merges into it; it stands for a mount of a PC's folder
        // (its identity is a guarded one, its path is not)
        fs::create_dir_all(dest.join("photos")).unwrap();
        let m = fs::metadata(dest.join("photos")).unwrap();
        let pc = confine::PcRoot { path: PathBuf::from("/elsewhere/laptop"), dev: m.dev(), ino: m.ino() };
        let copy = |dest: &Path, op: &str| {
            confine::set_guard(Some(vec![pc.clone()]));
            let spec: Spec = serde_json::from_value(serde_json::json!({ "op": op, "sources": [src], "dest": dest })).unwrap();
            let mut e = engine(spec);
            let _ = e.run();
            confine::set_guard(None);
            e
        };
        let e = copy(&dest, "copy");
        assert!(e.p.errors > 0, "the merge into the guarded folder is refused");
        assert!(!dest.join("photos/a.jpg").exists());
        // a destination that is a link to it: the whole job is refused
        std::os::unix::fs::symlink(dest.join("photos"), dir.join("lnk")).unwrap();
        let e = copy(&dir.join("lnk"), "copy");
        assert_eq!(e.fatal.as_ref().map(|f| f.0), Some(c::READ_ONLY));
        // a deletion of it, or of the folder that holds it
        let spec: Spec = serde_json::from_value(serde_json::json!({ "op": "delete", "sources": [dest] })).unwrap();
        confine::set_guard(Some(vec![confine::PcRoot::of(&dest.join("photos")).unwrap()]));
        let mut e = engine(spec);
        let _ = e.run();
        confine::set_guard(None);
        assert!(dest.join("photos").is_dir() && e.p.errors > 0);
        // nor moved away by a rename (the fast move inside one volume), itself or its holder
        let other = dir.join("other");
        fs::create_dir_all(&other).unwrap();
        for src in [dest.join("photos"), dest.clone()] {
            let spec: Spec = serde_json::from_value(serde_json::json!({ "op": "move", "sources": [src], "dest": other })).unwrap();
            confine::set_guard(Some(vec![confine::PcRoot::of(&dest.join("photos")).unwrap()]));
            let mut e = engine(spec);
            let _ = e.run();
            confine::set_guard(None);
            assert!(e.p.errors > 0, "{}", src.display());
            assert!(dest.join("photos").is_dir() && fs::read_dir(&other).unwrap().next().is_none());
        }
        // elsewhere, the copy is a copy
        let e = copy(&other, "copy");
        assert_eq!(e.p.errors, 0);
        assert!(other.join("photos/a.jpg").is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_the_recycle_bin() {
        let dir = scratch("bin");
        let share = dir.join("share");
        fs::create_dir_all(share.join("docs")).unwrap();
        fs::create_dir_all(share.join(crate::shares::RECYCLE)).unwrap();
        fs::create_dir_all(dir.join("elsewhere")).unwrap();
        fs::create_dir_all(share.join("docs/@eaDir")).unwrap();
        for f in ["docs/a.txt", "docs/@eaDir/x", "#recycle/old.txt"] {
            fs::write(share.join(f), "").unwrap();
        }
        fs::write(dir.join("elsewhere/b.txt"), "").unwrap();
        // a folder of the share that is a symlink out of it
        std::os::unix::fs::symlink(dir.join("elsewhere"), share.join("link")).unwrap();
        let root = RecycleRoot { name: "share".into(), path: share.clone() };
        let roots = [root.clone()];
        assert_eq!(recycle_target(&roots, &share.join("docs/a.txt")), Bin::Recycle(root.clone(), PathBuf::from("docs/a.txt")));
        // already in the bin: for good
        assert_eq!(recycle_target(&roots, &share.join("#recycle/old.txt")), Bin::Permanent);
        assert_eq!(recycle_target(&roots, &share.join("docs/@eaDir/x")), Bin::Permanent);
        // through the symlink: the share's bin, by the path as given (same file system here)
        assert_eq!(recycle_target(&roots, &share.join("link/b.txt")), Bin::Recycle(root.clone(), PathBuf::from("link/b.txt")));
        // under no share at all
        assert_eq!(recycle_target(&roots, &dir.join("elsewhere/b.txt")), Bin::Missing);
        assert_eq!(recycle_target(&roots, &share), Bin::Missing);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn recycle_asked_but_no_bin_deletes_nothing() {
        let dir = scratch("nobin");
        let share = dir.join("share");
        fs::create_dir_all(&share).unwrap();
        let outside = dir.join("outside");
        fs::create_dir_all(outside.join("sub")).unwrap();
        fs::write(outside.join("sub/f.txt"), "keep me").unwrap();
        let mut e = engine(delete_spec(vec![outside.clone()], vec![RecycleRoot { name: "share".into(), path: share }]));
        assert!(e.delete().is_ok());
        assert_eq!(e.p.errors, 1);
        assert_eq!(fs::read_to_string(outside.join("sub/f.txt")).unwrap(), "keep me");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn deletes_for_good_when_asked() {
        let dir = scratch("perm");
        let victim = dir.join("victim");
        fs::create_dir_all(victim.join("a/b")).unwrap();
        fs::write(victim.join("a/b/f.txt"), "x").unwrap();
        fs::write(victim.join("g.txt"), "y").unwrap();
        let mut e = engine(delete_spec(vec![victim.clone()], Vec::new()));
        assert!(e.delete().is_ok());
        assert_eq!(e.p.errors, 0);
        assert!(fs::symlink_metadata(&victim).is_err());
        // victim, a, a/b, a/b/f.txt, g.txt
        assert_eq!((e.p.files_done, e.p.files_total), (5, 5));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn recycles_with_the_samba_layout() {
        let dir = scratch("samba-bin");
        let share = dir.join("share");
        fs::create_dir_all(share.join("docs/sub")).unwrap();
        fs::write(share.join("docs/sub/a.txt"), "1").unwrap();
        let root = RecycleRoot { name: "share".into(), path: share.clone() };
        let mut e = engine(delete_spec(vec![share.join("docs/sub/a.txt")], vec![root.clone()]));
        assert!(e.delete().is_ok());
        assert_eq!(e.p.errors, 0);
        assert_eq!(fs::read_to_string(share.join("#recycle/docs/sub/a.txt")).unwrap(), "1");
        // the same name again: "Copy #1 of", then "Copy #2 of", as vfs_recycle versions
        for n in ["2", "3"] {
            fs::write(share.join("docs/sub/a.txt"), n).unwrap();
            let mut e = engine(delete_spec(vec![share.join("docs/sub/a.txt")], vec![root.clone()]));
            assert!(e.delete().is_ok());
        }
        assert_eq!(fs::read_to_string(share.join("#recycle/docs/sub/Copy #1 of a.txt")).unwrap(), "2");
        assert_eq!(fs::read_to_string(share.join("#recycle/docs/sub/Copy #2 of a.txt")).unwrap(), "3");
        // a whole folder, kept as a tree
        fs::write(share.join("docs/sub/b.txt"), "b").unwrap();
        let mut e = engine(delete_spec(vec![share.join("docs")], vec![root]));
        assert!(e.delete().is_ok());
        assert!(!share.join("docs").exists());
        assert_eq!(fs::read_to_string(share.join("#recycle/Copy #1 of docs/sub/b.txt")).unwrap(), "b");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn walks_whole_trees() {
        let dir = scratch("walk");
        fs::create_dir_all(dir.join("t/a")).unwrap();
        fs::write(dir.join("t/a/f"), "1234").unwrap();
        fs::write(dir.join("t/g"), "12").unwrap();
        std::os::unix::fs::symlink("/", dir.join("t/root")).unwrap();
        assert_eq!(Engine::walk_counts(&dir.join("t")), (3, 6));
        let mut seen = 0;
        let stopped = walk(&[dir.join("t")], |_, _| {
            seen += 1;
            if seen == 2 { Err("stop") } else { Ok(()) }
        });
        assert_eq!((stopped, seen), (Err("stop"), 2));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn knows_leftovers() {
        let dir = scratch("left");
        let kind = |name: &str| {
            let p = dir.join(name);
            fs::write(&p, "").unwrap();
            fs::symlink_metadata(&p).unwrap().file_type()
        };
        let now = std::time::SystemTime::now();
        let later = now + Duration::from_secs(25 * 3600);
        let fresh = || fs::metadata(&dir).ok();
        let file = kind("f");
        assert!(leftover(OsStr::new(".a.txt.rynas-part"), &file, fresh, now));
        assert!(leftover(OsStr::new(".rynas-x-123"), &file, fresh, now));
        assert!(!leftover(OsStr::new("a.rynas-part"), &file, fresh, now));
        assert!(!leftover(OsStr::new(".rynas-part"), &file, fresh, now));
        assert!(!leftover(OsStr::new("notes.txt"), &file, fresh, now));
        // uploads: only once abandoned for a day
        assert!(leftover(OsStr::new(".a.txt.k3j4h5g6.rynas-upload"), &file, fresh, later));
        assert!(!leftover(OsStr::new(".a.txt.k3j4h5g6.rynas-upload"), &file, fresh, now));
        let folder = fs::metadata(&dir).unwrap().file_type();
        assert!(!leftover(OsStr::new(".a.txt.k3j4h5g6.rynas-upload"), &folder, fresh, later));
        // what SynoFile left on a volume that comes from a NAS that ran it
        assert!(leftover(OsStr::new(".a.txt.synofile-part"), &file, fresh, now));
        assert!(leftover(OsStr::new(".synofile-x-9"), &file, fresh, now));
        assert!(leftover(OsStr::new(".a.k3.synofile-upload"), &file, fresh, later));
        assert!(!leftover(OsStr::new(".a.k3.synofile-upload"), &file, fresh, now));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn passwords_stay_off_the_command_line() {
        let args = |c: &std::process::Command| c.get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>();
        let (archive, into, list) = (Path::new("/v/a b.rar"), Path::new("/v/.rynas-x-1"), Path::new("/v/.rynas-x-1.list"));
        let unrar = tool_command(Tool::Unrar, archive, into, Some(list), false);
        assert_eq!(unrar.get_program(), "unrar");
        assert_eq!(args(&unrar), ["x", "-o+", "-y", "-n@/v/.rynas-x-1.list", "--", "/v/a b.rar", "/v/.rynas-x-1/"]);
        // no password: fail at once instead of asking
        assert!(args(&tool_command(Tool::Unrar, archive, into, None, true)).contains(&"-p-".to_string()));
        let seven = tool_command(Tool::SevenZip, Path::new("/v/a.zip"), into, None, false);
        assert!(args(&seven).iter().all(|a| !a.starts_with("-p")));
        assert!(args(&tool_command(Tool::SevenZip, Path::new("/v/a.zip"), into, None, true)).contains(&"-p".to_string()));
        let tar = tool_command(Tool::Tar(Some("-z")), Path::new("/v/a.tgz"), into, None, true);
        assert_eq!(args(&tar), ["-x", "-v", "--no-same-owner", "--no-same-permissions", "-z", "-f", "-", "-C", "/v/.rynas-x-1"]);
        assert_eq!(Tool::of(crate::archive::format("x.RAR")), Tool::Unrar);
        assert_eq!(Tool::of(crate::archive::format("x.tar.xz")), Tool::Tar(Some("-J")));
        assert_eq!(Tool::of(None), Tool::SevenZip);
    }

    #[test]
    fn follows_the_tools_output() {
        let mut out = ToolOutput::default();
        // unrar, as split on \r and backspaces; its questions are not files
        for line in ["Extracting from /v/a.rar", "Extracting  /v/x/a.txt                 6%", "OK", "b.txt - use current password ?", "Extracting  /v/x/b.txt      21%", "All OK"] {
            out.take(line, Tool::Unrar);
        }
        assert_eq!((out.files, out.last.as_str(), out.percent, out.password), (2, "/v/x/b.txt", 21, false));
        out.take("The specified password is incorrect.", Tool::Unrar);
        assert!(out.password);
        let mut out = ToolOutput::default();
        for line in [" 45% 12 - dir/file.txt", "- other.txt"] {
            out.take(line, Tool::SevenZip);
        }
        assert_eq!((out.files, out.last.as_str(), out.percent), (2, "other.txt", 45));
        let mut out = ToolOutput::default();
        for line in ["dir/", "dir/f"] {
            out.take(line, Tool::Tar(None));
        }
        assert_eq!(out.files, 1);
        for n in 0..20 {
            out.take(&format!("line {n}"), Tool::Tar(None));
        }
        assert_eq!(out.tail.len(), 12);
    }

    #[test]
    fn finds_mounts() {
        let mounts = "/dev/a / ext4 rw 0 0\n/dev/b /volume1 btrfs rw 0 0\n/dev/c /volume1/x/#snapshot btrfs ro 0 0\n";
        assert_eq!(mount_of(mounts, Path::new("/volume1/Documenti")).unwrap().0, PathBuf::from("/volume1"));
        assert_eq!(mount_of(mounts, Path::new("/volume10")).unwrap().0, PathBuf::from("/"));
    }

    #[test]
    fn archive_names() {
        assert_eq!(archive_name(Some("Foto"), true), "Foto.tar.zst");
        assert_eq!(archive_name(Some("Foto.TZST"), true), "Foto.TZST");
        assert_eq!(archive_name(Some("Foto.tar.zstd"), true), "Foto.tar.zstd");
        assert_eq!(archive_name(Some("Foto.zip"), true), "Foto.zip.tar.zst");
        assert_eq!(archive_name(Some("Foto"), false), "Foto.zip");
        assert_eq!(archive_name(None, false), "Archive.zip");
        assert_eq!(archive_name(None, true), "Archive.tar.zst");
        for (name, stem) in [("a.tar.zst", "a"), ("a.tar.zstd", "a"), ("a.tzst", "a"), ("a.TZSTD", "a"), ("a.zst", "a")] {
            assert_eq!(archive_stem(name), stem, "{name}");
        }
        assert_eq!((zstd_level("fast"), zstd_level("normal"), zstd_level("best"), zstd_level("store")), (1, 3, 9, 3));
        assert!((1..=4).contains(&zstd_workers()));
    }

    #[test]
    fn exact_length_whatever_the_file_does() {
        let mut seen = 0;
        let mut out = Vec::new();
        let mut short = Exact { inner: &b"abc"[..], left: 5, short: false, progress: |n| { seen += n; Ok(()) } };
        io::copy(&mut short, &mut out).unwrap();
        assert!(short.short);
        assert_eq!(out, b"abc\0\0");
        assert_eq!(seen, 5);
        let mut long = Exact { inner: &b"abcdef"[..], left: 4, short: false, progress: |_| Ok(()) };
        out.clear();
        io::copy(&mut long, &mut out).unwrap();
        assert!(!long.short);
        assert_eq!(out, b"abcd");
    }

    fn compress_spec(sources: Vec<PathBuf>, dest: &Path, name: &str) -> Spec {
        serde_json::from_value(serde_json::json!({ "op": "compress", "sources": sources, "dest": dest, "name": name, "format": "tar.zst", "level": "fast" })).unwrap()
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.contains(PART_TAG)).collect()
    }

    #[test]
    fn tar_zstd_round_trip() {
        use std::os::unix::fs::PermissionsExt;
        if crate::archive::missing(crate::archive::Format::Tar(Some("--zstd"))).is_some() {
            eprintln!("GNU tar or zstd missing: round trip skipped");
            return;
        }
        let dir = scratch("tzst");
        let src = dir.join("Progetto");
        let long_dir = "cartella con un nome davvero molto lungo per superare i cento caratteri di ustar";
        let long_file = format!("{}.txt", "è".repeat(60));
        fs::create_dir_all(src.join("vuota")).unwrap();
        fs::create_dir_all(src.join(long_dir)).unwrap();
        fs::create_dir_all(dir.join("fuori")).unwrap();
        fs::write(dir.join("fuori/segreto.txt"), "not to be archived").unwrap();
        fs::write(src.join("a.txt"), "ciao zstd\n".repeat(1000)).unwrap();
        fs::set_permissions(src.join("a.txt"), fs::Permissions::from_mode(0o640)).unwrap();
        let when = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        File::options().write(true).open(src.join("a.txt")).unwrap().set_modified(when).unwrap();
        fs::write(src.join(long_dir).join(&long_file), "lungo").unwrap();
        std::os::unix::fs::symlink("../fuori/segreto.txt", src.join("fuori")).unwrap();
        std::os::unix::fs::symlink("a.txt", src.join("collegamento")).unwrap();

        let mut e = engine(compress_spec(vec![src.clone()], &dir, "Progetto"));
        assert!(e.compress().is_ok());
        assert!(e.fatal.is_none(), "{:?}", e.fatal);
        assert_eq!(e.p.errors, 0);
        let archive = dir.join("Progetto.tar.zst");
        assert!(archive.is_file());
        assert!(leftovers(&dir).is_empty());

        // listed as GNU tar reads it
        let members = crate::archive::members(&archive).unwrap();
        let find = |p: &str| members.iter().find(|m| m.path == p).unwrap_or_else(|| panic!("{p} not in {members:?}"));
        assert!(find("Progetto/vuota").dir);
        assert!(find("Progetto/fuori").link);
        assert!(find("Progetto/collegamento").link);
        let a = find("Progetto/a.txt");
        assert_eq!((a.size, a.mode, a.mtime), (10_000, 0o640, 1_700_000_000));
        assert_eq!(find(&format!("Progetto/{long_dir}/{long_file}")).size, 5);
        assert!(!members.iter().any(|m| m.path.contains("segreto")));

        // and extracted by the job
        let out = dir.join("estratti");
        fs::create_dir_all(&out).unwrap();
        let spec: Spec = serde_json::from_value(serde_json::json!({ "op": "extract", "sources": [archive], "dest": out })).unwrap();
        let mut x = engine(spec);
        assert!(x.extract().is_ok());
        assert!(x.fatal.is_none(), "{:?}", x.fatal);
        let back = out.join("Progetto");
        assert_eq!(fs::read_to_string(back.join("a.txt")).unwrap(), "ciao zstd\n".repeat(1000));
        let meta = fs::metadata(back.join("a.txt")).unwrap();
        assert_eq!((meta.mode() & 0o777, meta.mtime()), (0o640, 1_700_000_000));
        assert_eq!(fs::read_to_string(back.join(long_dir).join(&long_file)).unwrap(), "lungo");
        assert!(back.join("vuota").is_dir());
        assert_eq!(fs::read_link(back.join("fuori")).unwrap(), PathBuf::from("../fuori/segreto.txt"));
        assert_eq!(fs::read_link(back.join("collegamento")).unwrap(), PathBuf::from("a.txt"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tar_zstd_cancel_leaves_nothing() {
        let dir = scratch("tzst-cancel");
        let src = dir.join("dati");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("grande.bin"), vec![7u8; 4 << 20]).unwrap();
        // slow enough (1 KB/s) to be cancelled while the file is being written
        let mut e = engine(compress_spec(vec![src], &dir, "dati"));
        e.shared.state.lock().unwrap().limit = 1024;
        let shared = Arc::clone(&e.shared);
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            shared.state.lock().unwrap().cancel = true;
            shared.wake.notify_all();
        });
        assert!(e.compress().is_err());
        canceller.join().unwrap();
        assert!(e.p.bytes_done > 0 && e.p.bytes_done < 4 << 20, "cancelled mid-file: {}", e.p.bytes_done);
        assert!(leftovers(&dir).is_empty(), "{:?}", leftovers(&dir));
        assert!(!dir.join("dati.tar.zst").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1536), "1,5 KB");
        assert_eq!(human(5 * 1024 * 1024 * 1024), "5,0 GB");
    }
}
