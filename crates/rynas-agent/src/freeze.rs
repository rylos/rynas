//! Filesystem freezes (FIFREEZE/FITHAW) that are always undone: on drop, by the watchdog at
//! the plan's limit, by the SIGINT/SIGTERM handler, and by the next start (a record per
//! frozen filesystem in `/run`, never on a frozen filesystem). From abb-agent's
//! `volume.rs` (same author, relicensed AGPL-3.0-or-later for rynas).

use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicI32, Ordering};

const FIFREEZE: libc::c_ulong = 0xC004_5877;
const FITHAW: libc::c_ulong = 0xC004_5878;
const FROZEN_DIR: &str = "/run/rynas-agent/frozen";

/// Descriptors of the frozen filesystems, for the signal handler (-1: free slot).
const SLOTS: usize = 32;
static FROZEN: [AtomicI32; SLOTS] = [const { AtomicI32::new(-1) }; SLOTS];

pub struct Freeze {
    dir: std::fs::File,
    pub mount: String,
    marker: std::path::PathBuf,
    slot: Option<usize>,
}

fn thaw_fd(fd: i32) -> std::io::Result<bool> {
    let mut arg: libc::c_int = 0;
    // SAFETY: FITHAW on a directory descriptor we own
    if unsafe { libc::ioctl(fd, FITHAW as _, &mut arg) } != 0 {
        let e = std::io::Error::last_os_error();
        return if e.raw_os_error() == Some(libc::EINVAL) { Ok(false) } else { Err(e) };
    }
    Ok(true)
}

impl Freeze {
    /// Freezes the filesystem mounted at `mount`.
    pub fn new(mount: &str) -> std::io::Result<Freeze> {
        let dir = std::fs::File::open(mount)?;
        let marker = std::path::Path::new(FROZEN_DIR).join(dir.metadata()?.dev().to_string());
        std::fs::create_dir_all(FROZEN_DIR)?;
        std::fs::write(&marker, mount)?;
        let mut arg: libc::c_int = 0;
        // SAFETY: FIFREEZE on a directory descriptor we own
        if unsafe { libc::ioctl(dir.as_raw_fd(), FIFREEZE as _, &mut arg) } != 0 {
            let e = std::io::Error::last_os_error();
            let _ = std::fs::remove_file(&marker);
            return Err(e);
        }
        let slot = FROZEN
            .iter()
            .position(|s| s.compare_exchange(-1, dir.as_raw_fd(), Ordering::SeqCst, Ordering::SeqCst).is_ok());
        Ok(Freeze {
            dir,
            mount: mount.to_string(),
            marker,
            slot,
        })
    }

    /// Thaws now (the watchdog); `Ok(false)` when it was not frozen any more.
    pub fn thaw(&self) -> std::io::Result<bool> {
        thaw_fd(self.dir.as_raw_fd())
    }
}

impl Drop for Freeze {
    fn drop(&mut self) {
        match self.thaw() {
            Ok(_) => {
                let _ = std::fs::remove_file(&self.marker);
            }
            Err(e) => eprintln!("thaw {}: {e}", self.mount),
        }
        if let Some(s) = self.slot {
            FROZEN[s].store(-1, Ordering::SeqCst);
        }
    }
}

/// From a signal handler: thaws everything still frozen (async-signal-safe: ioctl only).
pub fn thaw_all_from_signal() {
    for s in &FROZEN {
        let fd = s.load(Ordering::SeqCst);
        if fd >= 0 {
            let mut arg: libc::c_int = 0;
            // SAFETY: ioctl is async-signal-safe; the descriptor stays open while recorded
            unsafe { libc::ioctl(fd, FITHAW as _, &mut arg) };
        }
    }
}

/// Thaws what an earlier run froze and could not thaw (killed while frozen): only the
/// recorded filesystems, and only when the same one is still mounted there.
pub fn thaw_leftovers() {
    let Ok(entries) = std::fs::read_dir(FROZEN_DIR) else { return };
    for e in entries.flatten() {
        let marker = e.path();
        let dev = e.file_name().to_string_lossy().parse::<u64>().ok();
        if let (Some(dev), Ok(mount)) = (dev, std::fs::read_to_string(&marker))
            && let Ok(dir) = std::fs::File::open(&mount)
            && dir.metadata().is_ok_and(|m| m.dev() == dev)
        {
            match thaw_fd(dir.as_raw_fd()) {
                Ok(true) => eprintln!("{mount}: thawed (left frozen by an interrupted backup)"),
                Ok(false) => {}
                Err(e) => {
                    eprintln!("thaw {mount}: {e}");
                    continue;
                }
            }
        }
        let _ = std::fs::remove_file(&marker);
    }
}
