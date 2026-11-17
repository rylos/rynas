//! `rynasctl`: the small root helper the generated units and the apt hook call.
//!
//! - `rynasctl task-finished <id>`: from `ExecStopPost=+` of every task unit; sends systemd's
//!   `$SERVICE_RESULT`, `$EXIT_CODE`, `$EXIT_STATUS` and `$INVOCATION_ID` to rynasd, which
//!   records the run and its output. Always exits 0: bookkeeping never fails the unit.
//! - `rynasctl gate <id>`: `ExecCondition=` of every task unit, the scheduling gate
//!   (`rynas_scheduler::gate`): asks rynasd whether this start runs (exit 0) or is skipped
//!   (exit 1: paused, a missed run past its deadline, overlap). While the answer says
//!   `wait` (S4: the start waits in the queue of its volume, disk or the package system) it
//!   asks again every 5 s, saying in the journal what it waits for. Without an answer within
//!   20 s it runs the start, as before the gate (only a pause in the pause file still skips
//!   a timer's start), and spools the outcome for rynasd. Also the gate of the system's own
//!   timers rynas gates through drop-ins (`apt-upgrade`, `mdcheck-start`,
//!   `mdcheck-continue`).
//! - `rynasctl job run <id>`: `ExecStart=` of internal tasks; asks rynasd for the action:
//!   a command it then runs itself (scrub, SMART test, snapshots), or a rynasd task whose log
//!   it prints as it grows; exits with the result. SIGTERM (stop, timeout) cancels the
//!   rynasd task.
//! - `rynasctl snapshot system --reason apt ...`: a system restore point (apt hook); works
//!   without rynasd.
//! - `rynasctl power scheduled-shutdown <rule>`: `ExecStart=` of the task of a shutdown rule;
//!   starts rynasd's `power.shutdown` task (checks, policy, poweroff) and follows it.
//! - `rynasctl power arm-wake --boot|--shutdown`: `rynas-power-wake.service`; writes the next
//!   scheduled startup to the RTC (`rtcwake -m no`) or clears it; works without rynasd and
//!   never fails the unit.
//! - `rynasctl net rollback|boot-guard|watchdog|reset-button|confirm|status`: the network
//!   safety net (D-NET): the rollback timer of an unconfirmed change, the boot guard
//!   (`rynas-net-guard.service`, before networkd), the post-boot watchdog
//!   (`rynas-net-watchdog.timer`), a hardware module's reset button, a confirmation from the
//!   console. They work without rynasd (they read and write `/var/lib/rynas/net` under its
//!   lock) and then ask rynasd to collect the notifications they queued.
//! - `rynasctl ups safe-mode <reason>`: the pre-shutdown hook of a hardware module's UPS layer
//!   (the Synology module links it into synodeb's hooks); rynasd cancels its tasks, stops
//!   running scheduler units and records the reason. Always exits 0: the shutdown must go on.
//! - `rynasctl ups test quick|deep|stop`: a UPS battery test through rynasd (NUT or the
//!   module layer),
//!   followed to its outcome; the command of a scheduled battery test.
//! - `rynasctl crypt auto-unlock`: `rynas-crypt-unlock.service`, ordered before Docker at
//!   boot; rynasd unlocks the encrypted folders set to unlock automatically, so containers
//!   start on unlocked folders. Always exits 0.
//!
//! - `rynasctl ssh rollback`: `rynas-ssh-rollback.timer` of an unconfirmed SSH change; puts
//!   the previous drop-in and units back without rynasd (under `/var/lib/rynas/ssh/lock`).
//!
//! Talks HTTP/1.1 to rynasd's socket (root is always allowed by `SO_PEERCRED`).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::{Parser, Subcommand};
use rynas_api_types::{TaskInfo, TaskStarted, TaskStatus};
use rynas_auth::ipc::{DEFAULT_SOCKET, USER_HEADER};
use rynas_btrfs::system::{self, SystemSnapshot, Taken};
use rynas_scheduler::gate::{
    ANSWER_BY, GATE_TIMEOUT, GateAnswer, GateRequest, SpoolEntry, SpoolFinished, WAIT_POLL,
    load_pause, spool_append,
};
use rynasd::jobs::{JobPlan, PlanReport};
use rynasd::scheduler::{ExecuteRequest, TaskFinished};

#[derive(Parser, Debug)]
#[command(
    version,
    about = "rynas helper for scheduler units, the apt hook and the network safety net"
)]
struct Args {
    /// rynasd socket.
    #[arg(long, default_value = DEFAULT_SOCKET)]
    socket: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Record the end of a scheduler task run (ExecStopPost of rynas-task-<id>.service).
    TaskFinished {
        id: String,
        /// rynasd's state directory (the spool, when rynasd cannot be reached).
        #[arg(long, default_value = "/var/lib/rynas")]
        state_dir: PathBuf,
    },
    /// The scheduling gate (ExecCondition of rynas-task-<id>.service): exit 0 runs the
    /// start, 1 skips it.
    Gate {
        id: String,
        /// rynasd's state directory (the pause file, the spool of outcomes).
        #[arg(long, default_value = "/var/lib/rynas")]
        state_dir: PathBuf,
    },
    /// Internal scheduler tasks.
    Job {
        #[command(subcommand)]
        action: JobAction,
    },
    /// Snapshots.
    Snapshot {
        #[command(subcommand)]
        action: SnapshotAction,
    },
    /// Power schedule.
    Power {
        #[command(subcommand)]
        action: PowerCmd,
    },
    /// UPS.
    Ups {
        #[command(subcommand)]
        action: UpsCmd,
    },
    /// Network safety net.
    Net {
        /// rynasd's state directory (network revisions and trial marker below `net/`).
        #[arg(long, default_value = "/var/lib/rynas")]
        state_dir: PathBuf,
        /// Do not ask rynasd to collect the queued notifications (early boot: rynasd is not
        /// running and its socket would start it).
        #[arg(long)]
        no_flush: bool,
        #[command(subcommand)]
        action: NetAction,
    },
    /// Encrypted folders.
    Crypt {
        #[command(subcommand)]
        action: CryptCmd,
    },
    /// SSH service.
    Ssh {
        /// rynasd's state directory (the change under test in `ssh/trial.json`).
        #[arg(long, default_value = "/var/lib/rynas")]
        state_dir: PathBuf,
        #[command(subcommand)]
        action: SshCmd,
    },
    /// QNAP module: the Super I/O worker and the first-boot LED test (see docs/plugins/qnap.md).
    Qnap {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
enum SshCmd {
    /// Put back the SSH settings an unconfirmed change replaced (rynas-ssh-rollback.timer);
    /// works without rynasd.
    Rollback,
}

#[derive(Subcommand, Debug)]
enum CryptCmd {
    /// At boot, before Docker: unlock the encrypted folders whose password is sealed on this
    /// machine (rynas-crypt-unlock.service).
    AutoUnlock,
}

#[derive(Subcommand, Debug)]
enum PowerCmd {
    /// Run the scheduled shutdown of rule <rule> in rynasd and follow it to its end.
    ScheduledShutdown { rule: String },
    /// Arm the RTC wake alarm for the next scheduled startup (or clear it).
    ArmWake {
        /// At boot (ExecStart): also records how this boot started.
        #[arg(long, conflicts_with = "shutdown")]
        boot: bool,
        /// At shutdown (ExecStop).
        #[arg(long)]
        shutdown: bool,
        /// The applied schedule (written by the apply).
        #[arg(long, default_value = rynas_power::domain::SCHEDULE_FILE)]
        schedule: PathBuf,
        /// rynasd's state directory (power/state.json).
        #[arg(long, default_value = "/var/lib/rynas")]
        state_dir: PathBuf,
        /// Root of /sys and /proc (tests: a fake tree).
        #[arg(long, default_value = "/")]
        root: PathBuf,
        /// The rtcwake program.
        #[arg(long, default_value = "rtcwake")]
        rtcwake: String,
    },
}

#[derive(Subcommand, Debug)]
enum UpsCmd {
    /// Before a module layer's UPS shutdown: stop rynas tasks gracefully and record the reason
    /// (lowbatt, waittimeup, nocomm, fsd, shutdowncmd).
    SafeMode { reason: String },
    /// Run a UPS battery test (quick, deep) or stop the running one (stop), and follow it to
    /// its outcome (for Task Scheduler tasks).
    Test {
        /// quick, deep or stop.
        kind: String,
    },
}

#[derive(Subcommand, Debug)]
enum NetAction {
    /// Restore the last known good revision of an unconfirmed change (the rollback timer).
    Rollback {
        /// `timeout` (the timer), `boot_check` (migration at boot), `manual`.
        #[arg(long, default_value = "manual")]
        reason: String,
        /// The trial the timer was armed for (its start, in ms): set by the timer itself.
        #[arg(long)]
        trial: Option<u64>,
    },
    /// Early boot: undo an interrupted trial, or carry out a migration planned for this boot.
    BootGuard,
    /// After boot: fall back to the last known good revision, then to the rescue
    /// configuration, when no connected port has a routable address.
    Watchdog,
    /// One press of the reset button (wired by the machine's hardware module): a second
    /// press within 10 s restores the factory network.
    ResetButton,
    /// Confirm the change under test from the console (the token is printed by the API).
    Confirm {
        #[arg(long)]
        token: String,
    },
    /// Show the change under test, if any.
    Status,
}

#[derive(Subcommand, Debug)]
enum JobAction {
    /// Run the internal task <id> in rynasd and follow it to its end.
    Run { id: String },
}

#[derive(Subcommand, Debug)]
enum SnapshotAction {
    /// Take a system restore point (skipped when nothing changed since the last one).
    System {
        #[arg(long, default_value = "manual")]
        reason: String,
        #[arg(long, default_value = "/")]
        subvolume: PathBuf,
        #[arg(long, default_value = "/.snapshots/rootfs")]
        snapshot_dir: PathBuf,
        #[arg(long, default_value = "rootfs")]
        prefix: String,
        #[arg(long, default_value_t = 10)]
        keep: u32,
        /// rynasd's state directory (reasons log).
        #[arg(long, default_value = "/var/lib/rynas")]
        state_dir: PathBuf,
    },
}

const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Answers read back at most (a task log chunk is 256 KiB).
const MAX_ANSWER: usize = 2 << 20;

struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn error(&self) -> String {
        format!(
            "rynasd answered {}: {}",
            self.status,
            String::from_utf8_lossy(&self.body).trim()
        )
    }
}

/// One request on a fresh connection (`Connection: close`).
fn request(
    socket: &PathBuf,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> Result<Answer, String> {
    request_waiting(socket, method, path, body, IO_TIMEOUT)
}

/// [`request`] whose answer may take up to `wait`.
fn request_waiting(
    socket: &PathBuf,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    wait: Duration,
) -> Result<Answer, String> {
    let mut s = UnixStream::connect(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    s.set_read_timeout(Some(wait)).ok();
    s.set_write_timeout(Some(IO_TIMEOUT)).ok();
    let body = body.unwrap_or(b"");
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: rynasd\r\nConnection: close\r\n{USER_HEADER}: root\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
    s.write_all(body).map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    s.take(MAX_ANSWER as u64)
        .read_to_end(&mut raw)
        .map_err(|e| format!("reading the answer: {e}"))?;
    parse_answer(&raw)
}

fn parse_answer(raw: &[u8]) -> Result<Answer, String> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("truncated answer")?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .ok_or("invalid status line")?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let mut body = raw[split + 4..].to_vec();
    let chunked = headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked")
    });
    if chunked {
        body = dechunk(&body)?;
    }
    Ok(Answer {
        status,
        headers,
        body,
    })
}

fn dechunk(mut b: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let nl = b.windows(2).position(|w| w == b"\r\n").ok_or("bad chunk")?;
        let size_text = String::from_utf8_lossy(&b[..nl]);
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| "bad chunk size")?;
        b = &b[nl + 2..];
        if size == 0 {
            return Ok(out);
        }
        if b.len() < size {
            return Err("truncated chunk".into());
        }
        out.extend_from_slice(&b[..size]);
        b = b.get(size + 2..).unwrap_or(&[]);
    }
}

fn task_finished(socket: &PathBuf, state: &std::path::Path, id: &str) -> ExitCode {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let body = TaskFinished {
        invocation_id: env("INVOCATION_ID"),
        result: env("SERVICE_RESULT"),
        exit_code: env("EXIT_CODE"),
        exit_status: env("EXIT_STATUS"),
    };
    let json = serde_json::to_vec(&body).unwrap_or_default();
    let failed = match request(
        socket,
        "POST",
        &format!("/v1/scheduler/tasks/{id}/finished"),
        Some(&json),
    ) {
        Ok(a) if a.status / 100 == 2 => None,
        // a refusal (4xx) will not change on a second try
        Ok(a) if a.status / 100 == 4 => {
            warn(&format!("rynasctl: recording the run of {id}: {}", a.error()));
            None
        }
        // rynasd is away or could not record it (5xx): recorded from the spool later (an end
        // recorded twice is kept once)
        Ok(a) => Some(a.error()),
        Err(e) => Some(e),
    };
    if let Some(e) = failed {
        warn(&format!("rynasctl: recording the run of {id}: {e}; kept for rynasd"));
        let entry = SpoolEntry {
            task_id: id.to_string(),
            invocation_id: body.invocation_id.clone(),
            at_ms: rynas_tasks::now_ms(),
            by_timer: false,
            skipped: None,
            finished: Some(SpoolFinished {
                result: body.result,
                exit_code: body.exit_code,
                exit_status: body.exit_status,
            }),
        };
        if !entry.invocation_id.is_empty()
            && let Err(err) = spool_append(state, &entry)
        {
            warn(&format!("rynasctl: spooling the run of {id}: {err}"));
        }
    }
    ExitCode::SUCCESS
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn on_stop_signals() {
    // SAFETY: the handler only stores into an atomic
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
}

fn gate(socket: &PathBuf, state: &std::path::Path, id: &str) -> ExitCode {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let mut req = GateRequest {
        invocation_id: env("INVOCATION_ID"),
        trigger_unit: env("TRIGGER_UNIT"),
        answer_by_ms: 0,
        trigger_timer_ms: env("TRIGGER_TIMER_REALTIME_USEC").parse::<u64>().map_or(0, |us| us / 1000),
        // this rynasctl asks again while its start waits in the queue (S4)
        can_wait: true,
        queued_since_ms: 0,
    };
    let mut said = String::new();
    loop {
        // after this rynasd answers "run" without recording a skip: the start may be
        // running by the time a late answer arrives
        req.answer_by_ms = rynas_tasks::now_ms() + ANSWER_BY.as_millis() as u64;
        let body = serde_json::to_vec(&req).unwrap_or_default();
        let answer = request_waiting(
            socket,
            "POST",
            &format!("/v1/scheduler/tasks/{id}/gate"),
            Some(&body),
            GATE_TIMEOUT,
        )
        .and_then(|a| match a.status {
            200 => serde_json::from_slice::<GateAnswer>(&a.body).map_err(|e| e.to_string()),
            _ => Err(a.error()),
        });
        match answer {
            Ok(a) if a.wait => {
                // queued: say what it waits for when that changes, ask again soon
                if !a.message.is_empty() && a.message != said {
                    say(&a.message);
                    said = a.message.clone();
                }
                if req.queued_since_ms == 0 {
                    req.queued_since_ms = rynas_tasks::now_ms();
                    on_stop_signals();
                }
                let until = std::time::Instant::now() + WAIT_POLL;
                while std::time::Instant::now() < until {
                    if STOP.load(Ordering::SeqCst) {
                        // stopped while it waited: systemd is stopping the unit, nothing
                        // starts after this; exit 0, since any other exit (or dying by the
                        // signal) during a stop marks the unit failed (systemd 257)
                        say("rynas: stopped while waiting in the queue: nothing started");
                        return ExitCode::SUCCESS;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
            Ok(a) => {
                if !a.message.is_empty() {
                    say(&a.message);
                }
                return if a.run { ExitCode::SUCCESS } else { ExitCode::from(1) };
            }
            Err(e) => {
                // as before the gate, but a pause the user set still holds for timer starts
                let now = rynas_tasks::now_ms();
                let by_timer = rynas_scheduler::gate::started_by_timer(
                    req.trigger_unit == rynas_scheduler::units::timer_of(id),
                    req.trigger_timer_ms,
                    None,
                    now,
                );
                let skipped = if by_timer { load_pause(state).blocks(id, now) } else { None };
                let entry = SpoolEntry {
                    task_id: id.to_string(),
                    invocation_id: req.invocation_id.clone(),
                    at_ms: now,
                    by_timer,
                    skipped,
                    finished: None,
                };
                if !req.invocation_id.is_empty()
                    && let Err(err) = spool_append(state, &entry)
                {
                    warn(&format!("rynasctl: spooling the gate outcome: {err}"));
                }
                return if skipped.is_some() {
                    say(&format!("rynas: no answer from the scheduling gate ({e}); the task is paused: start skipped"));
                    ExitCode::from(1)
                } else {
                    say(&format!("rynas: no answer from the scheduling gate ({e}); running the start as before the gate"));
                    ExitCode::SUCCESS
                };
            }
        }
    }
}

/// One line on standard output; a failed write (journal gone) is ignored, never a panic.
fn say(m: &str) {
    let mut o = std::io::stdout().lock();
    let _ = writeln!(o, "{m}");
    let _ = o.flush();
}

/// One line on standard error, ignoring a failed write.
fn warn(m: &str) {
    let _ = writeln!(std::io::stderr().lock(), "{m}");
}

fn job_run(socket: &PathBuf, id: &str) -> ExitCode {
    on_stop_signals();
    let body = serde_json::to_vec(&ExecuteRequest { plan: true }).unwrap_or_default();
    // a scrub first waits for a deduplication run to save its work (up to 5.5 minutes)
    let started = request_waiting(
        socket,
        "POST",
        &format!("/v1/scheduler/tasks/{id}/execute"),
        Some(&body),
        Duration::from_secs(8 * 60),
    );
    match started {
        Ok(a) if a.status == 200 => match serde_json::from_slice::<JobPlan>(&a.body) {
            Ok(plan) => run_plan(&plan),
            Err(e) => {
                eprintln!("rynasctl: task {id}: unreadable plan: {e}");
                ExitCode::FAILURE
            }
        },
        Ok(a) if a.status == 202 => match serde_json::from_slice::<TaskStarted>(&a.body) {
            Ok(s) => follow_task(socket, &s.task_id),
            Err(e) => {
                eprintln!("rynasctl: starting task {id}: {e}");
                ExitCode::FAILURE
            }
        },
        Ok(a) => {
            eprintln!("rynasctl: starting task {id}: {}", a.error());
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("rynasctl: starting task {id}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Runs a command planned by rynasd in this unit and exits with its status.
fn run_plan(plan: &JobPlan) -> ExitCode {
    use std::process::{Command as Proc, Stdio};
    let Some((prog, args)) = plan.argv.split_first() else {
        eprintln!("rynasctl: empty plan");
        return ExitCode::FAILURE;
    };
    let mut c = Proc::new(prog);
    c.args(args).env_clear().stdin(Stdio::null());
    for (k, v) in &plan.env {
        c.env(k, v);
    }
    if !plan.env.iter().any(|(k, _)| k == "PATH") {
        c.env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    }
    let btrbk = plan.report == PlanReport::Btrbk;
    if btrbk {
        c.stdout(Stdio::piped());
    }
    println!("$ {}", plan.argv.join(" "));
    let _ = std::io::stdout().flush();
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => {
            eprintln!("rynasctl: {prog}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let reader = child.stdout.take().map(|mut o| {
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = o.read_to_string(&mut s);
            s
        })
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(plan.timeout_s.max(1));
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {}
            Err(e) => {
                eprintln!("rynasctl: waiting for {prog}: {e}");
                break None;
            }
        }
        if std::time::Instant::now() >= deadline {
            eprintln!("rynasctl: {prog} did not end within {} s: stopping it", plan.timeout_s);
            let _ = child.kill();
            let _ = child.wait();
            return ExitCode::FAILURE;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let out = reader.map(|h| h.join().unwrap_or_default()).unwrap_or_default();
    let code = status.and_then(|s| s.code());
    if btrbk {
        let r = rynas_btrfs::btrbk::parse_report(&out);
        for n in &r.created {
            println!("created {n}");
        }
        for n in &r.deleted {
            println!("deleted {n} (retention)");
        }
        if !r.errors.is_empty() {
            eprintln!("btrbk: {}", r.errors.join("; "));
            return ExitCode::FAILURE;
        }
        if code == Some(0) {
            println!("{} created, {} deleted", r.created.len(), r.deleted.len());
        }
    }
    // a scrub rynasd asked to give way (S4): its cancel is not a failure
    if code != Some(0)
        && let Some(state) = &plan.yield_state
        && rynas_scheduler::gate::gave_way(std::path::Path::new(state), &std::env::var("INVOCATION_ID").unwrap_or_default())
    {
        println!("rynas: stopped to give way to more urgent work on the volume; it resumes by itself from where it stopped");
        return ExitCode::SUCCESS;
    }
    match code {
        Some(0) => ExitCode::SUCCESS,
        Some(c) if plan.ok_codes.contains(&c) => {
            println!("{prog} exited with code {c}: nothing left to do");
            ExitCode::SUCCESS
        }
        Some(c) => {
            eprintln!("rynasctl: {prog} exited with code {c}");
            ExitCode::from(c.clamp(1, 255) as u8)
        }
        None => {
            eprintln!("rynasctl: {prog} was killed by a signal");
            ExitCode::FAILURE
        }
    }
}

fn follow(socket: &PathBuf, id: &str, start: &str) -> ExitCode {
    follow_with(socket, id, start, None)
}

/// Starts a rynasd task with `POST start` (and `body`), prints its log as it grows and exits
/// with its result; SIGTERM cancels it.
fn follow_with(socket: &PathBuf, id: &str, start: &str, body: Option<&[u8]>) -> ExitCode {
    on_stop_signals();
    let started = match request(socket, "POST", start, body) {
        Ok(a) if a.status == 202 => {
            serde_json::from_slice::<TaskStarted>(&a.body).map_err(|e| e.to_string())
        }
        Ok(a) => Err(a.error()),
        Err(e) => Err(e),
    };
    match started {
        Ok(s) => follow_task(socket, &s.task_id),
        Err(e) => {
            eprintln!("rynasctl: starting task {id}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Prints the log of rynasd task `upid` as it grows and exits with its result; SIGTERM
/// cancels it. A task rynasd no longer knows (it restarted meanwhile) ends the follow.
fn follow_task(socket: &PathBuf, upid: &str) -> ExitCode {
    let upid = upid.to_string();
    println!("rynas task {upid}");
    let _ = std::io::stdout().flush();
    let mut offset: u64 = 0;
    let mut cancelled = false;
    loop {
        if STOP.load(Ordering::SeqCst) && !cancelled {
            cancelled = true;
            eprintln!("rynasctl: stopping, cancelling {upid}");
            let _ = request(socket, "POST", &format!("/v1/tasks/{upid}/cancel"), None);
        }
        let mut complete = false;
        let log = request(
            socket,
            "GET",
            &format!("/v1/tasks/{upid}/log?offset={offset}"),
            None,
        );
        if let Ok(a) = &log
            && a.status == 404
        {
            eprintln!("rynasctl: rynasd no longer knows task {upid} (it restarted): the run is lost");
            return ExitCode::FAILURE;
        }
        if let Ok(a) = log
            && a.status == 200
        {
            let _ = std::io::stdout().write_all(&a.body);
            let _ = std::io::stdout().flush();
            offset = a
                .header("x-rynas-next-offset")
                .and_then(|v| v.parse().ok())
                .unwrap_or(offset + a.body.len() as u64);
            complete = a.header("x-rynas-log-complete") == Some("true");
        }
        if complete {
            let info = request(socket, "GET", &format!("/v1/tasks/{upid}"), None)
                .ok()
                .filter(|a| a.status == 200)
                .and_then(|a| serde_json::from_slice::<TaskInfo>(&a.body).ok());
            return match info.map(|i| i.status) {
                Some(TaskStatus::Ok) => ExitCode::SUCCESS,
                Some(s) => {
                    eprintln!("rynasctl: task {upid} ended: {s:?}");
                    ExitCode::FAILURE
                }
                None => ExitCode::FAILURE,
            };
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn snapshot_system(a: SnapshotAction) -> ExitCode {
    let SnapshotAction::System {
        reason,
        subvolume,
        snapshot_dir,
        prefix,
        keep,
        state_dir,
    } = a;
    if !system::valid_reason(&reason) || !rynas_btrfs::domain::valid_job_name(&prefix) {
        eprintln!("rynasctl: invalid reason or prefix");
        return ExitCode::FAILURE;
    }
    let reasons = system::reasons_path(&state_dir);
    let s = SystemSnapshot {
        subvolume: &subvolume,
        snapshot_dir: &snapshot_dir,
        prefix: &prefix,
        keep,
        reasons: Some(&reasons),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    match system::take(&s, &reason, now) {
        Ok(Taken::Created { name, pruned }) => {
            println!(
                "rynas: restore point {} ({reason})",
                snapshot_dir.join(&name).display()
            );
            for p in pruned {
                println!("rynas: old restore point {p} deleted");
            }
            ExitCode::SUCCESS
        }
        Ok(Taken::Skipped { latest, why }) => {
            println!("rynas: no new restore point: {why} ({latest})");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("rynas: restore point not taken: {e}");
            ExitCode::FAILURE
        }
    }
}

fn ups(socket: &PathBuf, cmd: UpsCmd) -> ExitCode {
    let reason = match cmd {
        UpsCmd::SafeMode { reason } => reason,
        UpsCmd::Test { kind } => {
            use rynas_api_types::ups::{BatteryTest, BatteryTestRequest};
            let Some(test) = BatteryTest::from_arg(&kind) else {
                eprintln!("rynasctl: the test is quick, deep or stop");
                return ExitCode::FAILURE;
            };
            let body = serde_json::to_vec(&BatteryTestRequest { test }).unwrap_or_default();
            return follow_with(socket, "ups.test", "/v1/ups/test", Some(&body));
        }
    };
    let body = serde_json::to_vec(&rynas_api_types::ups::SafeModeRequest { reason })
        .unwrap_or_default();
    match request(socket, "POST", "/v1/ups/safe-mode", Some(&body)) {
        Ok(a) if a.status / 100 == 2 => {
            println!("rynas: {}", String::from_utf8_lossy(&a.body).trim());
        }
        Ok(a) => eprintln!("rynasctl: Safe Mode: {}", a.error()),
        Err(e) => eprintln!("rynasctl: Safe Mode: {e}"),
    }
    ExitCode::SUCCESS
}

fn power(socket: &PathBuf, cmd: PowerCmd) -> ExitCode {
    match cmd {
        PowerCmd::ScheduledShutdown { rule } => {
            if !rynas_power::rules::valid_rule_id(&rule) {
                eprintln!("rynasctl: invalid rule id");
                return ExitCode::FAILURE;
            }
            follow(
                socket,
                &rynas_power::rules::task_id(&rule),
                &format!("/v1/power/schedule/rules/{rule}/shutdown"),
            )
        }
        PowerCmd::ArmWake {
            boot,
            shutdown: _,
            schedule,
            state_dir,
            root,
            rtcwake,
        } => arm_wake(boot, &schedule, &state_dir, &root, &rtcwake),
    }
}

/// Never fails: a problem with the RTC must not hold a boot or a shutdown.
fn arm_wake(
    boot: bool,
    schedule: &std::path::Path,
    state_dir: &std::path::Path,
    root: &std::path::Path,
    rtcwake: &str,
) -> ExitCode {
    use rynas_power::wake::{ArmInput, ArmReason, arm};
    let sched: Option<rynas_api_types::power::PowerSchedule> = match std::fs::read(schedule) {
        Ok(b) => match serde_json::from_slice(&b) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("rynasctl: {}: {e}", schedule.display());
                None
            }
        },
        Err(_) => None,
    };
    let tz = rynas_power::system_tz();
    // the hardware module's wake clock, when it has one
    let clock = rynasd::modules::detect(root, std::path::Path::new("/"), false).wake_clock();
    let run = |rtc: &str, wake: Option<i64>| -> Result<(), String> {
        let args = rynas_power::wake::rtcwake_args(rtc, wake);
        println!("rynas: {rtcwake} {}", args.join(" "));
        match rynas_tasks::Cmd::new(rtcwake)
            .args(&args)
            .env_clear()
            .timeout(Duration::from_secs(10))
            .run_blocking()
        {
            Ok(_) => Ok(()),
            // no rtcwake (minimal systems): the same through sysfs
            Err(rynas_tasks::CmdError::Spawn(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("rynas: {rtcwake} not found, using {rtc}/wakealarm");
                rynas_power::wake::sysfs_arm(root, rtc, wake)
            }
            Err(e) => Err(format!("{rtcwake}: {e}")),
        }
    };
    let a = arm(
        &ArmInput {
            root,
            state_dir,
            schedule: sched.as_ref(),
            now: jiff::Timestamp::now(),
            tz: &tz,
            reason: if boot {
                ArmReason::Boot
            } else {
                ArmReason::Shutdown
            },
            prefer_rtc: clock.map(|c| c.rtc_device_suffix),
        },
        &run,
    );
    match (&a.wake_ms, &a.error) {
        (_, Some(e)) => eprintln!("rynas: wake alarm not set: {e}"),
        (Some(ms), None) => {
            let t = jiff::Timestamp::from_millisecond(*ms as i64)
                .map(|t| {
                    t.to_zoned(tz.clone())
                        .strftime("%a %Y-%m-%d %H:%M %Z")
                        .to_string()
                })
                .unwrap_or_default();
            println!(
                "rynas: next scheduled startup {t} (rule {}) armed on {}",
                a.rule.as_deref().unwrap_or("?"),
                a.rtc.as_deref().unwrap_or("?")
            )
        }
        (None, None) => println!("rynas: no scheduled startup: wake alarm cleared"),
    }
    ExitCode::SUCCESS
}

fn net_engine(state_dir: &std::path::Path) -> rynas_net::Engine {
    let root = std::path::Path::new("/");
    rynas_net::Engine::new(
        std::sync::Arc::new(rynas_net::HostOs::new("/", false)),
        rynas_net::NetPaths::new(state_dir.join("net"), "/run/rynas"),
    )
    // the hardware module's beeps and LEDs on the rescue paths
    .with_feedback(rynasd::modules::detect(root, root, false).rescue_feedback())
}

fn net(socket: &PathBuf, state_dir: &std::path::Path, flush: bool, action: NetAction) -> ExitCode {
    let e = net_engine(state_dir);
    let log = |l: &str| {
        println!("{l}");
        let _ = std::io::stdout().flush();
    };
    let result: Result<String, String> = match action {
        NetAction::Rollback { reason, trial } => {
            use rynas_net::reason as r;
            if ![r::TIMEOUT, r::BOOT_CHECK, r::MANUAL].contains(&reason.as_str()) {
                eprintln!("rynasctl: unknown reason {reason}");
                return ExitCode::FAILURE;
            }
            match e.rollback_trial(&reason, trial, &log) {
                Ok(Some(r)) if r.verified => Ok(format!(
                    "revision {} restored and verified",
                    r.restored_revision
                )),
                Ok(Some(r)) => Err(format!(
                    "revision {} restored but NOT verified: {}",
                    r.restored_revision,
                    r.problems.join("; ")
                )),
                Ok(None) => Ok("nothing to roll back".into()),
                Err(err) => Err(err.to_string()),
            }
        }
        NetAction::BootGuard => e.boot_guard(&log).map_err(|x| x.to_string()),
        NetAction::Watchdog => e.watchdog(&log).map_err(|x| x.to_string()),
        NetAction::ResetButton => e
            .reset_button(&log)
            .map(|p| format!("reset button: {p:?}"))
            .map_err(|x| x.to_string()),
        NetAction::Confirm { token } => e
            .confirm(&token, rynas_net::txn::Origin::Cli)
            .map(|r| format!("revision {r} confirmed"))
            .map_err(|x| x.to_string()),
        NetAction::Status => match e.trial() {
            Ok(Some(m)) => Ok(format!(
                "revision {} under test ({:?}) on {}, rolled back to {} at {} ms",
                m.revision,
                m.kind,
                m.interfaces.join(", "),
                m.lkg,
                m.deadline_ms
            )),
            Ok(None) => Ok("no change under test".into()),
            Err(err) => Err(err.to_string()),
        },
    };
    // rynasd collects the queued notifications (it also does at start and every minute)
    if flush {
        let _ = request(socket, "POST", "/v1/network/events/flush", None);
    }
    match result {
        Ok(m) => {
            println!("rynas: {m}");
            ExitCode::SUCCESS
        }
        Err(m) => {
            eprintln!("rynas: {m}");
            ExitCode::FAILURE
        }
    }
}

/// The subcommand named on the command line (the first word that is not an option or the
/// value of `--socket`).
fn subcommand(args: &[String]) -> Option<&str> {
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        if a == "--socket" {
            it.next();
        } else if !a.starts_with('-') {
            return Some(a);
        }
    }
    None
}

/// The hooks of every task unit: `gate` (`ExecCondition=`: exit 1-254 skips the start, a
/// signal or 255 fails it) and `task-finished` (`ExecStopPost=`). Whatever goes wrong inside
/// them, the start goes ahead and the unit does not fail.
fn is_unit_hook(cmd: Option<&str>) -> bool {
    matches!(cmd, Some("gate" | "task-finished"))
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let hook = is_unit_hook(subcommand(&argv));
    if hook {
        // panic = "abort": the hook runs before the abort, so exit 0 here (a signal exit
        // would fail the unit, which then never runs)
        std::panic::set_hook(Box::new(|info| {
            warn(&format!("rynasctl: internal error ({info}); the start goes ahead"));
            std::process::exit(0);
        }));
    }
    let args = match Args::try_parse_from(&argv) {
        Ok(a) => a,
        // arguments of a newer or hand-edited unit: never a silent skip (exit 2)
        Err(e)
            if hook
                && !matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
                ) =>
        {
            let first = e.to_string();
            let first = first.lines().next().unwrap_or_default().to_string();
            warn(&format!("rynasctl: {first}; the start goes ahead"));
            return ExitCode::SUCCESS;
        }
        Err(e) => e.exit(),
    };
    match args.command {
        Command::TaskFinished { id, state_dir } => task_finished(&args.socket, &state_dir, &id),
        Command::Gate { id, state_dir } => gate(&args.socket, &state_dir, &id),
        Command::Job {
            action: JobAction::Run { id },
        } => job_run(&args.socket, &id),
        Command::Snapshot { action } => snapshot_system(action),
        Command::Power { action } => power(&args.socket, action),
        Command::Ups { action } => ups(&args.socket, action),
        Command::Net {
            state_dir,
            no_flush,
            action,
        } => net(&args.socket, &state_dir, !no_flush, action),
        Command::Crypt {
            action: CryptCmd::AutoUnlock,
        } => crypt_auto_unlock(&args.socket),
        Command::Ssh {
            state_dir,
            action: SshCmd::Rollback,
        } => {
            let ssh = rynas_services::ssh::Ssh::new(
                "/",
                &state_dir,
                std::sync::Arc::new(rynas_services::ssh::HostRunner),
            );
            match ssh.rollback("timeout") {
                Ok(true) => {
                    println!("rynas: the unconfirmed SSH change was undone");
                    ExitCode::SUCCESS
                }
                Ok(false) => {
                    println!("rynas: no SSH change to undo");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("rynasctl: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Qnap { args } => ExitCode::from(rynas_hw_qnap::superio::worker_main(&args) as u8),
    }
}

/// `rynas-crypt-unlock.service`, before Docker at boot: rynasd unlocks the encrypted folders
/// whose password is sealed on this machine and answers when it is done. A failure is
/// reported by rynasd (notification) and never fails the unit: Docker starts anyway.
fn crypt_auto_unlock(socket: &PathBuf) -> ExitCode {
    match request_waiting(socket, "POST", "/v1/crypt/auto-unlock", None, Duration::from_secs(270)) {
        Ok(a) if a.status / 100 == 2 => println!("rynas: unlocked {}", String::from_utf8_lossy(&a.body).trim()),
        Ok(a) => eprintln!("rynasctl: automatic unlock: {}", a.error()),
        Err(e) => eprintln!("rynasctl: automatic unlock: {e}"),
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_answers() {
        let a = parse_answer(b"HTTP/1.1 202 Accepted\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n{}").unwrap();
        assert_eq!(a.status, 202);
        assert_eq!(a.header("Content-Type"), Some("application/json"));
        assert_eq!(a.body, b"{}");
        let c = parse_answer(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nx-rynas-next-offset: 9\r\n\r\n4\r\nline\r\n5\r\n two\n\r\n0\r\n\r\n").unwrap();
        assert_eq!(c.body, b"line two\n");
        assert_eq!(c.header("x-rynas-next-offset"), Some("9"));
        assert!(parse_answer(b"HTTP/1.1 200").is_err());
    }

    #[test]
    fn unit_hooks_are_recognized_before_parsing() {
        let v = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(subcommand(&v("rynasctl gate t1")), Some("gate"));
        assert_eq!(subcommand(&v("rynasctl --socket /run/x.sock task-finished t1")), Some("task-finished"));
        assert_eq!(subcommand(&v("rynasctl --socket=/run/x gate t1 --future-flag")), Some("gate"));
        assert!(is_unit_hook(subcommand(&v("rynasctl gate t1 --unknown"))));
        assert!(!is_unit_hook(subcommand(&v("rynasctl job run t1"))));
        assert_eq!(subcommand(&v("rynasctl --socket")), None);
        // what main turns into exit 0 for a hook
        assert!(Args::try_parse_from(v("rynasctl gate t1 --unknown")).is_err());
        assert!(Args::try_parse_from(v("rynasctl gate t1")).is_ok());
    }
}
