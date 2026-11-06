//! `rynasctl`: the small root helper the generated units and the apt hook call.
//!
//! - `rynasctl task-finished <id>`: from `ExecStopPost=+` of every task unit; sends systemd's
//!   `$SERVICE_RESULT`, `$EXIT_CODE`, `$EXIT_STATUS` and `$INVOCATION_ID` to rynasd, which
//!   records the run and its output. Always exits 0: bookkeeping never fails the unit.
//! - `rynasctl job run <id>`: `ExecStart=` of internal tasks; asks rynasd to start the task,
//!   prints its log as it grows and exits with its result. SIGTERM (stop, timeout) cancels
//!   the rynasd task.
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
use rynasd::scheduler::TaskFinished;

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
    TaskFinished { id: String },
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
    let mut s = UnixStream::connect(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    s.set_read_timeout(Some(IO_TIMEOUT)).ok();
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

fn task_finished(socket: &PathBuf, id: &str) -> ExitCode {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let body = TaskFinished {
        invocation_id: env("INVOCATION_ID"),
        result: env("SERVICE_RESULT"),
        exit_code: env("EXIT_CODE"),
        exit_status: env("EXIT_STATUS"),
    };
    let json = serde_json::to_vec(&body).unwrap_or_default();
    match request(
        socket,
        "POST",
        &format!("/v1/scheduler/tasks/{id}/finished"),
        Some(&json),
    ) {
        Ok(a) if a.status / 100 == 2 => {}
        Ok(a) => eprintln!("rynasctl: recording the run of {id}: {}", a.error()),
        Err(e) => eprintln!("rynasctl: recording the run of {id}: {e}"),
    }
    ExitCode::SUCCESS
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn job_run(socket: &PathBuf, id: &str) -> ExitCode {
    follow(socket, id, &format!("/v1/scheduler/tasks/{id}/execute"))
}

fn follow(socket: &PathBuf, id: &str, start: &str) -> ExitCode {
    follow_with(socket, id, start, None)
}

/// Starts a rynasd task with `POST start` (and `body`), prints its log as it grows and exits
/// with its result; SIGTERM cancels it.
fn follow_with(socket: &PathBuf, id: &str, start: &str, body: Option<&[u8]>) -> ExitCode {
    // SAFETY: the handler only stores into an atomic
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
    let started = match request(socket, "POST", start, body) {
        Ok(a) if a.status == 202 => {
            serde_json::from_slice::<TaskStarted>(&a.body).map_err(|e| e.to_string())
        }
        Ok(a) => Err(a.error()),
        Err(e) => Err(e),
    };
    let upid = match started {
        Ok(s) => s.task_id,
        Err(e) => {
            eprintln!("rynasctl: starting task {id}: {e}");
            return ExitCode::FAILURE;
        }
    };
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
        if let Ok(a) = request(
            socket,
            "GET",
            &format!("/v1/tasks/{upid}/log?offset={offset}"),
            None,
        ) && a.status == 200
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
        NetAction::Rollback { reason } => {
            use rynas_net::reason as r;
            if ![r::TIMEOUT, r::BOOT_CHECK, r::MANUAL].contains(&reason.as_str()) {
                eprintln!("rynasctl: unknown reason {reason}");
                return ExitCode::FAILURE;
            }
            match e.rollback(&reason, &log) {
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

fn main() -> ExitCode {
    let args = Args::parse();
    match args.command {
        Command::TaskFinished { id } => task_finished(&args.socket, &id),
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
    }
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
}
