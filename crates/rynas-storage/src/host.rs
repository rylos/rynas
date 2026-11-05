//! Everything that touches the machine: external commands, sysfs/procfs reads and the ATA
//! power check. Parsers never call these directly, so they stay pure and testable; tests use
//! [`FakeHost`].

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use rynas_tasks::cmd::{Cmd, CmdError as RunError};

use rynas_api_types::storage::PowerState;

/// Output of an external command.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CmdOutput {
    /// Exit code; None when killed by a signal (e.g. on timeout).
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOutput {
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CmdError {
    /// The program could not be started (not installed, no permission).
    Spawn(String),
    Timeout,
}

impl std::fmt::Display for CmdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CmdError::Spawn(e) => write!(f, "cannot run: {e}"),
            CmdError::Timeout => f.write_str("timed out"),
        }
    }
}

/// The machine as seen by the inventory.
pub trait Host: Send + Sync {
    /// Run `program` with `args` (argv only, never a shell) and wait at most `timeout`.
    fn run(&self, program: &str, args: &[&str], timeout: Duration) -> Result<CmdOutput, CmdError>;
    /// Contents of a sysfs/procfs file, None when missing or unreadable.
    fn read(&self, path: &str) -> Option<String>;
    /// Entry names of a directory (unsorted), empty when missing.
    fn list(&self, path: &str) -> Vec<String>;
    /// Target of a symlink resolved to an absolute path (for sysfs device paths).
    fn canonical(&self, path: &str) -> Option<String>;
    /// ATA CHECK POWER MODE, the `hdparm -C` way: it never spins a disk up. None when not
    /// supported (NVMe, USB bridges) or not permitted (not root).
    fn ata_power(&self, dev_path: &str) -> Option<PowerState>;
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> u64;
}

/// The real machine.
#[derive(Debug, Default, Clone)]
pub struct SystemHost;

impl Host for SystemHost {
    /// Through the one command runner of rynas ([`Cmd`]): clean environment with `LC_ALL=C`,
    /// own process group killed on timeout, output capped at 1 MiB per stream.
    fn run(&self, program: &str, args: &[&str], timeout: Duration) -> Result<CmdOutput, CmdError> {
        let cmd = Cmd::new(program).args(args).env_clear().timeout(timeout);
        match cmd.output_blocking() {
            Ok(o) => Ok(CmdOutput {
                status: o.code,
                stdout: o.stdout,
                stderr: o.stderr,
            }),
            Err(RunError::Timeout(_)) => {
                tracing::warn!("{program} {args:?}: timed out after {timeout:?}");
                Err(CmdError::Timeout)
            }
            Err(e) => Err(CmdError::Spawn(format!("{program}: {e}"))),
        }
    }

    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn list(&self, path: &str) -> Vec<String> {
        std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect()
    }

    fn canonical(&self, path: &str) -> Option<String> {
        std::fs::canonicalize(path)
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    }

    fn ata_power(&self, dev_path: &str) -> Option<PowerState> {
        ata_check_power_mode(dev_path)
    }

    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// `HDIO_DRIVE_CMD` from `<linux/hdreg.h>`; libata translates it for SATA disks.
const HDIO_DRIVE_CMD: u64 = 0x031f;
const ATA_CHECK_POWER_MODE: u8 = 0xe5;
const ATA_CHECK_POWER_MODE_RETIRED: u8 = 0x98;

/// Meaning of the sector count register after CHECK POWER MODE (ATA8-ACS), as in `hdparm -C`.
pub fn power_from_ata(count: u8) -> PowerState {
    match count {
        0x00 => PowerState::Standby,
        // NV cache power modes: spindle down / up
        0x40 => PowerState::Standby,
        0x41 => PowerState::Active,
        0x80 => PowerState::Idle,
        0xff => PowerState::Active,
        _ => PowerState::Unknown,
    }
}

fn ata_check_power_mode(dev_path: &str) -> Option<PowerState> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    // read-only + non-blocking: opening the node does not touch the medium, and closing a
    // read-only fd does not trigger a udev "change" event
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(dev_path)
        .ok()?;
    for cmd in [ATA_CHECK_POWER_MODE, ATA_CHECK_POWER_MODE_RETIRED] {
        let mut args: [u8; 4] = [cmd, 0, 0, 0];
        // SAFETY: HDIO_DRIVE_CMD takes a pointer to 4 bytes (command, sector, feature,
        // count) that the kernel reads and overwrites with the status registers.
        let r = unsafe { libc::ioctl(f.as_raw_fd(), HDIO_DRIVE_CMD as _, args.as_mut_ptr()) };
        if r == 0 {
            return Some(power_from_ata(args[2]));
        }
    }
    None
}

/// Whether the kernel lets rynas claim a block device exclusively (`open(O_EXCL)`): it
/// refuses with `EBUSY` while the device or one of its partitions is mounted (in any mount
/// namespace), is a member of an assembled md array, holds an open dm/LUKS mapping or is
/// active swap. The last check before a destructive step: it does not depend on what lsblk
/// or the udev database say.
pub fn claim_free(dev_path: &str) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_EXCL | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(dev_path)
    {
        Ok(_) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EBUSY) => Err(format!(
            "{dev_path} is in use (mounted, in an array, an open mapping or swap)"
        )),
        Err(e) => Err(format!("{dev_path}: {e}")),
    }
}

/// Test double: canned command outputs and files.
#[derive(Default)]
pub struct FakeHost {
    pub commands: Mutex<HashMap<String, CmdOutput>>,
    pub files: HashMap<String, String>,
    pub links: HashMap<String, String>,
    pub power: HashMap<String, PowerState>,
    pub now_ms: u64,
    /// Every command line run, for "did not wake the disk" assertions.
    pub log: Mutex<Vec<String>>,
}

impl FakeHost {
    /// Register the output of `argv` (joined with spaces).
    pub fn cmd(&mut self, argv: &str, status: i32, stdout: &str) -> &mut Self {
        self.commands.get_mut().unwrap().insert(
            argv.to_string(),
            CmdOutput {
                status: Some(status),
                stdout: stdout.to_string(),
                stderr: String::new(),
            },
        );
        self
    }

    pub fn file(&mut self, path: &str, content: &str) -> &mut Self {
        self.files.insert(path.to_string(), content.to_string());
        self
    }

    pub fn ran(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

impl Host for FakeHost {
    fn run(&self, program: &str, args: &[&str], _: Duration) -> Result<CmdOutput, CmdError> {
        let line = std::iter::once(program)
            .chain(args.iter().copied())
            .collect::<Vec<_>>()
            .join(" ");
        self.log.lock().unwrap().push(line.clone());
        self.commands
            .lock()
            .unwrap()
            .get(&line)
            .cloned()
            .ok_or_else(|| CmdError::Spawn(format!("no fake output for `{line}`")))
    }

    fn read(&self, path: &str) -> Option<String> {
        self.files.get(path).cloned()
    }

    fn list(&self, path: &str) -> Vec<String> {
        let prefix = format!("{}/", path.trim_end_matches('/'));
        let mut v: Vec<String> = self
            .files
            .keys()
            .chain(self.links.keys())
            .filter_map(|k| k.strip_prefix(&prefix))
            .map(|rest| rest.split('/').next().unwrap_or(rest).to_string())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    fn canonical(&self, path: &str) -> Option<String> {
        self.links.get(path).cloned()
    }

    fn ata_power(&self, dev_path: &str) -> Option<PowerState> {
        self.power.get(dev_path).copied()
    }

    fn now_ms(&self) -> u64 {
        self.now_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_argv_without_shell() {
        let h = SystemHost;
        let o = h
            .run("echo", &["$HOME", "a b"], Duration::from_secs(5))
            .unwrap();
        assert!(o.success());
        // no shell: no expansion, arguments kept intact
        assert_eq!(o.stdout, "$HOME a b\n");
    }

    #[test]
    fn timeout_kills() {
        let h = SystemHost;
        let t = std::time::Instant::now();
        let r = h.run("sleep", &["5"], Duration::from_millis(100));
        assert_eq!(r, Err(CmdError::Timeout));
        assert!(t.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn missing_program() {
        let r = SystemHost.run("/nonexistent/prog", &[], Duration::from_secs(1));
        assert!(matches!(r, Err(CmdError::Spawn(_))));
    }

    #[test]
    fn env_is_clean() {
        let o = SystemHost.run("env", &[], Duration::from_secs(5)).unwrap();
        let mut keys: Vec<_> = o
            .stdout
            .lines()
            .filter_map(|l| l.split_once('=').map(|(k, _)| k))
            .collect();
        keys.sort();
        assert_eq!(keys, ["LC_ALL", "PATH"]);
    }

    #[test]
    fn ata_power_codes() {
        assert_eq!(power_from_ata(0x00), PowerState::Standby);
        assert_eq!(power_from_ata(0x80), PowerState::Idle);
        assert_eq!(power_from_ata(0xff), PowerState::Active);
        assert_eq!(power_from_ata(0x12), PowerState::Unknown);
    }
}
