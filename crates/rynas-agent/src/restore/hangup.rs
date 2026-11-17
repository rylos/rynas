//! What a hangup does to a restore: the SSH session or the terminal closes while
//! `rynas-agent restore` runs.
//!
//! Before: nothing handled SIGHUP. The shell resent it to the whole job when the session
//! closed, so the restore and the programs it runs (`sfdisk`, `mkfs`, `btrfs receive`, the
//! boot loader's installer) were killed wherever they were: a disk half partitioned or half
//! filled, no report to the NAS, nothing on screen to say so. And had it survived, its next
//! line to the vanished terminal (`println!` fails with EIO) would have aborted it.
//!
//! Now (the choice: the safest for a disk being written is to finish the work):
//!
//! - **while choosing** (the screens, the questions, the dry run): nothing is written yet, so
//!   SIGHUP keeps its default and the process ends at once; its private mount namespace goes
//!   with it, and the NAS ends the restore session by itself;
//! - **while writing** ([`writing`]: from the first change of the target disk to the end of
//!   the restore): SIGHUP is ignored, by this process and by every program it starts (an
//!   ignored signal stays ignored across `exec`), and a watcher thread sees the terminal go
//!   (`POLLHUP` on standard input): standard output and error then go on into
//!   [`log_path`] (`/var/log/rynas-restore.log`, or `/run`, or `/tmp`) and standard input
//!   reads nothing. The restore runs to its end, reports to the NAS as always (its log, the
//!   outcome), writes its log into the restored system, and never offers to reboot (no one
//!   to answer). Log in again and read the log file, or the PC's page on the NAS.
//!
//! The progress lines and the log lines are written without failing on a closed terminal
//! ([`out`], [`err`]), for the moment between the hangup and the switch to the file.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

static HUNG: AtomicBool = AtomicBool::new(false);

/// Where a restore whose terminal went away writes on (`RYNAS_RESTORE_LOG` for tests).
pub fn log_path() -> PathBuf {
    if let Some(p) = std::env::var_os("RYNAS_RESTORE_LOG") {
        return PathBuf::from(p);
    }
    for dir in ["/var/log", "/run", "/tmp"] {
        let d = std::path::Path::new(dir);
        if d.is_dir() && writable(d) {
            return d.join("rynas-restore.log");
        }
    }
    PathBuf::from("/tmp/rynas-restore.log")
}

fn writable(dir: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else { return false };
    // SAFETY: a plain access(2) on a NUL-terminated path
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

/// The terminal went away during this restore.
pub fn hung() -> bool {
    HUNG.load(Ordering::SeqCst)
}

/// A line on standard output that never fails (a closed terminal).
pub fn out(s: &str) {
    let mut o = std::io::stdout().lock();
    let _ = writeln!(o, "{s}");
    let _ = o.flush();
}

/// A line on standard error that never fails.
pub fn err(s: &str) {
    let _ = writeln!(std::io::stderr().lock(), "{s}");
}

/// Text on standard error without a newline (progress), never failing.
pub fn err_part(s: &str) {
    let mut e = std::io::stderr().lock();
    let _ = write!(e, "{s}");
    let _ = e.flush();
}

/// The writing phase of a restore (see the module): SIGHUP ignored and the terminal watched
/// until this guard is dropped.
pub struct Writing {
    /// Written to stop the watcher.
    stop: Option<std::os::fd::OwnedFd>,
    watcher: Option<std::thread::JoinHandle<()>>,
}

/// Enters the writing phase.
pub fn writing() -> Writing {
    // SAFETY: changing a signal's disposition to "ignore"
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    // SAFETY: isatty on a descriptor number
    if unsafe { libc::isatty(0) } != 1 {
        return Writing { stop: None, watcher: None };
    }
    let mut fds = [0i32; 2];
    // SAFETY: pipe2 fills two descriptors
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Writing { stop: None, watcher: None };
    }
    use std::os::fd::FromRawFd;
    // SAFETY: both descriptors were just made by pipe2 and are owned here only
    let (r, w) = unsafe { (std::os::fd::OwnedFd::from_raw_fd(fds[0]), std::os::fd::OwnedFd::from_raw_fd(fds[1])) };
    let watcher = std::thread::spawn(move || watch(r));
    Writing { stop: Some(w), watcher: Some(watcher) }
}

/// Waits for the terminal to go (or the stop pipe), then moves the output to the log file.
fn watch(stop: std::os::fd::OwnedFd) {
    loop {
        // events 0 on the terminal: only a hangup or an error wakes this (typing does not)
        let mut p = [
            libc::pollfd { fd: 0, events: 0, revents: 0 },
            libc::pollfd { fd: stop.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: poll on two valid pollfd entries
        let n = unsafe { libc::poll(p.as_mut_ptr(), 2, -1) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if p[1].revents != 0 {
            return;
        }
        if p[0].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            detach();
            return;
        }
    }
}

/// Standard output and error into the log file, standard input from `/dev/null`.
fn detach() {
    use std::os::unix::fs::OpenOptionsExt;
    let path = log_path();
    let Ok(log) = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&path) else { return };
    let null = std::fs::File::open("/dev/null").ok();
    // SAFETY: dup2 of descriptors this process owns onto 0, 1 and 2 (atomic for writers)
    unsafe {
        libc::dup2(log.as_raw_fd(), 1);
        libc::dup2(log.as_raw_fd(), 2);
        if let Some(n) = &null {
            libc::dup2(n.as_raw_fd(), 0);
        }
    }
    HUNG.store(true, Ordering::SeqCst);
    err(&format!(
        "\nrynas-agent restore: the terminal closed at {} (ms since 1970); the restore goes on, its output follows here",
        crate::state::now_ms()
    ));
}

impl Drop for Writing {
    fn drop(&mut self) {
        if let Some(w) = self.stop.take() {
            let _ = std::fs::File::from(w).write_all(b"x");
        }
        if let Some(t) = self.watcher.take() {
            let _ = t.join();
        }
        // idle again: a hangup ends the process
        // SAFETY: back to the default disposition
        unsafe {
            libc::signal(libc::SIGHUP, libc::SIG_DFL);
        }
    }
}

/// Hidden, tests (`hangup-test <done file>`): the writing phase for 3 s with a line every
/// 100 ms and a child that sleeps 2 s; at its end `done <child status> <hung>` in the file.
pub fn test_command(args: &[String]) -> Result<(), String> {
    let done = args.get(2).ok_or("hangup-test <done file>")?;
    let w = writing();
    let child = std::process::Command::new("sleep").arg("2").spawn().map_err(|e| e.to_string())?;
    for i in 0..30 {
        out(&format!("line {i}"));
        err_part(&format!("\r  progress {i}   "));
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let status = child.wait_with_output().map(|o| o.status.success()).unwrap_or(false);
    drop(w);
    out("the end");
    std::fs::write(done, format!("done {status} {}", hung())).map_err(|e| e.to_string())
}
