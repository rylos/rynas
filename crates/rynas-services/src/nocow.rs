//! The btrfs no copy-on-write attribute of a folder (`FS_NOCOW_FL`, `chattr +C`, `lsattr -d`
//! shows `C`).
//!
//! A folder with the attribute passes it to every file created in it afterwards: those files
//! are rewritten in place, without data checksums and without compression (recordings,
//! databases, VM images). The kernel sets it only on empty files, so turning it on is
//! offered for empty folders only; files already in a folder keep what they have. Reading
//! it is one unprivileged ioctl on the open folder (no fork per share); writing goes
//! through `chattr` in a task, so a dry system only logs it.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::time::Duration;

use nix::libc;
use rynas_config::System;
use rynas_tasks::Cmd;

use crate::host::ShareHost;

/// `FS_NOCOW_FL` (linux/fs.h).
pub const FS_NOCOW_FL: libc::c_int = 0x0080_0000;

/// Names that do not count when a share folder must be empty: its recycle bin and its
/// snapshot folder (a bind mount of the snapshots).
pub const IGNORED_ENTRIES: [&str; 2] = ["#recycle", rynas_api_types::SNAPSHOT_FOLDER];

/// `FS_IOC_GETFLAGS`: `_IOR('f', 1, long)`; the kernel reads and writes an `int`.
fn getflags_request() -> libc::c_ulong {
    nix::request_code_read!(b'f', 1, std::mem::size_of::<libc::c_long>()) as libc::c_ulong
}

/// The inode flags of an open file or folder.
pub fn flags_of(f: &File) -> std::io::Result<libc::c_int> {
    let mut flags: libc::c_int = 0;
    // SAFETY: FS_IOC_GETFLAGS writes one int to the pointer, which outlives the call.
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), getflags_request() as _, &mut flags) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(flags)
}

/// `FS_IOC_SETFLAGS`: `_IOW('f', 2, long)`; the kernel reads an `int`.
fn setflags_request() -> libc::c_ulong {
    nix::request_code_write!(b'f', 2, std::mem::size_of::<libc::c_long>()) as libc::c_ulong
}

/// The open folder is on btrfs (`fstatfs`).
pub fn on_btrfs(f: &File) -> bool {
    nix::sys::statfs::fstatfs(f).is_ok_and(|st| {
        #[allow(clippy::useless_conversion)]
        let magic = i64::from(st.filesystem_type().0);
        magic as u32 == 0x9123_683E
    })
}

/// Sets the attribute on an open, empty folder made a moment ago (app folders at install):
/// a descriptor, so no path is resolved again. Other inode flags are kept.
pub fn set_fd(f: &File) -> std::io::Result<()> {
    let flags = flags_of(f)? | FS_NOCOW_FL;
    // SAFETY: FS_IOC_SETFLAGS reads one int from the pointer, which outlives the call.
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), setflags_request() as _, &flags) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Whether the folder at `path` has the attribute; `None` when it cannot be read (not
/// btrfs, no permission, gone). Opening a folder read-only never changes it.
pub fn get(path: &str) -> Option<bool> {
    let f = File::open(path).ok()?;
    flags_of(&f).ok().map(|fl| fl & FS_NOCOW_FL != 0)
}

/// Why the attribute cannot be turned on for a folder with these entries: `None` when the
/// folder counts as empty ([`IGNORED_ENTRIES`] aside).
pub fn not_empty(entries: &[String]) -> Option<String> {
    let mut left: Vec<&str> = entries
        .iter()
        .map(String::as_str)
        .filter(|n| !IGNORED_ENTRIES.contains(n))
        .collect();
    if left.is_empty() {
        return None;
    }
    left.sort_unstable();
    let n = left.len();
    let shown: Vec<&str> = left.into_iter().take(3).collect();
    Some(format!(
        "the folder is not empty ({n} {}, e.g. {}): no copy-on-write only applies to files created afterwards, so it is turned on for empty folders only",
        if n == 1 { "entry" } else { "entries" },
        shown.join(", ")
    ))
}

/// The `chattr` command line that sets or clears the attribute on the folder itself (not
/// recursive: files inside keep theirs).
pub fn chattr_args(path: &str, on: bool) -> Vec<String> {
    vec![if on { "+C" } else { "-C" }.to_string(), path.to_string()]
}

/// Why the attribute of a folder cannot change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Turning it on needs an empty folder.
    NotEmpty(String),
    /// Not a local btrfs folder, a symbolic link, missing.
    Invalid(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::NotEmpty(m) | Refusal::Invalid(m) => f.write_str(m),
        }
    }
}

impl From<Refusal> for String {
    fn from(r: Refusal) -> String {
        r.to_string()
    }
}

/// Can the attribute of the folder at `path` go `on` (or off)? A local btrfs folder, not a
/// symbolic link; on only when it is empty. `Ok(false)`: it already is.
pub fn check(host: &dyn ShareHost, path: &str, on: bool) -> Result<bool, Refusal> {
    let info = host.probe(path);
    if !info.exists || !info.is_dir {
        return Err(Refusal::Invalid(format!("{path} is not a folder (apply the share first)")));
    }
    if info.symlink {
        return Err(Refusal::Invalid(format!("{path} is a symbolic link: its target is not changed")));
    }
    if info.fs_type != "btrfs" {
        return Err(Refusal::Invalid(format!(
            "{path} is on {}: no copy-on-write is a btrfs setting (other filesystems always write in place)",
            if info.fs_type.is_empty() { "an unknown filesystem" } else { &info.fs_type }
        )));
    }
    if info.nocow == Some(on) {
        return Ok(false);
    }
    if on {
        let names: Vec<String> = host.list_dir(path).into_iter().map(|e| e.name).collect();
        if let Some(e) = not_empty(&names) {
            return Err(Refusal::NotEmpty(format!("{path}: {e}")));
        }
    }
    Ok(true)
}

/// Turns the attribute on or off for the folder at `path` (task `shares.nocow`) after
/// [`check`]. `chattr` runs through `system`, so a dry system only logs it.
pub fn change(system: &dyn System, host: &dyn ShareHost, path: &str, on: bool, log: &dyn Fn(&str)) -> Result<(), String> {
    if !check(host, path, on)? {
        log(&format!("{path}: no copy-on-write is already {}", if on { "on" } else { "off" }));
        return Ok(());
    }
    let cmd = Cmd::new("chattr").args(chattr_args(path, on)).env_clear().timeout(Duration::from_secs(30));
    log(&format!("$ {}", cmd.display()));
    system.run(&cmd)?;
    if !on {
        log("files already in the folder keep no copy-on-write; files created from now on get copy-on-write and checksums");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::MemoryShares;
    use rynas_config::MemorySystem;

    #[test]
    fn change_checks_the_folder_then_runs_chattr() {
        let sys = MemorySystem::new("nas");
        let h = MemoryShares::new();
        let log = |_: &str| {};
        h.add_dir("/vol1/cams", Some("11111111-2222-4333-8444-555555555555"));
        h.add_dir("/vol1/cams/#recycle", None);
        change(&sys, &h, "/vol1/cams", true, &log).unwrap();
        h.add_dir("/vol1/cams/2026-11-07", None);
        let e = change(&sys, &h, "/vol1/cams", true, &log).unwrap_err();
        assert!(e.contains("not empty (1 entry, e.g. 2026-11-07)"), "{e}");
        // off is allowed with files inside
        h.set_nocow("/vol1/cams", true);
        change(&sys, &h, "/vol1/cams", false, &log).unwrap();
        // already in that state: nothing runs
        change(&sys, &h, "/vol1/cams", true, &log).unwrap();
        // ext4 and missing folders are refused
        h.add_dir("/data/x", None);
        assert!(change(&sys, &h, "/data/x", true, &log).unwrap_err().contains("is on ext4"));
        assert!(change(&sys, &h, "/vol1/none", true, &log).unwrap_err().contains("not a folder"));
        let calls: Vec<String> = sys.state().calls.iter().filter(|c| c.contains("chattr")).cloned().collect();
        assert_eq!(calls, ["run chattr +C /vol1/cams", "run chattr -C /vol1/cams"]);
    }

    #[test]
    fn request_code_matches_linux() {
        // FS_IOC_GETFLAGS on 64-bit Linux
        #[cfg(target_pointer_width = "64")]
        assert_eq!(getflags_request(), 0x8008_6601);
        #[cfg(target_pointer_width = "64")]
        assert_eq!(setflags_request(), 0x4008_6602);
    }

    #[test]
    fn emptiness_ignores_the_recycle_bin_and_snapshot_folder() {
        assert_eq!(not_empty(&[]), None);
        assert_eq!(not_empty(&["#recycle".into(), "#snapshot".into()]), None);
        let e = not_empty(&["b.mp4".into(), "#recycle".into(), "a.mp4".into(), "c".into(), "d".into()]).unwrap();
        assert!(e.contains("4 entries, e.g. a.mp4, b.mp4, c"), "{e}");
        assert_eq!(chattr_args("/vol1/cams", true), ["+C", "/vol1/cams"]);
        assert_eq!(chattr_args("/vol1/cams", false), ["-C", "/vol1/cams"]);
    }

    #[test]
    fn reading_a_folder_never_fails_hard() {
        // tmpfs/ext4 answer or refuse: either way no panic, and a missing path is None
        let d = tempfile::tempdir().unwrap();
        let _ = get(d.path().to_str().unwrap());
        assert_eq!(get("/nonexistent/rynas/nocow"), None);
    }
}
