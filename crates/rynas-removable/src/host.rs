//! Everything an action does to the machine, behind [`Host`] so the actions are tested with
//! a recording fake: commands (argv only, never a shell), sysfs writes, directories, the
//! SCSI and CD-ROM ioctls of a safe removal.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rynas_tasks::cmd::{Cmd, CmdError};

/// What a command printed and how it ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// The last non-empty line of stderr (else stdout): the reason of a failure.
    pub fn reason(&self) -> String {
        [&self.stderr, &self.stdout]
            .iter()
            .find_map(|s| s.lines().rev().map(str::trim).find(|l| !l.is_empty()))
            .map(str::to_string)
            .unwrap_or_else(|| match self.code {
                Some(c) => format!("exit code {c}"),
                None => "killed by a signal".into(),
            })
    }
}

/// The machine as the actions see it.
pub trait Host: Send + Sync {
    /// Root below which `/sys`, `/proc`, `/run/udev` are read and sysfs is written.
    fn root(&self) -> &Path;
    /// Runs `argv` with optional standard input; `Err` when it cannot start or times out.
    fn run(
        &self,
        argv: &[String],
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<Output, String>;
    /// Writes `value` to the sysfs attribute `path` (absolute, below the root).
    fn write_sysfs(&self, path: &str, value: &str) -> Result<(), String>;
    /// Writes a regular file (the SMB include) atomically.
    fn write_file(&self, path: &str, content: &str) -> Result<(), String>;
    fn make_dir(&self, path: &str) -> Result<(), String>;
    /// Removes an empty directory; anything else is left alone.
    fn remove_empty_dir(&self, path: &str);
    /// `fsync` + `BLKFLSBUF` of a block device.
    fn flush_device(&self, dev: &str) -> Result<(), String>;
    /// SCSI SYNCHRONIZE CACHE then START STOP UNIT (stop; with `eject` also load/eject).
    fn scsi_stop(&self, dev: &str, eject: bool) -> Result<(), String>;
    /// `CDROMEJECT` on an optical drive.
    fn cdrom_eject(&self, dev: &str) -> Result<(), String>;
    /// systemd is PID 1: mounts go through transient mount units (`systemd-mount`).
    fn systemd(&self) -> bool;
    /// A program is installed (`cryptsetup`, `smbcontrol`).
    fn has_program(&self, name: &str) -> bool;
}

const PATH_DIRS: [&str; 4] = ["/usr/sbin", "/usr/bin", "/sbin", "/bin"];

/// The real machine.
#[derive(Debug, Clone)]
pub struct SystemHost {
    root: PathBuf,
}

impl SystemHost {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn under(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }
}

impl Host for SystemHost {
    fn root(&self) -> &Path {
        &self.root
    }

    fn run(
        &self,
        argv: &[String],
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<Output, String> {
        let (prog, args) = argv.split_first().ok_or("empty command")?;
        let mut c = Cmd::new(prog).args(args).env_clear().timeout(timeout);
        if let Some(s) = stdin {
            c = c.stdin(s.to_vec());
        }
        match c.output_blocking() {
            Ok(o) => Ok(Output {
                code: o.code,
                stdout: o.stdout,
                stderr: o.stderr,
            }),
            Err(CmdError::Timeout(d)) => {
                Err(format!("{prog} did not finish within {} s", d.as_secs()))
            }
            Err(e) => Err(format!("{prog}: {e}")),
        }
    }

    fn write_sysfs(&self, path: &str, value: &str) -> Result<(), String> {
        std::fs::write(self.under(path), value).map_err(|e| format!("{path}: {e}"))
    }

    fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        let p = self.under(path);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
        }
        let tmp = p.with_extension("rynas-new");
        std::fs::write(&tmp, content).map_err(|e| format!("{path}: {e}"))?;
        std::fs::rename(&tmp, &p).map_err(|e| format!("{path}: {e}"))
    }

    fn make_dir(&self, path: &str) -> Result<(), String> {
        std::fs::create_dir_all(self.under(path)).map_err(|e| format!("{path}: {e}"))
    }

    fn remove_empty_dir(&self, path: &str) {
        let _ = std::fs::remove_dir(self.under(path));
    }

    fn flush_device(&self, dev: &str) -> Result<(), String> {
        sys::flush(dev)
    }

    fn scsi_stop(&self, dev: &str, eject: bool) -> Result<(), String> {
        sys::scsi_stop(dev, eject)
    }

    fn cdrom_eject(&self, dev: &str) -> Result<(), String> {
        sys::cdrom_eject(dev)
    }

    fn systemd(&self) -> bool {
        Path::new("/run/systemd/system").exists()
    }

    fn has_program(&self, name: &str) -> bool {
        PATH_DIRS.iter().any(|d| Path::new(d).join(name).exists())
    }
}

/// `--dry-system`: reads the given root, changes nothing, logs what it would do.
#[derive(Debug, Clone)]
pub struct DryHost {
    root: PathBuf,
}

impl DryHost {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl Host for DryHost {
    fn root(&self) -> &Path {
        &self.root
    }

    fn run(&self, argv: &[String], _stdin: Option<&[u8]>, _t: Duration) -> Result<Output, String> {
        tracing::info!("dry system: would run {}", argv.join(" "));
        Ok(Output {
            code: Some(0),
            ..Default::default()
        })
    }

    fn write_sysfs(&self, path: &str, value: &str) -> Result<(), String> {
        tracing::info!("dry system: would write {value:?} to {path}");
        Ok(())
    }

    fn write_file(&self, path: &str, _content: &str) -> Result<(), String> {
        tracing::info!("dry system: would write {path}");
        Ok(())
    }

    fn make_dir(&self, _path: &str) -> Result<(), String> {
        Ok(())
    }

    fn remove_empty_dir(&self, _path: &str) {}

    fn flush_device(&self, _dev: &str) -> Result<(), String> {
        Ok(())
    }

    fn scsi_stop(&self, dev: &str, eject: bool) -> Result<(), String> {
        tracing::info!("dry system: would stop {dev} (eject {eject})");
        Ok(())
    }

    fn cdrom_eject(&self, dev: &str) -> Result<(), String> {
        tracing::info!("dry system: would eject {dev}");
        Ok(())
    }

    fn systemd(&self) -> bool {
        true
    }

    fn has_program(&self, _name: &str) -> bool {
        true
    }
}

/// One call recorded by [`FakeHost`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    Run(Vec<String>, Option<Vec<u8>>),
    Sysfs(String, String),
    File(String, String),
    MakeDir(String),
    RemoveDir(String),
    Flush(String),
    ScsiStop(String, bool),
    CdromEject(String),
}

/// A recording fake for tests: commands answer from a script (`argv` prefix → output),
/// success otherwise.
#[derive(Default)]
pub struct FakeHost {
    pub root: PathBuf,
    pub calls: Mutex<Vec<Call>>,
    pub answers: Mutex<Vec<(Vec<String>, Output)>>,
    pub no_systemd: bool,
    pub missing: Vec<String>,
}

impl FakeHost {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            ..Default::default()
        }
    }

    /// Commands starting with `prefix` answer `out`.
    pub fn answer(&self, prefix: &[&str], out: Output) {
        self.answers
            .lock()
            .unwrap()
            .push((prefix.iter().map(|s| s.to_string()).collect(), out));
    }

    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    /// The command lines run, joined with spaces.
    pub fn commands(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Run(a, _) => Some(a.join(" ")),
                _ => None,
            })
            .collect()
    }

    fn push(&self, c: Call) {
        self.calls.lock().unwrap().push(c);
    }
}

impl Host for FakeHost {
    fn root(&self) -> &Path {
        &self.root
    }

    fn run(&self, argv: &[String], stdin: Option<&[u8]>, _t: Duration) -> Result<Output, String> {
        self.push(Call::Run(argv.to_vec(), stdin.map(<[u8]>::to_vec)));
        let answers = self.answers.lock().unwrap();
        Ok(answers
            .iter()
            .find(|(p, _)| argv.starts_with(p))
            .map(|(_, o)| o.clone())
            .unwrap_or(Output {
                code: Some(0),
                ..Default::default()
            }))
    }

    fn write_sysfs(&self, path: &str, value: &str) -> Result<(), String> {
        self.push(Call::Sysfs(path.into(), value.into()));
        Ok(())
    }

    fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        self.push(Call::File(path.into(), content.into()));
        Ok(())
    }

    fn make_dir(&self, path: &str) -> Result<(), String> {
        self.push(Call::MakeDir(path.into()));
        Ok(())
    }

    fn remove_empty_dir(&self, path: &str) {
        self.push(Call::RemoveDir(path.into()));
    }

    fn flush_device(&self, dev: &str) -> Result<(), String> {
        self.push(Call::Flush(dev.into()));
        Ok(())
    }

    fn scsi_stop(&self, dev: &str, eject: bool) -> Result<(), String> {
        self.push(Call::ScsiStop(dev.into(), eject));
        Ok(())
    }

    fn cdrom_eject(&self, dev: &str) -> Result<(), String> {
        self.push(Call::CdromEject(dev.into()));
        Ok(())
    }

    fn systemd(&self) -> bool {
        !self.no_systemd
    }

    fn has_program(&self, name: &str) -> bool {
        !self.missing.iter().any(|m| m == name)
    }
}

/// The ioctls of a safe removal.
mod sys {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    /// `BLKFLSBUF` from `<linux/fs.h>`.
    const BLKFLSBUF: libc::c_ulong = 0x1261;
    /// `SG_IO` from `<scsi/sg.h>`.
    const SG_IO: libc::c_ulong = 0x2285;
    /// `CDROMEJECT` from `<linux/cdrom.h>`.
    const CDROMEJECT: libc::c_ulong = 0x5309;
    const SG_DXFER_NONE: libc::c_int = -1;

    /// `struct sg_io_hdr` of `<scsi/sg.h>`.
    #[repr(C)]
    struct SgIoHdr {
        interface_id: libc::c_int,
        dxfer_direction: libc::c_int,
        cmd_len: libc::c_uchar,
        mx_sb_len: libc::c_uchar,
        iovec_count: libc::c_ushort,
        dxfer_len: libc::c_uint,
        dxferp: *mut libc::c_void,
        cmdp: *const libc::c_uchar,
        sbp: *mut libc::c_uchar,
        timeout: libc::c_uint,
        flags: libc::c_uint,
        pack_id: libc::c_int,
        usr_ptr: *mut libc::c_void,
        status: libc::c_uchar,
        masked_status: libc::c_uchar,
        msg_status: libc::c_uchar,
        sb_len_wr: libc::c_uchar,
        host_status: libc::c_ushort,
        driver_status: libc::c_ushort,
        resid: libc::c_int,
        duration: libc::c_uint,
        info: libc::c_uint,
    }

    fn open(dev: &str, write: bool) -> Result<std::fs::File, String> {
        std::fs::OpenOptions::new()
            .read(true)
            .write(write)
            .custom_flags(libc::O_NONBLOCK)
            .open(dev)
            .map_err(|e| format!("{dev}: {e}"))
    }

    pub fn flush(dev: &str) -> Result<(), String> {
        let f = open(dev, true)?;
        f.sync_all().map_err(|e| format!("fsync {dev}: {e}"))?;
        // SAFETY: BLKFLSBUF takes no argument.
        let r = unsafe { libc::ioctl(f.as_raw_fd(), BLKFLSBUF as _, 0) };
        if r != 0 {
            return Err(format!("flush {dev}: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }

    fn scsi(f: &std::fs::File, cdb: &[u8], timeout_ms: u32) -> Result<(), String> {
        let mut sense = [0u8; 32];
        let mut h = SgIoHdr {
            interface_id: b'S' as libc::c_int,
            dxfer_direction: SG_DXFER_NONE,
            cmd_len: cdb.len() as u8,
            mx_sb_len: sense.len() as u8,
            iovec_count: 0,
            dxfer_len: 0,
            dxferp: std::ptr::null_mut(),
            cmdp: cdb.as_ptr(),
            sbp: sense.as_mut_ptr(),
            timeout: timeout_ms,
            flags: 0,
            pack_id: 0,
            usr_ptr: std::ptr::null_mut(),
            status: 0,
            masked_status: 0,
            msg_status: 0,
            sb_len_wr: 0,
            host_status: 0,
            driver_status: 0,
            resid: 0,
            duration: 0,
            info: 0,
        };
        // SAFETY: `h` is a valid sg_io_hdr whose pointers (cdb, sense) outlive the call;
        // no data is transferred.
        let r = unsafe { libc::ioctl(f.as_raw_fd(), SG_IO as _, &mut h) };
        if r != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        if h.status != 0 || h.host_status != 0 || h.driver_status != 0 {
            return Err(format!(
                "SCSI status {:#x}, host {:#x}, driver {:#x}",
                h.status, h.host_status, h.driver_status
            ));
        }
        Ok(())
    }

    /// SYNCHRONIZE CACHE (10), then START STOP UNIT: stop the motor, and with `eject`
    /// unload the medium (what `udisksctl power-off` and `eject` send).
    pub fn scsi_stop(dev: &str, eject: bool) -> Result<(), String> {
        let f = open(dev, false)?;
        let sync = [0x35u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let r1 = scsi(&f, &sync, 60_000);
        let stop = [0x1bu8, 0, 0, 0, if eject { 0x02 } else { 0x00 }, 0];
        let r2 = scsi(&f, &stop, 30_000);
        match (r1, r2) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(e), _) => Err(format!("SYNCHRONIZE CACHE on {dev}: {e}")),
            (_, Err(e)) => Err(format!("START STOP UNIT on {dev}: {e}")),
        }
    }

    pub fn cdrom_eject(dev: &str) -> Result<(), String> {
        let f = open(dev, false)?;
        // SAFETY: CDROMEJECT takes no argument.
        let r = unsafe { libc::ioctl(f.as_raw_fd(), CDROMEJECT as _, 0) };
        if r != 0 {
            return Err(format!("eject {dev}: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }
}
