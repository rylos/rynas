//! The capabilities a worker keeps once it is the user.
//!
//! Every file operation runs as the user who asked for it, so the kernel applies that
//! user's permissions and POSIX ACLs to everything that changes a file. rynasd, as root,
//! switches to the user between fork and exec ([`crate::identity::become_raw`]); nothing is
//! written on the binary (no file capabilities): systemd gives rynasd what it needs.
//!
//! An administrator may look everywhere: the worker keeps `CAP_DAC_READ_SEARCH`, to read
//! any file and open any folder (a backup received from another machine, owned by a uid
//! the NAS does not know; a read-only snapshot). Writing stays the user's. Other users keep
//! nothing at all.
//!
//! The one exception is an administrator changing the apps' data (`@apps`, when the Docker
//! setting shows it): the files there belong to root or to the apps' accounts, so that
//! worker also keeps `CAP_DAC_OVERRIDE`, `CAP_FOWNER` and `CAP_CHOWN` ([`Powers::AppData`]),
//! never in its ambient set, and rynasd starts it under a Landlock ruleset that allows
//! writes below the apps' folders only (`jobs::manager::worker_command`).

use std::io;

const CAP_CHOWN: u32 = 0;
const CAP_DAC_OVERRIDE: u32 = 1;
const CAP_DAC_READ_SEARCH: u32 = 2;
const CAP_FOWNER: u32 = 3;
/// The last capability the bounding-set loop tries (the kernel answers EINVAL past its own).
const CAP_LAST: u32 = 63;

const READ: u64 = 1 << CAP_DAC_READ_SEARCH;
/// An administrator's worker in the apps' data: reading, writing whatever the owner, and
/// giving what it makes the owner of the folder it lands in. rynasd confines its writes to
/// the apps' folders with Landlock ([`crate::landlock`]) before it runs.
const APP_DATA: u64 = READ | 1 << CAP_DAC_OVERRIDE | 1 << CAP_FOWNER | 1 << CAP_CHOWN;

/// What a worker keeps once it is the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Powers {
    /// Nothing (every user).
    None,
    /// `CAP_DAC_READ_SEARCH` (administrators).
    Read,
    /// Reading plus `CAP_DAC_OVERRIDE`, `CAP_FOWNER`, `CAP_CHOWN`: an administrator changing
    /// the apps' data, under a Landlock ruleset that allows writes there only.
    AppData,
}

impl Powers {
    fn bits(self) -> u64 {
        match self {
            Powers::None => 0,
            Powers::Read => READ,
            Powers::AppData => APP_DATA,
        }
    }
}

/// Whether this process may write anywhere its Landlock ruleset allows (an administrator's
/// worker in the apps' data, [`Powers::AppData`]): what it makes is then given the owner of
/// the folder it is in ([`crate::confine::adopt`]).
pub fn app_data() -> bool {
    // a worker's capabilities never change once it runs: read once (a copy asks per file)
    static APP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // never rynasd itself (root, which links this crate): only a worker, already the user
    // SAFETY: plain getter
    *APP.get_or_init(|| unsafe { libc::geteuid() } != 0 && status_field("CapEff:").is_some_and(|v| v & (1 << CAP_CHOWN) != 0))
}

fn status_field(name: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| line.strip_prefix(name)).and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
}

/// Whether this process holds no capability but reading: it can no longer become anyone
/// else nor gain anything through exec. The inheritable set alone gives nothing (a
/// capability must also be permitted to be raised, ambient or effective; file
/// capabilities are void under `no_new_privs`).
pub fn only_reading() -> bool {
    ["CapEff:", "CapPrm:", "CapAmb:"].iter().all(|f| status_field(f).is_some_and(|v| v & !READ == 0))
}

/// Whether this process holds at most [`Powers::AppData`], and nothing but reading in its
/// ambient set (the programs it runs never write as it does).
fn at_most_app_data() -> bool {
    ["CapEff:", "CapPrm:"].iter().all(|f| status_field(f).is_some_and(|v| v & !APP_DATA == 0)) && status_field("CapAmb:").is_some_and(|v| v & !READ == 0)
}

/// Whether this process can read everywhere (an administrator's worker).
pub fn reads_everywhere() -> bool {
    status_field("CapEff:").is_some_and(|v| v & READ != 0)
}

#[repr(C)]
struct Header {
    version: u32,
    pid: i32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Data {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}
const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

fn capset(bits: u32, inheritable: u32) -> io::Result<()> {
    let header = Header { version: LINUX_CAPABILITY_VERSION_3, pid: 0 };
    let none = Data { effective: 0, permitted: 0, inheritable: 0 };
    let data = [Data { effective: bits, permitted: bits, inheritable }, none];
    // SAFETY: the kernel reads a v3 header and two data structs
    let rc = unsafe { libc::syscall(libc::SYS_capset, &header as *const Header, data.as_ptr()) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Before the identity change, still privileged: drops every capability from the bounding
/// set but those of `powers`, so that nothing the worker runs can ever get one back.
/// Failures are ignored (no CAP_SETPCAP in development): the sets themselves are emptied
/// after the switch anyway. Only system calls: runs between fork and exec.
pub fn limit_bounding(powers: Powers) {
    let keep = powers.bits();
    for cap in 0..=CAP_LAST {
        if keep & (1 << cap) != 0 {
            continue;
        }
        // SAFETY: plain prctl with integer arguments
        unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0) };
    }
}

/// After becoming the user: every capability goes but those of `powers`. Reading also
/// stays in the inheritable and ambient sets, so the programs the worker runs (7z, ffmpeg)
/// read everywhere too; the other powers never pass on. Only system calls, no allocation:
/// it runs between fork and exec.
pub fn keep(powers: Powers) -> io::Result<()> {
    let header = Header { version: LINUX_CAPABILITY_VERSION_3, pid: 0 };
    let mut data = [Data { effective: 0, permitted: 0, inheritable: 0 }; 2];
    // SAFETY: the kernel writes two data structs for a v3 header
    let rc = unsafe { libc::syscall(libc::SYS_capget, &header as *const Header, data.as_mut_ptr()) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    let keep = data[0].permitted & powers.bits() as u32;
    let read = keep & READ as u32;
    capset(keep, read)?;
    if read != 0 {
        // SAFETY: plain prctl with integer arguments
        let rc = unsafe {
            libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_RAISE as libc::c_ulong, CAP_DAC_READ_SEARCH as libc::c_ulong, 0, 0)
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Empties every capability set of the calling thread, the ambient one included.
pub fn clear() -> io::Result<()> {
    // SAFETY: plain prctl with integer arguments
    unsafe { libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_CLEAR_ALL as libc::c_ulong, 0, 0, 0) };
    capset(0, 0)
}

/// What every worker checks first: it is not root, it holds nothing but (maybe) reading —
/// or, started for the apps' data, nothing beyond [`Powers::AppData`] with reading alone
/// ambient — and nobody can attach a debugger to it to borrow that.
pub fn check_worker() -> io::Result<()> {
    // SAFETY: plain prctl / getters
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
        if libc::getuid() == 0 || libc::geteuid() == 0 {
            return Err(io::Error::other("the file manager worker never runs as root"));
        }
    }
    if !only_reading() && !at_most_app_data() {
        return Err(io::Error::other("the file manager worker holds capabilities it must not have"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_process_holds_nothing_dangerous() {
        // the test runner is an ordinary user: the check passes outside the container
        // tests (as root it would refuse, which is what the container test checks)
        // SAFETY: plain getter
        if unsafe { libc::getuid() } != 0 {
            assert!(only_reading());
        }
    }
}
