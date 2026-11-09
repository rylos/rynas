//! Writes a file to disk while it is being copied, instead of leaving it to the kernel.
//!
//! Left alone, a copy fills the page cache up to the limit of its cgroup, and the kernel then
//! stops the writer (`balance_dirty_pages`) until enough has reached the disk: bursts at full
//! speed, then seconds at a fraction of it. Here each window is sent to the disk as soon as it is
//! complete and the writer waits only for the window before it, so at most about three windows
//! are in memory and a slow disk slows the copy evenly.
//!
//! The window adapts to where the worker runs: an eighth of the memory limit of its cgroup, or
//! 1/64 of the RAM when that is smaller (the kernel's own dirty limits are a fraction of it).
//! Where the calls do not apply the file is simply left to the kernel; a write error the disk
//! reports while waiting fails the copy, as it should.

use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::OnceLock;

const MIB: u64 = 1 << 20;
const MIN_WINDOW: u64 = 4 * MIB;
/// A cancel or a pause waits for at most about this much to reach the disk.
const MAX_WINDOW: u64 = 64 * MIB;
const CGROUP_FS: &str = "/sys/fs/cgroup";

/// The window for a memory limit of the cgroup (if any) and this much RAM (if known).
fn window_for(cgroup_limit: Option<u64>, ram: Option<u64>) -> u64 {
    let window = [cgroup_limit.map(|l| l / 8), ram.map(|r| r / 64)].into_iter().flatten().min().unwrap_or(MAX_WINDOW);
    window.clamp(MIN_WINDOW, MAX_WINDOW) & !(MIB - 1)
}

/// A `memory.high` or `memory.max` file: bytes, or "max" for no limit.
fn parse_limit(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// The lowest memory limit on this cgroup and the ones above it (`/proc/self/cgroup` text).
fn cgroup_limit(proc_cgroup: &str, root: &Path) -> Option<u64> {
    let own = proc_cgroup.lines().find_map(|l| l.strip_prefix("0::/"))?;
    let leaf = root.join(own);
    leaf.ancestors()
        .take_while(|dir| dir.starts_with(root) && *dir != root)
        .flat_map(|dir| ["memory.high", "memory.max"].map(|f| dir.join(f)))
        .filter_map(|f| parse_limit(&fs::read_to_string(f).ok()?))
        .min()
}

/// `MemTotal` of `/proc/meminfo`, in bytes.
fn parse_ram(meminfo: &str) -> Option<u64> {
    let kib = meminfo.lines().find_map(|l| l.strip_prefix("MemTotal:"))?.trim().strip_suffix("kB")?;
    Some(kib.trim().parse::<u64>().ok()? * 1024)
}

/// This process's window, worked out once.
fn window() -> u64 {
    static WINDOW: OnceLock<u64> = OnceLock::new();
    *WINDOW.get_or_init(|| {
        let limit = fs::read_to_string("/proc/self/cgroup").ok().and_then(|c| cgroup_limit(&c, Path::new(CGROUP_FS)));
        let ram = fs::read_to_string("/proc/meminfo").ok().and_then(|m| parse_ram(&m));
        window_for(limit, ram)
    })
}

/// Follows a file written from start to end. Call [`WriteBehind::wrote`] after each write.
pub struct WriteBehind {
    window: u64,
    /// where the next window to send starts
    sent: u64,
    written: u64,
    off: bool,
}

impl Default for WriteBehind {
    fn default() -> Self {
        Self::with_window(window())
    }
}

impl WriteBehind {
    /// For a new file, written from its first byte.
    pub fn new() -> Self {
        Self::default()
    }

    fn with_window(window: u64) -> Self {
        WriteBehind { window, sent: 0, written: 0, off: false }
    }

    /// `n` more bytes were written to `file`, at the end of what it had. An error is a failed
    /// write of the data, reported here only (the kernel tells it once per descriptor).
    pub fn wrote(&mut self, file: &File, n: usize) -> io::Result<()> {
        self.written += n as u64;
        while !self.off && self.written - self.sent >= self.window {
            self.send(file)?;
        }
        Ok(())
    }

    /// The file is complete: what was sent is confirmed on the disk, with the last windows.
    /// Nothing to do for a file smaller than a window, which stays with the kernel.
    pub fn finish(&self, file: &File) -> io::Result<()> {
        if self.sent > 0 && !self.off { file.sync_data() } else { Ok(()) }
    }

    /// Sends the next window to the disk, waits for the one before it and drops that from memory.
    fn send(&mut self, file: &File) -> io::Result<()> {
        let (fd, len) = (file.as_raw_fd(), self.window as i64);
        // SAFETY: plain calls on a descriptor that `file` keeps open; ranges are by value
        unsafe {
            if libc::sync_file_range(fd, self.sent as i64, len, libc::SYNC_FILE_RANGE_WRITE) != 0 {
                return self.failed();
            }
            if let Some(before) = self.sent.checked_sub(self.window) {
                let wait = libc::SYNC_FILE_RANGE_WAIT_BEFORE | libc::SYNC_FILE_RANGE_WRITE | libc::SYNC_FILE_RANGE_WAIT_AFTER;
                if libc::sync_file_range(fd, before as i64, len, wait) != 0 {
                    return self.failed();
                }
                libc::posix_fadvise(fd, before as i64, len, libc::POSIX_FADV_DONTNEED);
            }
        }
        self.sent += self.window;
        Ok(())
    }

    /// After a failed call: where it does not apply the file goes back to the kernel; a real
    /// error is the copy's; an interrupted call is made again.
    fn failed(&mut self) -> io::Result<()> {
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINTR) => Ok(()),
            Some(libc::ESPIPE | libc::EINVAL | libc::ENOSYS | libc::EPERM | libc::EOPNOTSUPP) => {
                self.off = true;
                Ok(())
            }
            _ => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, Write};

    #[test]
    fn the_window_follows_the_machine() {
        // the workers' default limit on a 4 GiB NAS
        assert_eq!(window_for(Some(192 * MIB), Some(4096 * MIB)), 24 * MIB);
        // no cgroup limit: the RAM decides
        assert_eq!(window_for(None, Some(4096 * MIB)), 64 * MIB);
        assert_eq!(window_for(None, Some(64 * 1024 * MIB)), MAX_WINDOW);
        // a tiny limit keeps a window worth sending, in whole MiB
        assert_eq!(window_for(Some(8 * MIB), None), MIN_WINDOW);
        assert_eq!(window_for(Some(100 * MIB), None), 12 * MIB);
        assert_eq!(window_for(None, None), MAX_WINDOW);
    }

    #[test]
    fn limits_are_read_up_the_tree() {
        let root = std::env::temp_dir().join(format!("rynas-writeback-{}", std::process::id()));
        let leaf = root.join("a/b");
        fs::create_dir_all(&leaf).unwrap();
        fs::write(root.join("memory.high"), "1\n").unwrap();
        fs::write(root.join("a/memory.high"), "max\n").unwrap();
        fs::write(root.join("a/memory.max"), "300\n").unwrap();
        fs::write(leaf.join("memory.high"), "200\n").unwrap();
        fs::write(leaf.join("memory.max"), "max\n").unwrap();
        // the root's own files do not count
        assert_eq!(cgroup_limit("0::/a/b\n", &root), Some(200));
        fs::write(leaf.join("memory.high"), "max\n").unwrap();
        assert_eq!(cgroup_limit("0::/a/b\n", &root), Some(300));
        assert_eq!(cgroup_limit("1:name=x:/a\n", &root), None);
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(parse_ram("MemFree: 1 kB\nMemTotal:  2048 kB\n"), Some(2 << 20));
    }

    #[test]
    fn the_file_comes_out_whole() {
        let path = std::env::temp_dir().join(format!("rynas-writeback-file-{}", std::process::id()));
        let mut file = File::options().read(true).write(true).create_new(true).open(&path).unwrap();
        let mut behind = WriteBehind::with_window(MIB);
        let block: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
        for _ in 0..40 {
            file.write_all(&block).unwrap();
            behind.wrote(&file, block.len()).unwrap();
        }
        assert!(!behind.off, "a regular file takes the calls");
        assert_eq!(behind.sent, 11 * MIB);
        behind.finish(&file).unwrap();
        file.rewind().unwrap();
        let mut back = Vec::new();
        file.read_to_end(&mut back).unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(back.len(), 40 * block.len());
        assert!(back.chunks(block.len()).all(|c| c == block));
    }

    #[test]
    fn what_cannot_be_synced_is_left_alone() {
        let (read, write) = {
            let mut fds = [0; 2];
            // SAFETY: `fds` has room for the two descriptors, owned by the Files below
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            use std::os::fd::FromRawFd;
            unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
        };
        let mut behind = WriteBehind::with_window(MIB);
        behind.wrote(&write, 2 * MIB as usize).unwrap();
        assert!(behind.off);
        assert_eq!(behind.sent, 0);
        behind.finish(&write).unwrap();
        drop(read);
    }
}
