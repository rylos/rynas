//! Changes in a few folders, told by inotify (the certificate folder, `/run/rynas`): the
//! loops that looked at a file every few seconds wait on this instead, with a slow safety
//! net ([`SAFETY_NET`]) for what inotify cannot see (a folder made after the watch started).
//!
//! One thread per watch, asleep in `read(2)` on the inotify descriptor; a burst of events (two
//! renames of an update) is one wake-up of the receivers (`watch` keeps the last value only).

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::watch;

/// How often a waiter looks anyway while inotify reports nothing (and the pace without
/// inotify is the caller's own old one).
pub const SAFETY_NET: Duration = Duration::from_secs(60);

/// Watches `dirs` (each one that exists now); `None` when inotify is not available or none
/// of them could be watched. The value counts the bursts seen.
pub fn watch_dirs(dirs: &[&Path]) -> Option<watch::Receiver<u64>> {
    // SAFETY: plain syscall, the descriptor is owned by the thread below
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let mut watched: std::collections::HashMap<i32, PathBuf> = std::collections::HashMap::new();
    for d in dirs {
        if let Some(wd) = add_watch(fd, d) {
            watched.insert(wd, d.to_path_buf());
        }
    }
    if watched.is_empty() {
        // SAFETY: our descriptor, not used afterwards
        unsafe { libc::close(fd) };
        return None;
    }
    let names: Vec<PathBuf> = watched.values().cloned().collect();
    let (tx, rx) = watch::channel(0u64);
    let spawned = std::thread::Builder::new().name("fswatch".into()).spawn(move || {
        let mut buf = [0u8; 4096];
        let mut n = 0u64;
        loop {
            // SAFETY: the buffer is ours and large enough for several events
            let r = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if r < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            // a watched folder deleted or moved: its watch is gone; watched again when it is
            // there (made again at once), else this watch ends and the waiters fall back to
            // their timer instead of waiting for good
            for (wd, mask) in events(&buf[..r as usize]) {
                if mask & (libc::IN_IGNORED | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF) != 0
                    && let Some(path) = watched.remove(&wd)
                {
                    // SAFETY: our descriptor; a watch already gone is an error we ignore
                    unsafe { libc::inotify_rm_watch(fd, wd) };
                    if let Some(new) = add_watch(fd, &path) {
                        watched.insert(new, path);
                    }
                }
            }
            if watched.is_empty() {
                break;
            }
            // the rest of a burst (the second rename of an update) joins this wake-up
            std::thread::sleep(Duration::from_millis(200));
            n += 1;
            if tx.send(n).is_err() {
                break;
            }
        }
        // SAFETY: our descriptor
        unsafe { libc::close(fd) };
    });
    if spawned.is_err() {
        return None;
    }
    tracing::debug!("fswatch: watching {names:?}");
    Some(rx)
}

/// Watches the folder `d` on `fd`: its watch descriptor.
fn add_watch(fd: i32, d: &Path) -> Option<i32> {
    let mask = libc::IN_CLOSE_WRITE
        | libc::IN_MOVED_TO
        | libc::IN_MOVED_FROM
        | libc::IN_CREATE
        | libc::IN_DELETE
        | libc::IN_ATTRIB
        | libc::IN_DELETE_SELF
        | libc::IN_MOVE_SELF;
    let c = std::ffi::CString::new(d.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: valid descriptor and C string
    let wd = unsafe { libc::inotify_add_watch(fd, c.as_ptr(), mask) };
    (wd >= 0).then_some(wd)
}

/// `(watch descriptor, mask)` of each `struct inotify_event` in a read.
fn events(buf: &[u8]) -> Vec<(i32, u32)> {
    const HEAD: usize = 16; // wd, mask, cookie, len
    let mut out = Vec::new();
    let mut i = 0;
    while i + HEAD <= buf.len() {
        let wd = i32::from_ne_bytes(buf[i..i + 4].try_into().unwrap_or_default());
        let mask = u32::from_ne_bytes(buf[i + 4..i + 8].try_into().unwrap_or_default());
        let len = u32::from_ne_bytes(buf[i + 12..i + 16].try_into().unwrap_or_default()) as usize;
        out.push((wd, mask));
        i += HEAD + len;
    }
    out
}

/// Waits for the next change in `rx`, or `fallback` when there is no watch (or it ended), or
/// [`SAFETY_NET`] at the latest.
pub async fn next(rx: &mut Option<watch::Receiver<u64>>, fallback: Duration) {
    match rx {
        Some(r) => {
            tokio::select! {
                c = r.changed() => {
                    if c.is_err() {
                        *rx = None;
                        tokio::time::sleep(fallback).await;
                    }
                }
                _ = tokio::time::sleep(SAFETY_NET) => {}
            }
        }
        None => tokio::time::sleep(fallback).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_file_written_in_a_watched_folder_wakes_the_waiter() {
        let d = tempfile::tempdir().unwrap();
        let mut rx = watch_dirs(&[d.path()]);
        assert!(rx.is_some());
        std::fs::write(d.path().join("ui.crt"), b"x").unwrap();
        let woke = tokio::time::timeout(Duration::from_secs(5), next(&mut rx, Duration::from_secs(3600))).await;
        assert!(woke.is_ok());
        assert!(watch_dirs(&[Path::new("/nonexistent/rynas")]).is_none());
        // the folder goes: the watch ends and the waiter falls back to its timer, it never
        // waits for good
        std::fs::remove_dir_all(d.path()).unwrap();
        for _ in 0..3 {
            let r = tokio::time::timeout(Duration::from_secs(5), next(&mut rx, Duration::from_millis(50))).await;
            assert!(r.is_ok());
        }
        assert!(rx.is_none());
    }
}
