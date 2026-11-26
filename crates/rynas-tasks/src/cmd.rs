//! External commands: argv only (never a shell), timeout, own process group, output captured.
//! The one runner of rynas (guidelines M3): tasks, configuration appliers, file services and
//! the storage inventory all start their commands through [`Cmd`].
//!
//! Every command runs in a new process group so that a timeout or a cancellation can stop
//! the whole tree it started (`SIGTERM` to the group, `SIGKILL` after a grace period).
//! `LC_ALL=C` keeps the output parseable.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::process::Stdio;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

/// Default timeout of a command.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// Time between `SIGTERM` and `SIGKILL`.
const KILL_GRACE: Duration = Duration::from_secs(3);
/// Captured stdout/stderr are cut at this size each.
const CAPTURE_CAP: usize = 1 << 20;
/// `ioprio_set(2)`: the calling process...
const IOPRIO_WHO_PROCESS: libc::c_int = 1;
/// ...in the idle class (`IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT`).
const IOPRIO_IDLE: libc::c_int = 3 << 13;
/// `PATH` of a child started with [`Cmd::env_clear`].
const CHILD_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// A command line to run.
#[derive(Clone)]
pub struct Cmd {
    argv: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    clear_env: bool,
    stdin: Option<StdinData>,
    timeout: Duration,
    /// `RuntimeMaxSec` of the transient unit the command runs in ([`Outside::cmd`]): systemd
    /// stops the unit then, and the client exits with a plain failure code.
    runtime_max: Option<Duration>,
    /// Over the capture cap, the end of each stream is kept instead of its start.
    tail: bool,
    /// Lowest CPU and I/O priority ([`Cmd::heavy`]).
    low: bool,
    /// Stopped when nothing happened for this long ([`Cmd::stall_timeout`]).
    stall: Option<(Duration, Activity)>,
}

/// Last sign of life of a long command, for its stall watchdog ([`Cmd::stall_watch`]): every
/// output line counts, and the caller bumps it when it sees progress another way (a status
/// file read now and then, bytes written). Cheap to clone; the clones share the time.
#[derive(Clone, Debug)]
pub struct Activity(std::sync::Arc<std::sync::atomic::AtomicU64>);

fn mono_ms() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

impl Default for Activity {
    fn default() -> Self {
        Self(std::sync::Arc::new(std::sync::atomic::AtomicU64::new(mono_ms())))
    }
}

impl Activity {
    pub fn new() -> Self {
        Self::default()
    }

    /// Something happened now.
    pub fn bump(&self) {
        self.0.store(mono_ms(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Time since the last sign of life.
    pub fn idle(&self) -> Duration {
        Duration::from_millis(mono_ms().saturating_sub(self.0.load(std::sync::atomic::Ordering::Relaxed)))
    }
}

/// A reader that bumps an [`Activity`] at every chunk (pipe readers of the blocking runner).
struct Bumping<R> {
    r: R,
    act: Option<Activity>,
}

impl<R: Read> Read for Bumping<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.r.read(buf)?;
        if n > 0
            && let Some(a) = &self.act
        {
            a.bump();
        }
        Ok(n)
    }
}

/// Bytes for a child's standard input (passwords, keys): overwritten with zeros when dropped,
/// copies included.
#[derive(Clone)]
struct StdinData(Vec<u8>);

impl Drop for StdinData {
    fn drop(&mut self) {
        for b in self.0.iter_mut() {
            // SAFETY: a valid, aligned pointer to a byte of this vector.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

/// Standard input carries secrets (passwords for `chpasswd`, `smbpasswd`...) and environment
/// values may too: `Debug` shows only their size and the variable names.
impl std::fmt::Debug for Cmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cmd")
            .field("argv", &self.argv)
            .field(
                "env",
                &self
                    .env
                    .iter()
                    .map(|(k, _)| format!("{}=<redacted>", k.to_string_lossy()))
                    .collect::<Vec<_>>(),
            )
            .field("clear_env", &self.clear_env)
            .field(
                "stdin",
                &self
                    .stdin
                    .as_ref()
                    .map(|b| format!("<{} bytes redacted>", b.0.len())),
            )
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// What a command printed and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdOutput {
    /// Exit code; `None` when killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

#[derive(Debug)]
pub enum CmdError {
    /// The program could not be started.
    Spawn(std::io::Error),
    Timeout(Duration),
    Cancelled,
    /// It ran but exited with an error ([`Cmd::run_checked`] only).
    Failed(CmdOutput),
    /// No output and no progress for this long ([`Cmd::stall_timeout`]): stopped.
    Stalled(Duration),
}

impl std::fmt::Display for CmdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CmdError::Spawn(e) => write!(f, "cannot start: {e}"),
            CmdError::Timeout(d) => write!(f, "timed out after {} s", d.as_secs_f32()),
            CmdError::Cancelled => write!(f, "cancelled"),
            CmdError::Stalled(d) => write!(f, "stopped: no progress for {} s", d.as_secs()),
            CmdError::Failed(o) => {
                let last = o.stderr.lines().rev().find(|l| !l.trim().is_empty());
                match (o.code, last) {
                    (Some(c), Some(l)) => write!(f, "exit code {c}: {l}"),
                    (Some(c), None) => write!(f, "exit code {c}"),
                    (None, _) => write!(f, "killed by a signal"),
                }
            }
        }
    }
}

impl std::error::Error for CmdError {}

impl Cmd {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            argv: vec![program.as_ref().to_owned()],
            env: Vec::new(),
            clear_env: false,
            stdin: None,
            timeout: DEFAULT_TIMEOUT,
            runtime_max: None,
            tail: false,
            low: false,
            stall: None,
        }
    }

    /// Heavy work (a tree walk, a checksum, a copy, a measure): the child runs at the lowest
    /// priorities, `nice 19` and the idle I/O class (it gets the disk only when nobody else
    /// wants it), set in the child before it starts, so it never slows the NAS's
    /// interactive work. Most heavy commands also want [`Cmd::stall_timeout`] instead of a
    /// fixed timeout; rynasd's `heavy::heavy_cmd` sets both.
    pub fn heavy(mut self) -> Self {
        self.low = true;
        self
    }

    /// Stopped ([`CmdError::Stalled`]) once neither output nor progress was seen for `d`:
    /// work of unknown length is never cut at a fixed time while it still moves. The fixed
    /// [`Cmd::timeout`] still applies (make it long). Output lines count as progress.
    pub fn stall_timeout(self, d: Duration) -> Self {
        self.stall_watch(d, Activity::new())
    }

    /// [`Cmd::stall_timeout`] with an [`Activity`] the caller bumps when it sees progress
    /// another way (the command is quiet, its progress is read elsewhere).
    pub fn stall_watch(mut self, d: Duration, activity: Activity) -> Self {
        self.stall = Some((d, activity));
        self
    }

    /// Keeps the end of the captured output when it is over the cap, not its start (a log
    /// whose last lines are the ones that matter).
    pub fn capture_tail(mut self) -> Self {
        self.tail = true;
        self
    }

    /// A failure after the unit's `RuntimeMaxSec` is systemd stopping it: a timeout, not
    /// whatever exit code the client reported.
    fn unit_timeout(&self, started: Instant, code: Option<i32>) -> Option<CmdError> {
        self.runtime_max
            .filter(|max| code != Some(0) && started.elapsed() >= *max)
            .map(CmdError::Timeout)
    }

    pub fn arg(mut self, a: impl AsRef<OsStr>) -> Self {
        self.argv.push(a.as_ref().to_owned());
        self
    }

    pub fn args<I: IntoIterator<Item = S>, S: AsRef<OsStr>>(mut self, args: I) -> Self {
        self.argv
            .extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    pub fn env(mut self, k: impl AsRef<OsStr>, v: impl AsRef<OsStr>) -> Self {
        self.env
            .push((k.as_ref().to_owned(), v.as_ref().to_owned()));
        self
    }

    /// Start from an empty environment: only `LC_ALL=C`, a fixed `PATH` and what
    /// [`Cmd::env`] adds reach the child (no inherited secrets, predictable output).
    pub fn env_clear(mut self) -> Self {
        self.clear_env = true;
        self
    }

    /// Bytes written to the standard input (otherwise `/dev/null`).
    pub fn stdin(mut self, data: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(StdinData(data.into()));
        self
    }

    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    pub fn program(&self) -> &OsStr {
        &self.argv[0]
    }

    /// Program and arguments, for running the same command elsewhere (a task unit runs
    /// what rynasd planned: `rynasctl job run`).
    pub fn argv_lossy(&self) -> Vec<String> {
        self.argv.iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    /// The variables set with [`Cmd::env`] (the caller decides whether they may leave the
    /// process: they can carry secrets).
    pub fn env_lossy(&self) -> Vec<(String, String)> {
        self.env
            .iter()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
            .collect()
    }

    /// The bytes given with [`Cmd::stdin`] (simulations read them; never log them: they can
    /// carry secrets).
    pub fn stdin_data(&self) -> Option<&[u8]> {
        self.stdin.as_ref().map(|d| d.0.as_slice())
    }

    /// The command line for logs (arguments are not quoted: never feed it to a shell).
    pub fn display(&self) -> String {
        self.argv
            .iter()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn std_command(&self) -> std::process::Command {
        use std::os::unix::process::CommandExt;
        let mut c = std::process::Command::new(&self.argv[0]);
        if self.clear_env {
            c.env_clear().env("PATH", CHILD_PATH);
        }
        c.args(&self.argv[1..])
            .env("LC_ALL", "C")
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(if self.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        if self.low {
            // SAFETY: only async-signal-safe system calls between fork and exec; a failure
            // leaves the default priorities (never worse than without `heavy`)
            unsafe {
                c.pre_exec(|| {
                    libc::setpriority(libc::PRIO_PROCESS as _, 0, 19);
                    libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, IOPRIO_IDLE);
                    Ok(())
                });
            }
        }
        c
    }

    /// Run to the end. `line` receives every output line as it arrives (stderr lines are
    /// prefixed with "stderr: "); `cancel` stops the command early.
    pub async fn run(
        &self,
        cancel: Option<&CancellationToken>,
        line: Option<&(dyn Fn(&str) + Sync)>,
    ) -> Result<CmdOutput, CmdError> {
        let started = Instant::now();
        let mut child = tokio::process::Command::from(self.std_command())
            .kill_on_drop(true)
            .spawn()
            .map_err(CmdError::Spawn)?;
        let pgid = child.id().map(|p| Pid::from_raw(p as i32));
        if let (Some(data), Some(mut w)) = (self.stdin.clone(), child.stdin.take()) {
            tokio::spawn(async move {
                let _ = w.write_all(&data.0).await;
            });
        }
        let out = child.stdout.take().expect("piped stdout");
        let err = child.stderr.take().expect("piped stderr");
        let never = CancellationToken::new();
        let cancel = cancel.unwrap_or(&never);

        let act = self.stall.as_ref().map(|s| s.1.clone());
        if let Some(a) = &act {
            a.bump();
        }
        let readers = async {
            tokio::join!(
                read_lines(out, "", line, self.tail, act.as_ref()),
                read_lines(err, "stderr: ", line, self.tail, act.as_ref())
            )
        };
        let stall = self.stall.clone();
        let stalled = async move {
            let Some((d, a)) = stall else { return std::future::pending().await };
            loop {
                let idle = a.idle();
                if idle >= d {
                    return CmdError::Stalled(d);
                }
                tokio::time::sleep(d - idle).await;
            }
        };
        let (status, (stdout, stderr)) = tokio::join!(
            async {
                let waited = tokio::select! {
                    s = child.wait() => Ok(s),
                    _ = tokio::time::sleep(self.timeout) => Err(CmdError::Timeout(self.timeout)),
                    _ = cancel.cancelled() => Err(CmdError::Cancelled),
                    e = stalled => Err(e),
                };
                match waited {
                    Ok(s) => Ok(s),
                    Err(e) => {
                        if let Some(g) = pgid {
                            let _ = killpg(g, Signal::SIGTERM);
                            if tokio::time::timeout(KILL_GRACE, child.wait())
                                .await
                                .is_err()
                            {
                                let _ = killpg(g, Signal::SIGKILL);
                            }
                        }
                        let _ = child.wait().await;
                        if let Some(g) = pgid {
                            // leftovers of the group that ignored SIGTERM
                            let _ = killpg(g, Signal::SIGKILL);
                        }
                        Err(e)
                    }
                }
            },
            readers
        );
        let status = status?.map_err(CmdError::Spawn)?;
        if let Some(e) = self.unit_timeout(started, status.code()) {
            return Err(e);
        }
        Ok(CmdOutput {
            code: status.code(),
            stdout,
            stderr,
        })
    }

    /// Like [`Cmd::run`], but a non-zero exit is an error.
    pub async fn run_checked(
        &self,
        cancel: Option<&CancellationToken>,
        line: Option<&(dyn Fn(&str) + Sync)>,
    ) -> Result<CmdOutput, CmdError> {
        let o = self.run(cancel, line).await?;
        if o.success() {
            Ok(o)
        } else {
            Err(CmdError::Failed(o))
        }
    }

    /// Synchronous variant for code that is not async (e.g. a configuration apply running on
    /// a blocking thread). A non-zero exit is an error.
    pub fn run_blocking(&self) -> Result<CmdOutput, CmdError> {
        self.run_blocking_in(None)
    }

    /// [`Cmd::run_blocking`] that stops the command when `cancel` fires (a task's token, on
    /// the task's blocking thread: [`crate::TaskCtx::run_blocking`]).
    pub fn run_blocking_in(&self, cancel: Option<&CancellationToken>) -> Result<CmdOutput, CmdError> {
        let o = self.output_blocking_in(cancel)?;
        if o.success() {
            Ok(o)
        } else {
            Err(CmdError::Failed(o))
        }
    }

    /// Synchronous run that returns the output whatever the exit code (tools such as
    /// smartctl report findings in it). Same guarantees as [`Cmd::run`]: own process group,
    /// `SIGTERM` then `SIGKILL` to the group on timeout, output capped at 1 MiB per stream.
    pub fn output_blocking(&self) -> Result<CmdOutput, CmdError> {
        self.output_blocking_in(None)
    }

    /// [`Cmd::output_blocking`] that stops the command when `cancel` fires.
    pub fn output_blocking_in(&self, cancel: Option<&CancellationToken>) -> Result<CmdOutput, CmdError> {
        use std::io::Write;
        let started = Instant::now();
        let mut child = self.std_command().spawn().map_err(CmdError::Spawn)?;
        let pgid = Pid::from_raw(child.id() as i32);
        if let (Some(data), Some(mut w)) = (self.stdin.clone(), child.stdin.take()) {
            std::thread::spawn(move || {
                let _ = w.write_all(&data.0);
            });
        }
        let mut out = child.stdout.take().expect("piped stdout");
        let mut err = child.stderr.take().expect("piped stderr");
        // both pipes are drained on small helper threads (reading one here would block the
        // timeout check); this thread waits for the exit
        let tail = self.tail;
        let act = self.stall.as_ref().map(|s| s.1.clone());
        if let Some(a) = &act {
            a.bump();
        }
        let (a_out, a_err) = (act.clone(), act);
        let t_out = small_thread(move || capture(&mut Bumping { r: &mut out, act: a_out }, tail));
        let t_err = small_thread(move || capture(&mut Bumping { r: &mut err, act: a_err }, tail));
        let deadline = Instant::now() + self.timeout;
        // short commands end in a few ms: poll often at first, then back off
        let mut nap = Duration::from_millis(1);
        let status = loop {
            match child.try_wait().map_err(CmdError::Spawn)? {
                Some(s) => break Ok(s),
                None if Instant::now() >= deadline => break Err(CmdError::Timeout(self.timeout)),
                None if cancel.is_some_and(CancellationToken::is_cancelled) => break Err(CmdError::Cancelled),
                None if self.stall.as_ref().is_some_and(|(d, a)| a.idle() >= *d) => {
                    break Err(CmdError::Stalled(self.stall.as_ref().map(|s| s.0).unwrap_or_default()));
                }
                None => {
                    std::thread::sleep(nap.min(deadline.saturating_duration_since(Instant::now())));
                    // a long command is looked at twice a second (cancel, stall), not 20 times
                    // (other blocking runs keep their 50 ms: callers wait for their output)
                    // (only work that is long by nature: heavy, watched for stalls, cancellable)
                    let long = self.low || self.stall.is_some() || cancel.is_some();
                    let cap = if long && started.elapsed() >= Duration::from_secs(2) { 500 } else { 50 };
                    nap = (nap * 2).min(Duration::from_millis(cap));
                }
            }
        };
        let status = match status {
            Ok(s) => s,
            Err(e) => {
                let _ = killpg(pgid, Signal::SIGTERM);
                let end = Instant::now() + KILL_GRACE;
                while Instant::now() < end && matches!(child.try_wait(), Ok(None)) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                let _ = killpg(pgid, Signal::SIGKILL);
                let _ = child.wait();
                return Err(e);
            }
        };
        if let Some(e) = self.unit_timeout(started, status.code()) {
            return Err(e);
        }
        Ok(CmdOutput {
            code: status.code(),
            stdout: t_out.join().unwrap_or_default(),
            stderr: t_err.join().unwrap_or_default(),
        })
    }
}

/// Grace the `systemd-run` client gets past a unit's `RuntimeMaxSec` to report its end.
const OUTSIDE_GRACE: Duration = Duration::from_secs(10);

/// A command run in a transient systemd service, outside the caller's sandbox.
///
/// rynasd runs with `ProtectSystem=yes` (`/usr` read-only), a seccomp filter without the mount
/// calls (`SystemCallFilter=@system-service`: `mount`, `umount2` fail with `EPERM`) and a mount
/// namespace of its own. What writes `/usr` (apt, dpkg) or mounts and unmounts goes through
/// `systemd-run --wait --pipe --collect`: the unit runs with the default service environment
/// in the host's mount namespace, `--pipe` streams its output back and returns its exit status.
/// A mount made there propagates into rynasd's namespace (systemd sets it up as a slave of the
/// host's), so rynasd works in it afterwards; an unmount propagates the same way.
///
/// The unit is `<unit>-<seconds>-<pid>-<n>.service` (see [`transient_unit`]).
#[derive(Debug, Clone, Copy)]
pub struct Outside<'a> {
    unit: &'a str,
    description: &'a str,
    timeout: Duration,
    runtime_max: bool,
    setenv: &'a [&'a str],
    properties: &'a [&'a str],
}

impl<'a> Outside<'a> {
    /// `unit`: prefix of the unit name (`rynas-rollback`); `description`: what `systemctl`
    /// shows (`rynas: …`); `timeout`: the unit is stopped by systemd after it
    /// (`RuntimeMaxSec`), the client waits [`OUTSIDE_GRACE`] longer.
    pub fn new(unit: &'a str, description: &'a str, timeout: Duration) -> Self {
        Self {
            unit,
            description,
            timeout,
            runtime_max: true,
            setenv: &[],
            properties: &[],
        }
    }

    /// No `RuntimeMaxSec`: the unit runs to its end even when the client gives up after the
    /// timeout (an interrupted dpkg is worse than a finished upgrade).
    pub fn keep_running(mut self) -> Self {
        self.runtime_max = false;
        self
    }

    /// Environment of the unit, `NAME=value` each (`--setenv`).
    pub fn setenv(mut self, vars: &'a [&'a str]) -> Self {
        self.setenv = vars;
        self
    }

    /// Extra unit properties, `Name=value` each (`--property`).
    pub fn properties(mut self, props: &'a [&'a str]) -> Self {
        self.properties = props;
        self
    }

    /// `argv` (program first) in a new transient unit.
    pub fn cmd<I: IntoIterator<Item = S>, S: AsRef<OsStr>>(&self, argv: I) -> Cmd {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut c = Cmd::new("systemd-run")
            .args(["--wait", "--pipe", "--collect", "--quiet", "--service-type=exec"])
            .arg(format!(
                "--unit={}-{secs}-{}-{}",
                self.unit,
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ))
            .arg(format!("--description={}", self.description));
        let mut timeout = self.timeout;
        if self.runtime_max {
            let max = self.timeout.as_secs().max(1);
            c = c.arg(format!("--property=RuntimeMaxSec={max}"));
            c.runtime_max = Some(Duration::from_secs(max));
            timeout += OUTSIDE_GRACE;
        }
        for p in self.properties {
            c = c.arg(format!("--property={p}"));
        }
        for v in self.setenv {
            c = c.arg(format!("--setenv={v}"));
        }
        c.arg("--").args(argv).env_clear().timeout(timeout)
    }

    /// `argv` as a long-lived service named exactly `<unit>.service` (`unit` is the full
    /// name, not a prefix), `Type=forking`: `systemd-run` returns once the program's first
    /// process has exited, i.e. when a self-daemonizing program (gocryptfs without `-fg`)
    /// has finished its start-up; a failed start makes it exit non-zero and leaves the
    /// failed unit for `systemctl show -p ExecMainStatus`. No `--wait`, no `--pipe`, no
    /// `--collect`, no `RuntimeMaxSec` (the service runs until it is stopped); `timeout`
    /// bounds the client. `${VAR}` in `argv` is expanded by systemd (`--expand-environment`,
    /// the default since systemd 254), e.g. `${CREDENTIALS_DIRECTORY}`.
    pub fn daemon<I: IntoIterator<Item = S>, S: AsRef<OsStr>>(&self, argv: I) -> Cmd {
        let mut c = Cmd::new("systemd-run")
            .args(["--quiet", "--service-type=forking"])
            .arg(format!("--unit={}", self.unit))
            .arg(format!("--description={}", self.description));
        for p in self.properties {
            c = c.arg(format!("--property={p}"));
        }
        for v in self.setenv {
            c = c.arg(format!("--setenv={v}"));
        }
        c.arg("--").args(argv).env_clear().timeout(self.timeout + OUTSIDE_GRACE)
    }
}

/// The transient unit (`<name>.service`) a command made by [`Outside::cmd`] with the unit
/// prefix `unit` runs in; `None` for any other command.
pub fn transient_unit(cmd: &Cmd, unit: &str) -> Option<String> {
    if cmd.program() != "systemd-run" {
        return None;
    }
    let want = format!("--unit={unit}-");
    cmd.argv
        .iter()
        .take_while(|a| a.as_os_str() != "--")
        .filter_map(|a| a.to_str())
        .find(|a| a.starts_with(&want))
        .map(|a| format!("{}.service", &a["--unit=".len()..]))
}

/// Unit prefix of the transient units made by [`crypt_cmd`].
pub const CRYPT_UNIT: &str = "rynas-luks";

/// `cryptsetup` actions that derive a key from a passphrase (Argon2: hundreds of MiB).
const KDF_ACTIONS: [&str; 11] = [
    "luksFormat",
    "open",
    "luksOpen",
    "luksAddKey",
    "luksChangeKey",
    "luksConvertKey",
    "luksRemoveKey",
    "luksKillSlot",
    "luksResume",
    "resize",
    "reencrypt",
];

/// `argv` runs a LUKS key derivation: `cryptsetup` with one of [`KDF_ACTIONS`], or
/// `systemd-cryptenroll` (it unlocks a key slot with the passphrase before enrolling).
pub fn kdf_heavy<S: AsRef<str>>(argv: &[S]) -> bool {
    match argv.first().map(AsRef::as_ref) {
        Some("systemd-cryptenroll") => true,
        Some("cryptsetup") => argv[1..].iter().any(|a| KDF_ACTIONS.contains(&a.as_ref())),
        _ => false,
    }
}

/// `argv` (program first, never a shell) as a [`Cmd`] with the environment cleared and
/// `timeout`. A LUKS key derivation ([`kdf_heavy`]) runs in a transient unit
/// (`rynas-luks-*`, [`Outside`]) when `systemd` is PID 1: Argon2 with `--pbkdf-memory 262144`
/// (or a disk formatted elsewhere, up to 1 GiB and more) needs more memory than rynasd's
/// `daemon/` cgroup allows (`memory.high` 96M) and would stall there forever in memory
/// reclaim. `--pipe` hands the unit the client's standard input, so a key on stdin
/// (`--key-file -`, `--unlock-key-file=/dev/stdin`) reaches it, and the client exits with the
/// unit's exit code (cryptsetup's 2 = wrong passphrase stays visible). Without systemd
/// (containers, recovery shells) the command runs directly. Find the unit to stop on a cancel
/// with [`transient_unit`] and [`CRYPT_UNIT`].
pub fn crypt_cmd<S: AsRef<str>>(argv: &[S], timeout: Duration, systemd: bool) -> Cmd {
    let args = argv.iter().map(AsRef::as_ref);
    if systemd && kdf_heavy(argv) {
        Outside::new(CRYPT_UNIT, "rynas: LUKS key derivation", timeout).cmd(args)
    } else {
        let mut it = args;
        Cmd::new(it.next().unwrap_or_default())
            .args(it)
            .env_clear()
            .timeout(timeout)
    }
}

/// A pipe reader: tiny stack, it only copies bytes.
fn small_thread<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> std::thread::JoinHandle<T> {
    std::thread::Builder::new()
        .name("cmd-pipe".into())
        .stack_size(64 << 10)
        .spawn(f)
        .expect("spawning a pipe reader")
}

fn capture(r: &mut impl Read, tail: bool) -> String {
    if tail {
        return capture_end(r);
    }
    let mut buf = Vec::new();
    let _ = r.take(CAPTURE_CAP as u64).read_to_end(&mut buf);
    // drain the rest so the child never blocks on a full pipe
    let _ = std::io::copy(r, &mut std::io::sink());
    String::from_utf8_lossy(&buf).into_owned()
}

/// The last [`CAPTURE_CAP`] bytes of a stream, read to its end; cut at a line start when
/// something was dropped.
fn capture_end(r: &mut impl Read) -> String {
    let mut buf: Vec<u8> = Vec::new();
    // on the heap: pipe readers have a tiny stack
    let mut chunk = vec![0u8; 16 << 10];
    let mut dropped = false;
    loop {
        match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
        if buf.len() > 2 * CAPTURE_CAP {
            buf.drain(..buf.len() - CAPTURE_CAP);
            dropped = true;
        }
    }
    if buf.len() > CAPTURE_CAP {
        buf.drain(..buf.len() - CAPTURE_CAP);
        dropped = true;
    }
    if dropped && let Some(nl) = buf.iter().position(|&b| b == b'\n') {
        buf.drain(..=nl);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

async fn read_lines(
    r: impl AsyncRead + Unpin,
    prefix: &str,
    sink: Option<&(dyn Fn(&str) + Sync)>,
    tail: bool,
    act: Option<&Activity>,
) -> String {
    let mut reader = BufReader::new(r);
    let mut captured = String::new();
    let emit = |line: &[u8], cut: bool, captured: &mut String| {
        let l = String::from_utf8_lossy(line);
        let l = l.trim_end_matches('\r');
        let mark = if cut { " …" } else { "" };
        if let Some(s) = sink {
            s(&format!("{prefix}{l}{mark}"));
        }
        if tail || captured.len() < CAPTURE_CAP {
            captured.push_str(l);
            captured.push_str(mark);
            captured.push('\n');
        }
        // the end is what is kept: the start goes, now and then (not at every line)
        if tail && captured.len() > 2 * CAPTURE_CAP {
            keep_end(captured);
        }
    };
    // a line is cut at MAX_LINE while it is read: a program that never prints a newline
    // cannot make the reader hold all of its output
    let mut line: Vec<u8> = Vec::new();
    let mut cut = false;
    loop {
        let buf = match reader.fill_buf().await {
            Ok(b) if !b.is_empty() => b,
            _ => break,
        };
        if let Some(a) = act {
            a.bump();
        }
        let (take, newline) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (i, true),
            None => (buf.len(), false),
        };
        let room = MAX_LINE.saturating_sub(line.len());
        if take > room {
            cut = true;
        }
        line.extend_from_slice(&buf[..take.min(room)]);
        reader.consume(take + usize::from(newline));
        if newline {
            emit(&line, cut, &mut captured);
            line.clear();
            cut = false;
        }
    }
    if !line.is_empty() || cut {
        emit(&line, cut, &mut captured);
    }
    if tail && captured.len() > CAPTURE_CAP {
        keep_end(&mut captured);
    }
    captured
}

/// Cuts `s` to its last [`CAPTURE_CAP`] bytes, from a line start.
fn keep_end(s: &mut String) {
    let mut at = s.len().saturating_sub(CAPTURE_CAP);
    while !s.is_char_boundary(at) {
        at += 1;
    }
    if at > 0 {
        at = s[at..].find('\n').map_or(s.len(), |nl| at + nl + 1);
    }
    s.drain(..at);
}

/// Longest output line kept (the rest of the line is dropped while reading).
const MAX_LINE: usize = 64 << 10;

#[cfg(test)]
mod tests {
    #[test]
    fn outside_builds_the_systemd_run_line() {
        let c = super::Outside::new("rynas-rollback", "rynas: rollback", super::Duration::from_secs(30))
            .cmd(["mount", "-o", "bind,ro", "/a b", "/run/x"]);
        let argv: Vec<String> = c.argv.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(argv[..6], ["systemd-run", "--wait", "--pipe", "--collect", "--quiet", "--service-type=exec"]);
        let unit = argv[6].strip_prefix("--unit=rynas-rollback-").expect("unit");
        let parts: Vec<&str> = unit.split('-').collect();
        assert_eq!(parts.len(), 3, "{unit}");
        assert!(parts.iter().all(|p| p.parse::<u64>().is_ok()), "{unit}");
        assert_eq!(
            argv[7..],
            ["--description=rynas: rollback", "--property=RuntimeMaxSec=30", "--", "mount", "-o", "bind,ro", "/a b", "/run/x"]
        );
        // the client outlives the unit's own limit, with no inherited environment
        assert_eq!(c.timeout, super::Duration::from_secs(40));
        assert!(c.clear_env);
        // a second command gets a unit of its own
        let d = super::Outside::new("rynas-rollback", "r", super::Duration::from_secs(1)).cmd(["true"]);
        assert_ne!(super::transient_unit(&c, "rynas-rollback"), super::transient_unit(&d, "rynas-rollback"));
        assert_eq!(
            super::transient_unit(&c, "rynas-rollback"),
            Some(format!("{}.service", &argv[6]["--unit=".len()..]))
        );
        assert_eq!(super::transient_unit(&c, "rynas-apt"), None);
        assert_eq!(super::transient_unit(&super::Cmd::new("mount"), "rynas-rollback"), None);
    }

    #[test]
    fn luks_key_derivations_leave_the_cgroup_only_under_systemd() {
        let s = |v: &super::Cmd| -> Vec<String> { v.argv.iter().map(|a| a.to_string_lossy().into_owned()).collect() };
        let t = super::Duration::from_secs(600);
        for heavy in [
            &["cryptsetup", "luksFormat", "--pbkdf-memory", "262144", "--key-file", "-", "/dev/md/vol1"][..],
            &["cryptsetup", "open", "--key-file", "-", "/dev/md/vol1", "rynas-vol1"],
            &["cryptsetup", "resize", "--key-file", "-", "rynas-vol1"],
            &["cryptsetup", "luksAddKey", "--batch-mode", "--key-file", "-", "/dev/sdb1"],
            &["systemd-cryptenroll", "--recovery-key", "--unlock-key-file=/dev/stdin", "/dev/md/vol1"],
        ] {
            assert!(super::kdf_heavy(heavy), "{heavy:?}");
            let c = super::crypt_cmd(heavy, t, true);
            let argv = s(&c);
            assert_eq!(argv[..3], ["systemd-run", "--wait", "--pipe"]);
            assert!(super::transient_unit(&c, super::CRYPT_UNIT).is_some());
            let tail = &argv[argv.iter().position(|a| a == "--").unwrap() + 1..];
            assert_eq!(tail, heavy);
            assert!(argv.contains(&"--property=RuntimeMaxSec=600".to_string()));
            assert!(c.clear_env);
            // no systemd: the plain command
            let p = super::crypt_cmd(heavy, t, false);
            assert_eq!(s(&p), heavy);
            assert_eq!(p.timeout, t);
            assert!(p.clear_env);
        }
        for light in [
            &["cryptsetup", "close", "rynas-vol1"][..],
            &["cryptsetup", "luksUUID", "/dev/md/vol1"],
            &["cryptsetup", "luksHeaderBackup", "/dev/md/vol1", "--header-backup-file", "/run/x.img"],
            &["cryptsetup", "status", "rynas-vol1"],
            &["mdadm", "--detail", "open"],
        ] {
            assert!(!super::kdf_heavy(light), "{light:?}");
            assert_eq!(s(&super::crypt_cmd(light, t, true)), light);
        }
    }

    #[tokio::test]
    async fn long_lines_are_cut_while_reading() {
        let c = super::Cmd::new("sh").args(["-c", "head -c 200000 /dev/zero | tr '\\0' a; echo; echo next; printf tail"]);
        let seen = std::sync::Mutex::new(Vec::<usize>::new());
        let sink = |l: &str| seen.lock().unwrap().push(l.len());
        let o = c.run(None, Some(&sink)).await.unwrap();
        let lines: Vec<&str> = o.stdout.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].len(), super::MAX_LINE + " …".len());
        assert_eq!(lines[1..], ["next", "tail"]);
        assert_eq!(seen.lock().unwrap()[1..], [4, 4]);
    }

    #[test]
    fn outside_options() {
        let c = super::Outside::new("rynas-apt", "rynas: apt-get", super::Duration::from_secs(600))
            .keep_running()
            .properties(&["Nice=5"])
            .setenv(&["LC_ALL=C"])
            .cmd(["apt-get", "-y", "upgrade"]);
        let s = c.display();
        assert!(!s.contains("RuntimeMaxSec"), "{s}");
        assert!(s.ends_with("--property=Nice=5 --setenv=LC_ALL=C -- apt-get -y upgrade"), "{s}");
        assert_eq!(c.timeout, super::Duration::from_secs(600));
        // a program argument that looks like a unit is not taken for one
        let d = super::Outside::new("x", "x", super::Duration::from_secs(1)).cmd(["echo", "--unit=rynas-apt-1"]);
        assert_eq!(super::transient_unit(&d, "rynas-apt"), None);
    }

    /// systemd stops a transient unit at its `RuntimeMaxSec` and the client exits non-zero:
    /// that is reported as a timeout, not as "exit code 1".
    #[tokio::test]
    async fn a_unit_stopped_at_its_runtime_max_is_a_timeout() {
        let c = super::Outside::new("rynas-luks", "rynas: test", super::Duration::from_secs(600))
            .cmd(["true"]);
        assert_eq!(c.runtime_max, Some(super::Duration::from_secs(600)));
        assert_eq!(super::Outside::new("x", "y", super::Duration::from_secs(5)).keep_running().cmd(["true"]).runtime_max, None);
        // the same check on a local command standing for the client
        let mut c = Cmd::new("sh").args(["-c", "sleep 0.3; exit 1"]);
        c.runtime_max = Some(super::Duration::from_millis(100));
        let e = c.run_checked(None, None).await.unwrap_err();
        assert!(matches!(e, CmdError::Timeout(_)), "{e}");
        assert!(e.to_string().starts_with("timed out"), "{e}");
        assert!(matches!(c.output_blocking(), Err(CmdError::Timeout(_))));
        // a failure before the limit keeps its exit code, a success is a success
        let mut c = Cmd::new("sh").args(["-c", "exit 2"]);
        c.runtime_max = Some(super::Duration::from_secs(60));
        assert!(matches!(c.run_checked(None, None).await, Err(CmdError::Failed(_))));
        let mut c = Cmd::new("sh").args(["-c", "sleep 0.2"]);
        c.runtime_max = Some(super::Duration::from_millis(50));
        assert!(c.run_checked(None, None).await.is_ok());
    }

    #[test]
    fn outside_daemon_is_a_named_forking_service() {
        let c = super::Outside::new("rynas-crypt-vol1-docs", "rynas: encrypted folder", super::Duration::from_secs(60))
            .properties(&["LoadCredential=pass:/run/rynas/crypt/x.pass"])
            .daemon(["/usr/bin/gocryptfs", "-passfile", "${CREDENTIALS_DIRECTORY}/pass", "--", "/c", "/m"]);
        let s = c.display();
        assert!(s.starts_with("systemd-run --quiet --service-type=forking --unit=rynas-crypt-vol1-docs "), "{s}");
        assert!(!s.contains("--wait") && !s.contains("--pipe") && !s.contains("--collect") && !s.contains("RuntimeMaxSec"), "{s}");
        assert!(s.ends_with("--property=LoadCredential=pass:/run/rynas/crypt/x.pass -- /usr/bin/gocryptfs -passfile ${CREDENTIALS_DIRECTORY}/pass -- /c /m"), "{s}");
        assert!(c.timeout > super::Duration::from_secs(60));
    }

    #[test]
    fn debug_hides_stdin_and_env_values() {
        let c = Cmd::new("chpasswd")
            .arg("-c")
            .env("SECRET_TOKEN", "hunter2-env")
            .stdin(b"anna:hunter2-stdin\n".to_vec());
        let d = format!("{c:?}");
        assert!(!d.contains("hunter2"), "{d}");
        assert!(d.contains("chpasswd") && d.contains("SECRET_TOKEN=<redacted>"), "{d}");
        assert!(d.contains("<19 bytes redacted>"), "{d}");
    }

    use super::*;
    use std::sync::Mutex;

    #[tokio::test]
    async fn captures_output_and_exit_code() {
        let o = Cmd::new("sh")
            .args(["-c", "echo out; echo err >&2; exit 3"])
            .run(None, None)
            .await
            .unwrap();
        assert_eq!(o.code, Some(3));
        assert_eq!(o.stdout, "out\n");
        assert_eq!(o.stderr, "err\n");
        let e = Cmd::new("false").run_checked(None, None).await.unwrap_err();
        assert!(matches!(e, CmdError::Failed(_)), "{e}");
    }

    #[tokio::test]
    async fn arguments_are_not_interpreted_by_a_shell() {
        let o = Cmd::new("echo")
            .arg("$HOME; rm -rf /")
            .run(None, None)
            .await
            .unwrap();
        assert_eq!(o.stdout, "$HOME; rm -rf /\n");
    }

    #[tokio::test]
    async fn stdin_and_line_sink() {
        let seen = Mutex::new(Vec::new());
        let sink = |l: &str| seen.lock().unwrap().push(l.to_owned());
        let o = Cmd::new("cat")
            .stdin("a\nb\n")
            .run(None, Some(&sink))
            .await
            .unwrap();
        assert_eq!(o.stdout, "a\nb\n");
        assert_eq!(*seen.lock().unwrap(), ["a", "b"]);
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        // the grandchild would create the marker after 2 s if it outlived the group kill
        let script = format!("(sleep 2; touch {}) & sleep 30", marker.display());
        let t = Instant::now();
        let e = Cmd::new("sh")
            .args(["-c", &script])
            .timeout(Duration::from_millis(300))
            .run(None, None)
            .await
            .unwrap_err();
        assert!(matches!(e, CmdError::Timeout(_)), "{e}");
        assert!(t.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(!marker.exists(), "grandchild survived the timeout");
    }

    #[tokio::test]
    async fn cancellation_stops_the_command() {
        let token = CancellationToken::new();
        let t2 = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            t2.cancel();
        });
        let t = Instant::now();
        let e = Cmd::new("sleep")
            .arg("30")
            .run(Some(&token), None)
            .await
            .unwrap_err();
        assert!(matches!(e, CmdError::Cancelled));
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn blocking_variant() {
        let o = Cmd::new("sh")
            .args(["-c", "echo hi"])
            .run_blocking()
            .unwrap();
        assert_eq!(o.stdout, "hi\n");
        let e = Cmd::new("sleep")
            .arg("30")
            .timeout(Duration::from_millis(200))
            .run_blocking()
            .unwrap_err();
        assert!(matches!(e, CmdError::Timeout(_)));
        let e = Cmd::new("/nonexistent/program").run_blocking().unwrap_err();
        assert!(matches!(e, CmdError::Spawn(_)));
        // the output of a failing command is still there
        let o = Cmd::new("sh")
            .args(["-c", "echo found; exit 4"])
            .output_blocking()
            .unwrap();
        assert_eq!((o.code, o.stdout.as_str()), (Some(4), "found\n"));
    }

    #[test]
    fn clean_environment() {
        let o = Cmd::new("env").env_clear().output_blocking().unwrap();
        let mut keys: Vec<_> = o
            .stdout
            .lines()
            .filter_map(|l| l.split_once('=').map(|(k, _)| k))
            .collect();
        keys.sort();
        assert_eq!(keys, ["LC_ALL", "PATH"]);
    }

    #[test]
    fn the_end_of_a_long_output_is_kept() {
        let text: String = (0..200_000).map(|i| format!("line {i}\n")).collect();
        assert!(text.len() > 2 * super::CAPTURE_CAP);
        let got = super::capture(&mut text.as_bytes(), true);
        assert!(got.len() <= super::CAPTURE_CAP && got.starts_with("line ") && got.ends_with("line 199999\n"));
        assert!(super::capture(&mut text.as_bytes(), false).starts_with("line 0\n"));
        let mut s = text.clone();
        super::keep_end(&mut s);
        assert!(s.len() <= super::CAPTURE_CAP && s.starts_with("line ") && s.ends_with("line 199999\n"));
    }

    #[test]
    fn blocking_timeout_kills_the_group() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        let script = format!("(sleep 2; touch {}) & sleep 30", marker.display());
        let t = Instant::now();
        let e = Cmd::new("sh")
            .args(["-c", &script])
            .timeout(Duration::from_millis(200))
            .output_blocking()
            .unwrap_err();
        assert!(matches!(e, CmdError::Timeout(_)), "{e}");
        assert!(t.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(2500));
        assert!(!marker.exists(), "grandchild survived the timeout");
    }

    #[test]
    fn heavy_runs_at_the_lowest_priorities() {
        let o = Cmd::new("sh").args(["-c", "cut -d' ' -f19 /proc/self/stat"]).heavy().run_blocking().unwrap();
        assert_eq!(o.stdout.trim(), "19", "nice of the child");
        let o = Cmd::new("sh").args(["-c", "cut -d' ' -f19 /proc/self/stat"]).run_blocking().unwrap();
        assert_ne!(o.stdout.trim(), "19");
    }

    #[test]
    fn a_quiet_command_stalls_and_a_talkative_one_does_not() {
        let t = Instant::now();
        let e = Cmd::new("sleep").arg("30").timeout(Duration::from_secs(60)).stall_timeout(Duration::from_millis(400)).output_blocking().unwrap_err();
        assert!(matches!(e, CmdError::Stalled(_)), "{e}");
        assert!(t.elapsed() < Duration::from_secs(5));
        // a line every 200 ms for 1.2 s: never 400 ms without output
        let o = Cmd::new("sh")
            .args(["-c", "for i in 1 2 3 4 5 6; do echo $i; sleep 0.2; done"])
            .stall_timeout(Duration::from_millis(600))
            .run_blocking()
            .unwrap();
        assert_eq!(o.stdout.lines().count(), 6);
        // progress bumped by the caller keeps a quiet command alive
        let a = Activity::new();
        let a2 = a.clone();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let s2 = stop.clone();
        let bumper = std::thread::spawn(move || {
            while !s2.load(std::sync::atomic::Ordering::Relaxed) {
                a2.bump();
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        Cmd::new("sleep").arg("1").stall_watch(Duration::from_millis(500), a).run_blocking().unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        bumper.join().unwrap();
    }

    #[tokio::test]
    async fn async_stall_stops_the_command() {
        let t = Instant::now();
        let e = Cmd::new("sleep").arg("30").stall_timeout(Duration::from_millis(300)).run(None, None).await.unwrap_err();
        assert!(matches!(e, CmdError::Stalled(_)), "{e}");
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn blocking_run_stops_on_cancel() {
        let token = CancellationToken::new();
        let t2 = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            t2.cancel();
        });
        let t = Instant::now();
        let e = Cmd::new("sleep").arg("30").run_blocking_in(Some(&token)).unwrap_err();
        assert!(matches!(e, CmdError::Cancelled), "{e}");
        assert!(t.elapsed() < Duration::from_secs(5));
    }
}
