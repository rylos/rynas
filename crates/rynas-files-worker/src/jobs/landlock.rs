//! Landlock (Linux 5.13, unprivileged): the archive tools may write below their own folder
//! only.
//!
//! An archive can hold a link (`a -> /etc`, `a -> ../../other-share`) and then a file below
//! it (`a/passwd`): a tool that follows its own link writes out of the folder it extracts
//! to. GNU tar, 7-Zip and unrar each guard against some of it; the kernel guards against
//! all of it. The tool runs under a ruleset that handles every kind of write and allows
//! them only below the extraction folder (and on `/dev/null`); reading is
//! not restricted. Where the kernel has no Landlock (disabled, older than 5.13) the tools
//! run as before, with their own guards, and the links they made are moved into place as
//! links, never followed by the worker.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::Path;

const CREATE_RULESET_VERSION: u32 = 1;
const RULE_PATH_BENEATH: u32 = 1;

const WRITE_FILE: u64 = 1 << 1;
const REMOVE_DIR: u64 = 1 << 4;
const REMOVE_FILE: u64 = 1 << 5;
const MAKE_CHAR: u64 = 1 << 6;
const MAKE_DIR: u64 = 1 << 7;
const MAKE_REG: u64 = 1 << 8;
const MAKE_SOCK: u64 = 1 << 9;
const MAKE_FIFO: u64 = 1 << 10;
const MAKE_BLOCK: u64 = 1 << 11;
const MAKE_SYM: u64 = 1 << 12;
/// ABI 2: links and renames between folders.
const REFER: u64 = 1 << 13;
/// ABI 3: truncate(2).
const TRUNCATE: u64 = 1 << 14;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

#[repr(C, packed)]
struct PathBeneath {
    allowed_access: u64,
    parent_fd: i32,
}

/// A ruleset ready to be applied to a program before it runs.
pub struct Ruleset {
    fd: OwnedFd,
}

/// The Landlock ABI of the running kernel, `None` without Landlock.
pub fn abi() -> Option<i64> {
    // SAFETY: the version query takes no attributes
    let v = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<RulesetAttr>(), 0usize, CREATE_RULESET_VERSION) };
    (v > 0).then_some(v)
}

fn add(ruleset: &OwnedFd, dir: &Path, access: u64) -> io::Result<()> {
    let c = CString::new(dir.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: a valid C string
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just opened
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let rule = PathBeneath { allowed_access: access, parent_fd: fd.as_raw_fd() };
    // SAFETY: a ruleset descriptor and a path-beneath rule
    let rc = unsafe { libc::syscall(libc::SYS_landlock_add_rule, ruleset.as_raw_fd(), RULE_PATH_BENEATH, &rule as *const PathBeneath, 0u32) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Every write refused but below `dir` (and on `/dev/null`); `None` when the
/// kernel has no Landlock or the ruleset cannot be made.
pub fn writes_below(dir: &Path) -> Option<Ruleset> {
    let abi = abi()?;
    let mut handled = WRITE_FILE | REMOVE_DIR | REMOVE_FILE | MAKE_CHAR | MAKE_DIR | MAKE_REG | MAKE_SOCK | MAKE_FIFO | MAKE_BLOCK | MAKE_SYM;
    if abi >= 2 {
        handled |= REFER;
    }
    if abi >= 3 {
        handled |= TRUNCATE;
    }
    let attr = RulesetAttr { handled_access_fs: handled };
    // SAFETY: a ruleset attribute of the size given
    let fd = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, &attr as *const RulesetAttr, std::mem::size_of::<RulesetAttr>(), 0u32) };
    if fd < 0 {
        return None;
    }
    // SAFETY: just made
    let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    add(&fd, dir, handled).ok()?;
    let _ = add(&fd, Path::new("/dev/null"), WRITE_FILE | (handled & TRUNCATE));
    Some(Ruleset { fd })
}

impl Ruleset {
    /// The program `cmd` starts runs under the ruleset (set between fork and exec).
    pub fn apply(&self, cmd: &mut std::process::Command) {
        let fd = self.fd.as_raw_fd();
        // SAFETY: only system calls between fork and exec
        unsafe {
            cmd.pre_exec(move || {
                // the worker has no_new_privs already (identity::become_raw); Landlock wants it
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::syscall(libc::SYS_landlock_restrict_self, fd, 0u32) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_only_below_the_folder() {
        let Some(_) = abi() else {
            eprintln!("no Landlock in this kernel: skipped");
            return;
        };
        let t = tempfile::tempdir().unwrap();
        let inside = t.path().join("inside");
        let outside = t.path().join("outside");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, inside.join("link")).unwrap();
        let rules = writes_below(&inside).unwrap();
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg("echo a > in.txt; echo b > link/out.txt; echo c > ../outside/out2.txt; cat ../outside/../inside/in.txt").current_dir(&inside);
        rules.apply(&mut cmd);
        let out = cmd.output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "a\n");
        assert!(inside.join("in.txt").exists());
        assert!(!outside.join("out.txt").exists() && !outside.join("out2.txt").exists());
    }
}
