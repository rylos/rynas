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
}

impl std::fmt::Display for CmdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CmdError::Spawn(e) => write!(f, "cannot start: {e}"),
            CmdError::Timeout(d) => write!(f, "timed out after {} s", d.as_secs_f32()),
            CmdError::Cancelled => write!(f, "cancelled"),
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
        }
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
        c
    }

    /// Run to the end. `line` receives every output line as it arrives (stderr lines are
    /// prefixed with "stderr: "); `cancel` stops the command early.
    pub async fn run(
        &self,
        cancel: Option<&CancellationToken>,
        line: Option<&(dyn Fn(&str) + Sync)>,
    ) -> Result<CmdOutput, CmdError> {
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

        let readers =
            async { tokio::join!(read_lines(out, "", line), read_lines(err, "stderr: ", line)) };
        let (status, (stdout, stderr)) = tokio::join!(
            async {
                let waited = tokio::select! {
                    s = child.wait() => Ok(s),
                    _ = tokio::time::sleep(self.timeout) => Err(CmdError::Timeout(self.timeout)),
                    _ = cancel.cancelled() => Err(CmdError::Cancelled),
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
        let o = self.output_blocking()?;
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
        use std::io::Write;
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
        let t_out = small_thread(move || capture(&mut out));
        let t_err = small_thread(move || capture(&mut err));
        let deadline = Instant::now() + self.timeout;
        // short commands end in a few ms: poll often at first, then back off
        let mut nap = Duration::from_millis(1);
        let status = loop {
            match child.try_wait().map_err(CmdError::Spawn)? {
                Some(s) => break Ok(s),
                None if Instant::now() >= deadline => break Err(CmdError::Timeout(self.timeout)),
                None => {
                    std::thread::sleep(nap.min(deadline.saturating_duration_since(Instant::now())));
                    nap = (nap * 2).min(Duration::from_millis(50));
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
            c = c.arg(format!("--property=RuntimeMaxSec={}", self.timeout.as_secs().max(1)));
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

fn capture(r: &mut impl Read) -> String {
    let mut buf = Vec::new();
    let _ = r.take(CAPTURE_CAP as u64).read_to_end(&mut buf);
    // drain the rest so the child never blocks on a full pipe
    let _ = std::io::copy(r, &mut std::io::sink());
    String::from_utf8_lossy(&buf).into_owned()
}

async fn read_lines(
    r: impl AsyncRead + Unpin,
    prefix: &str,
    sink: Option<&(dyn Fn(&str) + Sync)>,
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
        if captured.len() < CAPTURE_CAP {
            captured.push_str(l);
            captured.push_str(mark);
            captured.push('\n');
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
    captured
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
}
