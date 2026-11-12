//! The capabilities a worker keeps once it is the user.
//!
//! Every file operation runs as the user who asked for it, so the kernel applies that
//! user's permissions and POSIX ACLs to everything that changes a file. rynasd, as root,
//! switches to the user between fork and exec ([`crate::identity::become_raw`]); nothing is
//! written on the binary (no file capabilities): systemd gives rynasd what it needs.
//!
//! An administrator may do in the shared folders and on the removable drives whatever the
//! Unix permissions would deny (the way DSM's File Station works for its administrators):
//! the worker keeps `CAP_DAC_READ_SEARCH` (read any file, open any folder: a backup received
//! from another machine, owned by a uid the NAS does not know; a read-only snapshot),
//! `CAP_DAC_OVERRIDE` (write, create, delete and rename whatever the mode bits and ACLs say)
//! and `CAP_FOWNER` (sticky folders, times and modes of files it does not own):
//! [`Powers::Admin`]. Never `CAP_CHOWN`: what it makes stays the administrator's, existing
//! files keep their owners. rynasd starts it under a Landlock ruleset that allows writes
//! below the shares' and drives' folders only (`jobs::manager::worker_command`): the system's
//! folders stay out of reach, and so do the apps' folders (below). A read-only mount or a
//! read-only btrfs snapshot stays read-only (EROFS comes before any capability). Where the
//! kernel cannot fence those writes (no Landlock, or an ABI below
//! [`crate::landlock::APP_POWERS_ABI`]) the administrator's worker keeps reading only, and
//! writing stays the user's. Other users keep nothing at all.
//!
//! An administrator changing the apps' data (`@apps`, when the Docker setting shows it): the
//! files there belong to root or to the apps' accounts, and what the worker makes there must
//! go to them too, so that worker also keeps `CAP_CHOWN` ([`Powers::AppData`]) under a
//! Landlock ruleset that allows writes below the apps' folders only.
//!
//! Only reading passes on to the programs a worker runs (7z, unrar, tar, ffmpeg: inheritable
//! and ambient): the powers to write over owners never do. Those programs read archives and
//! media from anyone, and Landlock does not cover chmod or extended attributes; they write
//! only into folders the worker made for them (`.rynas-x-…`, owned by the user).

use std::io;

const CAP_CHOWN: u32 = 0;
const CAP_DAC_OVERRIDE: u32 = 1;
const CAP_DAC_READ_SEARCH: u32 = 2;
const CAP_FOWNER: u32 = 3;
/// The last capability the bounding-set loop tries (the kernel answers EINVAL past its own).
const CAP_LAST: u32 = 63;

const READ: u64 = 1 << CAP_DAC_READ_SEARCH;
/// An administrator's worker: reading, and writing over the mode bits, ACLs and owners of
/// the shares' files. rynasd confines its writes to the shares' and removable drives'
/// folders with Landlock ([`crate::landlock`]) before it runs.
const ADMIN: u64 = READ | 1 << CAP_DAC_OVERRIDE | 1 << CAP_FOWNER;
/// An administrator's worker in the apps' data: as [`ADMIN`], and giving what it makes the
/// owner of the folder it lands in. rynasd confines its writes to the apps' folders with
/// Landlock before it runs.
const APP_DATA: u64 = ADMIN | 1 << CAP_CHOWN;

/// What a worker keeps once it is the user. Each level holds the one before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Powers {
    /// Nothing (every user).
    None,
    /// `CAP_DAC_READ_SEARCH`: an administrator where the kernel cannot fence writes.
    Read,
    /// Reading plus `CAP_DAC_OVERRIDE` and `CAP_FOWNER`: an administrator in the shares and
    /// on the removable drives, under a Landlock ruleset that allows writes there only.
    Admin,
    /// [`Powers::Admin`] plus `CAP_CHOWN`: an administrator changing the apps' data, under a
    /// Landlock ruleset that allows writes there only.
    AppData,
}

impl Powers {
    fn bits(self) -> u64 {
        match self {
            Powers::None => 0,
            Powers::Read => READ,
            Powers::Admin => ADMIN,
            Powers::AppData => APP_DATA,
        }
    }

    /// The lowest level that holds every capability of `bits`; `None` when no level does.
    fn covering(bits: u64) -> Option<Powers> {
        [Powers::None, Powers::Read, Powers::Admin, Powers::AppData].into_iter().find(|p| bits & !p.bits() == 0)
    }
}

/// Whether this process writes over owners where its Landlock ruleset allows it
/// ([`Powers::Admin`] or [`Powers::AppData`]): what it may do is then asked of the kernel
/// with its effective identity and capabilities (`crate::fsops::access`).
pub fn writes_over_owners() -> bool {
    // a worker's capabilities never change once it runs: read once (a listing asks per item)
    static OVER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // never rynasd itself (root, which links this crate): only a worker, already the user
    // SAFETY: plain getter
    *OVER.get_or_init(|| unsafe { libc::geteuid() } != 0 && status_field("CapEff:").is_some_and(|v| v & (1 << CAP_DAC_OVERRIDE) != 0))
}

/// Whether this process is an administrator's worker in the apps' data
/// ([`Powers::AppData`]): what it makes is then given the owner of the folder it is in
/// ([`crate::confine::adopt`]). An administrator's worker in the shares ([`Powers::Admin`])
/// has no `CAP_CHOWN`: what it makes stays its own.
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

/// Whether capability sets are those of a worker: nothing beyond the set of the level they
/// show ([`Powers::covering`] of the effective and permitted sets: nothing that would let it
/// become someone else, load modules, mount…), effective within permitted, and nothing but
/// reading ambient (the programs it runs never write over owners).
fn worker_sets(eff: u64, prm: u64, amb: u64) -> bool {
    Powers::covering(eff | prm).is_some() && eff & !prm == 0 && amb & !READ == 0
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

/// What every worker checks first: it is not root, it holds no capability beyond the set
/// of its level ([`Powers`]: nothing, reading, an administrator's, the apps' data), effective
/// within permitted, reading alone ambient — and nobody can attach a debugger to it to
/// borrow that.
pub fn check_worker() -> io::Result<()> {
    // SAFETY: plain prctl / getters
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
        if libc::getuid() == 0 || libc::geteuid() == 0 {
            return Err(io::Error::other("the file manager worker never runs as root"));
        }
    }
    let sets = (status_field("CapEff:"), status_field("CapPrm:"), status_field("CapAmb:"));
    let (Some(eff), Some(prm), Some(amb)) = sets else {
        return Err(io::Error::other("the file manager worker cannot read its own capabilities"));
    };
    if !worker_sets(eff, prm, amb) {
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
            assert!(!writes_over_owners());
        }
    }

    #[test]
    fn each_level_holds_the_one_before() {
        assert_eq!(Powers::None.bits(), 0);
        assert_eq!(Powers::Read.bits(), 1 << CAP_DAC_READ_SEARCH);
        assert_eq!(Powers::Admin.bits(), 1 << CAP_DAC_READ_SEARCH | 1 << CAP_DAC_OVERRIDE | 1 << CAP_FOWNER);
        assert_eq!(Powers::AppData.bits(), Powers::Admin.bits() | 1 << CAP_CHOWN);
        // an administrator never gives away what it makes, nor takes files over
        assert_eq!(Powers::Admin.bits() & (1 << CAP_CHOWN), 0);
        for (lower, higher) in [(Powers::None, Powers::Read), (Powers::Read, Powers::Admin), (Powers::Admin, Powers::AppData)] {
            assert_eq!(lower.bits() & !higher.bits(), 0, "{lower:?} in {higher:?}");
        }
        assert_eq!(Powers::covering(0), Some(Powers::None));
        assert_eq!(Powers::covering(READ), Some(Powers::Read));
        assert_eq!(Powers::covering(1 << CAP_DAC_OVERRIDE), Some(Powers::Admin));
        assert_eq!(Powers::covering(ADMIN), Some(Powers::Admin));
        assert_eq!(Powers::covering(1 << CAP_CHOWN), Some(Powers::AppData));
        assert_eq!(Powers::covering(APP_DATA), Some(Powers::AppData));
    }

    #[test]
    fn a_worker_holds_nothing_beyond_its_level() {
        const SETUID: u64 = 1 << 7;
        const SYS_ADMIN: u64 = 1 << 21;
        const FSETID: u64 = 1 << 4;
        // what keep() leaves for each level: the level's set, reading alone ambient
        for p in [Powers::None, Powers::Read, Powers::Admin, Powers::AppData] {
            let amb = p.bits() & READ;
            assert!(worker_sets(p.bits(), p.bits(), amb), "{p:?}");
        }
        // anything beyond the highest level
        for extra in [SETUID, SYS_ADMIN, FSETID] {
            assert!(!worker_sets(ADMIN | extra, ADMIN | extra, READ));
            assert!(!worker_sets(ADMIN, ADMIN | extra, READ));
        }
        // the powers to write over owners never ambient: the programs it runs would have them
        assert!(!worker_sets(ADMIN, ADMIN, READ | 1 << CAP_DAC_OVERRIDE));
        assert!(!worker_sets(ADMIN, ADMIN, READ | 1 << CAP_FOWNER));
        assert!(!worker_sets(APP_DATA, APP_DATA, READ | 1 << CAP_CHOWN));
        // effective beyond permitted cannot happen, and is refused if it ever did
        assert!(!worker_sets(ADMIN, READ, READ));
    }
}
