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
    let mask = libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_MOVED_FROM | libc::IN_CREATE | libc::IN_DELETE | libc::IN_ATTRIB;
    let mut watched: Vec<PathBuf> = Vec::new();
    for d in dirs {
        let Ok(c) = std::ffi::CString::new(d.as_os_str().as_encoded_bytes()) else { continue };
        // SAFETY: valid descriptor and C string
        if unsafe { libc::inotify_add_watch(fd, c.as_ptr(), mask) } >= 0 {
            watched.push(d.to_path_buf());
        }
    }
    if watched.is_empty() {
        // SAFETY: our descriptor, not used afterwards
        unsafe { libc::close(fd) };
        return None;
    }
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
    tracing::debug!("fswatch: watching {watched:?}");
    Some(rx)
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
    }
}
