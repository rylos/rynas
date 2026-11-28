//! The service (`rynas-agent run`, a systemd unit): follows the NAS' plan with a long poll,
//! runs a backup when one is due (after boot with the plan's delay, then every
//! `interval_hours`, failed runs again after a growing pause) or when the console says
//! "Back up now", and stops the running one on "Cancel" or a signal. Ideas from abb-agent's
//! `scheduler.rs` (same author).
//!
//! Each backup runs as a child process (`rynas-agent backup --service`): its private mount
//! namespace and its freezes end with it, and a cancel is a SIGTERM (the child thaws first).
//! While it runs, a `systemd-inhibit` lock keeps the PC from sleeping.
//!
//! Wake-on-LAN and power (`crate::power`, `crate::facts`): the PC's facts go to the NAS after
//! the first contact and every 6 h when changed; `Sleep` is refused while a backup runs;
//! `EnableWol` cards are switched on again at every start; a PC the NAS woke for a due backup
//! starts it without the start delay and, after it, does the plan's "after a wake" action when
//! nobody used the PC. Commands are acknowledged as before: only by the next poll (the NAS sees
//! the contact) and `handled_command`; a refusal is a log line.

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

use jiff::tz::TimeZone;
use rynas_api_types::pcagent::{AgentCommand, AgentPlan, AgentReport, PcRun};
use rynas_api_types::scheduler::{RunTrigger, SkipReason};

use crate::backup::client_of;
use crate::state::{State, now_ms};

pub static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
    // only async-signal-safe calls from here on: kill, waitpid, nanosleep, _exit
    if let TermAction::StopChild(pid) = term_action(CHILD_PID.load(Ordering::SeqCst)) {
        // SAFETY: async-signal-safe calls on our own child
        unsafe {
            libc::kill(pid, libc::SIGTERM);
            // the child thaws and exits within seconds; never longer than the unit's stop
            // timeout allows (then it is killed)
            let mut gone = false;
            for _ in 0..CHILD_STOP_TICKS {
                let mut status = 0;
                let r = libc::waitpid(pid, &mut status, libc::WNOHANG);
                // reaped here, or already by the main loop (ECHILD)
                if r == pid || r < 0 {
                    gone = true;
                    break;
                }
                let ts = libc::timespec { tv_sec: 0, tv_nsec: 100_000_000 };
                libc::nanosleep(&ts, std::ptr::null_mut());
            }
            if !gone {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
    let inhibit = INHIBIT_PID.load(Ordering::SeqCst);
    if inhibit > 0 {
        // SAFETY: async-signal-safe signal to our own child
        unsafe {
            libc::kill(inhibit, libc::SIGTERM);
        }
    }
    // the state is written to a temporary file and renamed (`state::write_atomic`): an exit
    // here never leaves half a state, and the lock (flock) ends with the process
    // SAFETY: async-signal-safe exit
    unsafe { libc::_exit(0) };
}

/// The running backup child (0: none): the signal handler stops it.
pub static CHILD_PID: AtomicI32 = AtomicI32::new(0);
/// The running `systemd-inhibit` (0: none).
pub static INHIBIT_PID: AtomicI32 = AtomicI32::new(0);
/// How long the handler waits for the child (100 ms ticks): under the unit's 30 s stop timeout.
const CHILD_STOP_TICKS: u32 = 250;

/// What SIGTERM/SIGINT does: exit at once (nothing runs; the long poll or a pause would keep
/// the PC's shutdown waiting), or stop the backup child first (it thaws on SIGTERM).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermAction {
    ExitNow,
    StopChild(i32),
}

pub fn term_action(child_pid: i32) -> TermAction {
    if child_pid > 0 { TermAction::StopChild(child_pid) } else { TermAction::ExitNow }
}

/// A pause that ends early when the service is told to stop (checked every 500 ms).
pub fn pause(d: Duration) {
    let end = Instant::now() + d;
    while !STOP.load(Ordering::SeqCst) {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        std::thread::sleep(left.min(Duration::from_millis(500)));
    }
}

pub fn install_signals() {
    // SAFETY: a zeroed sigaction with our handler; no SA_RESTART, so blocking calls return
    // EINTR where they can
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_term as *const () as libc::sighandler_t;
        sa.sa_flags = 0;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
    }
}

/// When the next scheduled backup is due (ms), ignoring the pause and the window: by the
/// plan's calendar (its first time after the last good run) or every so many hours after the
/// last good run, and also at every start of the PC when the plan says so; at least the delay
/// after the start; a failed run again after 10, 20, 40… minutes (never later than the
/// interval). The rules are `rynas_schedule::window`'s, the same as the NAS's overview.
pub fn due_ms(st: &State, started_ms: u64) -> u64 {
    due_ms_at(st, started_ms, boot_key().as_deref())
}

/// [`due_ms`] with the key of this boot of the PC given (`None`: unknown, then "also at every
/// start" runs nothing rather than once per restart of the agent).
pub fn due_ms_at(st: &State, started_ms: u64, boot: Option<&str>) -> u64 {
    use rynas_schedule::window;
    let s = st.plan.schedule();
    let interval = u64::from(s.every_h) * 3_600_000;
    let delay = u64::from(s.delay_min) * 60_000;
    // an expression this agent cannot read (it never should: the NAS checks it with the same
    // parser) or without any time left: the interval, so backups go on
    // (the same rule as the NAS's view of this PC: `window::agent_due`)
    let spec = s.calendar.as_deref().and_then(|c| rynas_schedule::CalendarSpec::parse(c).ok());
    // also at every start of the PC: whichever comes first (after the boot's backup the
    // interval counts from it; a calendar's next time still comes)
    let boot_done = s.at_boot.then(|| boot.is_none_or(|b| st.last_ok_boot.as_deref() == Some(b)));
    // the interval from the last good run's start (no drift by the run's length), a calendar
    // from its end: the NAS judges this PC by the same `LastOk`
    let last_ok = st.last_ok_ms.map(|end| window::LastOk::new(st.last_ok_start_ms, end));
    let due = window::agent_due(spec.as_ref(), interval, &zone(&st.plan), last_ok, started_ms, delay, boot_done);
    // a failed run: again after 10, 20, 40… minutes, never later than the interval
    let retry = match (st.failures, st.last_attempt_ms) {
        (n, Some(at)) if n > 0 => at + ((5 * 60_000u64) << n.min(6)).min(interval),
        _ => 0,
    };
    due.max(retry)
}

/// The key of this boot of the PC: the kernel's boot id (`/proc/sys/kernel/random/boot_id`:
/// new at every boot, untouched by clock changes), else `btime` of `/proc/stat`; `None` when
/// neither can be read.
pub fn boot_key() -> Option<String> {
    if let Ok(id) = std::fs::read_to_string("/proc/sys/kernel/random/boot_id") {
        let id = id.trim();
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    std::fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("btime ")?.trim().parse::<u64>().ok()))
        .map(|b| format!("btime:{b}"))
}

/// The zone of the allowed window: the NAS's (sent in the plan: its name, else its POSIX
/// rule when the name is missing or this PC has no zone database), else this PC's, else UTC.
pub fn zone(plan: &AgentPlan) -> TimeZone {
    plan.tz
        .as_deref()
        .and_then(|z| TimeZone::get(z).ok())
        .or_else(|| plan.tz_posix.as_deref().and_then(|p| TimeZone::posix(p).ok()))
        .or_else(|| TimeZone::try_system().ok())
        .unwrap_or(TimeZone::UTC)
}

/// When the next scheduled backup starts (ms), not before `now_ms`: when it is due, or when
/// the allowed window opens after that (a backup overdue while the window is closed starts
/// when it opens next, not at its past due time). `None` when paused.
pub fn next_due(st: &State, started_ms: u64, now_ms: u64) -> Option<u64> {
    if st.plan.plan.paused {
        return None;
    }
    let s = st.plan.schedule();
    rynas_schedule::window::start_at(s.window.as_deref(), &zone(&st.plan), due_ms(st, started_ms).max(now_ms))
}

/// What the service does about the schedule now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nothing due (or a skipped start already reported for this wait).
    Idle,
    /// A backup starts.
    Start(RunTrigger),
    /// A backup is due but cannot start: reported to the NAS once per wait (and again after
    /// a day, or the interval when longer, of waiting, or for another reason).
    Skip(SkipReason),
}

/// [`Decision`] at `now_ms`. Skipped starts are reported only to a NAS that sent the shared
/// trigger (an older one would count them as failed runs).
pub fn decide(st: &State, started_ms: u64, now_ms: u64, tz: &TimeZone) -> Decision {
    let s = st.plan.schedule();
    if now_ms < due_ms(st, started_ms) {
        return Decision::Idle;
    }
    let again = (u64::from(s.every_h) * 3_600_000).max(86_400_000);
    let may_report = |reason: SkipReason| {
        s.shared && st.skip_noted.is_none_or(|(at, r)| r != reason || now_ms >= at.saturating_add(again))
    };
    let blocked = if st.plan.plan.paused {
        Some(SkipReason::Paused)
    } else if let Some(w) = s.window.as_deref()
        && !rynas_schedule::window::is_open(w, tz, now_ms)
    {
        Some(SkipReason::Window)
    } else {
        None
    };
    match blocked {
        Some(r) if may_report(r) => Decision::Skip(r),
        Some(_) => Decision::Idle,
        None if st.failures > 0 => Decision::Start(RunTrigger::Retry),
        None => Decision::Start(RunTrigger::Timer),
    }
}

/// A skipped start, told to the NAS (its scheduler history): not a run.
fn report_skip(st: &State, reason: SkipReason, now: u64) -> Result<(), String> {
    let run = PcRun {
        run_id: format!("skip-{now}"),
        started_ms: now,
        ended_ms: now,
        trigger: Some(RunTrigger::Timer),
        skipped: Some(reason),
        ..PcRun::default()
    };
    let r = AgentReport { run, manifest: None, agent_version: env!("CARGO_PKG_VERSION").into(), root_btrfs: None };
    client_of(st)?.post_json::<_, serde_json::Value>("/api/v1/pcbackup/agent/report", &r).map(|_| ())
}

struct Running {
    child: Child,
    inhibit: Option<Child>,
    since: Instant,
}

/// How the service runs: for real (`rynas-agent run`), or the hidden test of the schedule
/// (`schedule-test`: a stand-in run instead of a backup, for a while).
#[derive(Debug, Clone, Copy, Default)]
pub struct Mode {
    /// Each run is `rynas-agent test-run` (it only reports a run), and no sleep lock.
    pub fake: bool,
    /// The service ends after this many seconds.
    pub for_s: Option<u64>,
}

fn trigger_word(t: RunTrigger) -> &'static str {
    match t {
        RunTrigger::Manual => "manual",
        RunTrigger::Retry => "retry",
        _ => "timer",
    }
}

/// The `--trigger` of a run (`timer` when absent or unknown).
pub fn trigger_of(word: Option<&str>) -> RunTrigger {
    match word {
        Some("manual") => RunTrigger::Manual,
        Some("retry") => RunTrigger::Retry,
        _ => RunTrigger::Timer,
    }
}

fn start_backup(trigger: RunTrigger, mode: Mode) -> Result<Running, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let args: &[&str] = if mode.fake { &["test-run"] } else { &["backup", "--service"] };
    let child = Command::new(&exe)
        .args(args)
        .args(["--trigger", trigger_word(trigger)])
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start the backup: {e}"))?;
    CHILD_PID.store(child.id() as i32, Ordering::SeqCst);
    if mode.fake {
        return Ok(Running { child, inhibit: None, since: Instant::now() });
    }
    // no sleep while it runs (when systemd-logind is there)
    let inhibit = Command::new("systemd-inhibit")
        .args([
            "--what=sleep:idle",
            "--who=rynas-agent",
            "--why=PC backup to the NAS",
            "--mode=block",
            "sleep",
            "infinity",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok();
    if let Some(i) = &inhibit {
        INHIBIT_PID.store(i.id() as i32, Ordering::SeqCst);
    }
    Ok(Running {
        child,
        inhibit,
        since: Instant::now(),
    })
}

fn stop_child(r: &mut Running) {
    // SAFETY: a signal to our own child
    unsafe {
        libc::kill(r.child.id() as i32, libc::SIGTERM);
    }
    for _ in 0..600 {
        if r.child.try_wait().ok().flatten().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = r.child.kill();
    let _ = r.child.wait();
    CHILD_PID.store(0, Ordering::SeqCst);
}

fn end_inhibit(r: &mut Running) {
    if let Some(i) = &mut r.inhibit {
        INHIBIT_PID.store(0, Ordering::SeqCst);
        let _ = i.kill();
        let _ = i.wait();
    }
}

fn poll(st: &State, wait: u64) -> Result<(AgentPlan, String), String> {
    let c = client_of(st)?;
    let v: serde_json::Value = c.get_json(&format!("/api/v1/pcbackup/agent/plan?rev={}&wait={wait}", st.plan.rev))?;
    let (plan, note) = parse_plan(v)?;
    if let Some(n) = note {
        eprintln!("{n}");
    }
    Ok((plan, c.ep().url()))
}

/// The plan from the NAS' answer. A command this agent does not know (a newer NAS) would make
/// the whole plan unreadable: the plan is read without it then, with a note for the log.
pub fn parse_plan(mut v: serde_json::Value) -> Result<(AgentPlan, Option<String>), String> {
    match serde_json::from_value::<AgentPlan>(v.clone()) {
        Ok(p) => Ok((p, None)),
        Err(e) => {
            let cmd = v.as_object_mut().and_then(|o| o.remove("command"));
            let Some(cmd) = cmd else {
                return Err(format!("bad answer from the NAS: {e}"));
            };
            let p = serde_json::from_value::<AgentPlan>(v).map_err(|_| format!("bad answer from the NAS: {e}"))?;
            let kind = cmd.get("kind").and_then(|k| k.as_str()).unwrap_or("?").to_string();
            Ok((p, Some(format!("the NAS sent a command this agent does not know ({kind}): ignored; update rynas-agent"))))
        }
    }
}

/// The start of the service as the schedule sees it: a PC the NAS woke recently for a due
/// backup ([`crate::power::recent_wake`]) starts it without the plan's start delay.
pub fn effective_start(st: &State, started_ms: u64, now_ms: u64) -> u64 {
    if crate::power::recent_wake(&st.plan, now_ms).is_some() {
        started_ms.saturating_sub(u64::from(st.plan.schedule().delay_min) * 60_000)
    } else {
        started_ms
    }
}

/// The trust and addresses from the NAS (`trust::refresh`), saved; when the next refresh is
/// due.
fn refresh_trust(st: &mut State) -> u64 {
    match crate::trust::refresh(st) {
        Ok(r) => {
            if r == crate::trust::Refreshed::Upgraded {
                eprintln!("the NAS' certificate authority is pinned now: a new name, address or certificate of the NAS needs no pairing again");
            }
            if r == crate::trust::Refreshed::Cleartext {
                eprintln!("the NAS is reached over plain http: nothing it answers is trusted (pair this PC again over https for a NAS that can change its name or address)");
            }
            if r != crate::trust::Refreshed::Same {
                let (nas_id, cas, addresses, url, fingerprint, checked) =
                    (st.nas_id.clone(), st.cas.clone(), st.addresses.clone(), st.url.clone(), st.fingerprint.clone(), st.trust_checked_ms);
                let _ = State::update(|s| {
                    s.nas_id = nas_id;
                    s.cas = cas;
                    s.addresses = addresses;
                    s.url = url;
                    s.fingerprint = fingerprint;
                    s.trust_checked_ms = checked;
                });
            }
            now_ms() + crate::trust::REFRESH_EVERY_MS
        }
        Err(e) => {
            eprintln!("NAS certificate authority: {e}");
            now_ms() + crate::trust::RETRY_MS
        }
    }
}

/// The outcome of the last run as the state on disk has it (written by the service's child,
/// or by a backup run by hand), taken into the service's copy. A new good backup also means
/// this boot of the PC has had its backup.
fn take_outcome(st: &mut State, fresh: &State) {
    if fresh.last_ok_ms.is_some() && fresh.last_ok_ms != st.last_ok_ms {
        let boot = boot_key();
        st.last_ok_boot = boot.clone();
        let _ = State::update(|s| s.last_ok_boot = boot);
    }
    st.last_ok_ms = fresh.last_ok_ms;
    st.last_ok_start_ms = fresh.last_ok_start_ms;
    st.last_attempt_ms = fresh.last_attempt_ms;
    st.failures = fresh.failures;
}

pub fn run_service(mode: Mode) -> Result<(), String> {
    install_signals();
    if !mode.fake {
        crate::backup::cleanup_leftovers();
    }
    let started = now_ms();
    let mut st = State::load()?;
    eprintln!("rynas-agent {}: PC {} on {}", env!("CARGO_PKG_VERSION"), st.device, st.url);
    // the schedule test never changes the PC's network or power: it logs instead
    let exec: Box<dyn crate::power::Exec> = if mode.fake { Box::new(crate::power::DryRun) } else { Box::new(crate::power::System) };
    let sys = std::path::Path::new("/sys");
    // magic-packet wake kept on: drivers reset it at boot
    let kept = crate::power::existing_nics(sys, &st.wol_kept);
    if !kept.is_empty() {
        let on = crate::power::enable_wol(exec.as_ref(), &kept);
        if !on.is_empty() {
            eprintln!("Wake-on-LAN kept on: {}", on.join(", "));
        }
    }
    let mut facts = crate::facts::Sender::default();
    let root_btrfs = crate::btrfs::root_is_btrfs();
    let mut running: Option<Running> = None;
    // the wake by the NAS the running backup was started for
    let mut woken_run: Option<u64> = None;
    let mut last_poll_error: Option<Instant> = None;
    // the trust and addresses: at the first contact, then every few hours
    let mut trust_due = 0u64;
    let end = mode.for_s.map(|s| Instant::now() + Duration::from_secs(s));
    while !STOP.load(Ordering::SeqCst) && end.is_none_or(|e| Instant::now() < e) {
        // a short poll while a backup runs (to see Cancel), a long one otherwise, ending when
        // the next scheduled start comes (its due time, or the window opening)
        let wait = if running.is_some() {
            5
        } else {
            next_due(&st, effective_start(&st, started, now_ms()), now_ms()).map_or(50, |d| (d.saturating_sub(now_ms()) / 1000).clamp(5, 50))
        };
        let wait = end.map_or(wait, |e| wait.min(e.saturating_duration_since(Instant::now()).as_secs().max(1)));
        match poll(&st, wait) {
            Ok((plan, url)) => {
                // the whole plan: a NAS whose zone changed sends the same revision
                if plan.rev != st.plan.rev || plan != st.plan {
                    st.plan = plan.clone();
                    let plan = plan.clone();
                    let _ = State::update(|s| s.plan = plan);
                }
                // another known address answered: tried first from now on
                if url != st.url {
                    eprintln!("the NAS answers at {url}");
                    st.url = url.clone();
                    let _ = State::update(|s| s.url = url);
                }
                // never while a backup runs: a Cancel must not wait behind it
                if running.is_none() && now_ms() >= trust_due {
                    trust_due = refresh_trust(&mut st);
                }
                match plan.command {
                    Some(AgentCommand::BackupNow { id }) if id > st.handled_command => {
                        st.handled_command = id;
                        let _ = State::update(|s| s.handled_command = id);
                        if running.is_none() {
                            eprintln!("back up now (from the NAS)");
                            running = start_backup(RunTrigger::Manual, mode).map_err(|e| eprintln!("{e}")).ok();
                            woken_run = None;
                        }
                    }
                    Some(AgentCommand::Cancel { id }) if id > st.handled_command => {
                        st.handled_command = id;
                        let _ = State::update(|s| s.handled_command = id);
                        if let Some(r) = &mut running {
                            eprintln!("cancel (from the NAS)");
                            stop_child(r);
                        }
                    }
                    // handled before it is carried out: after the PC wakes again the same
                    // command (still in the plan) does not put it to sleep once more
                    Some(AgentCommand::Sleep { id, mode: sleep }) if id > st.handled_command => {
                        st.handled_command = id;
                        let _ = State::update(|s| s.handled_command = id);
                        match crate::power::sleep_decision(sleep, running.is_some()) {
                            crate::power::SleepDecision::RefuseBackup => {
                                eprintln!("{} (from the NAS) refused: a backup is running", crate::power::verb(sleep));
                            }
                            crate::power::SleepDecision::RefuseUnknown => {
                                eprintln!("a sleep mode this agent does not know (from the NAS): refused; update rynas-agent");
                            }
                            crate::power::SleepDecision::Go(m) => {
                                eprintln!("{} (from the NAS)", crate::power::verb(m));
                                if let Err(e) = crate::power::sleep_now(exec.as_ref(), m) {
                                    eprintln!("{}: {e}", crate::power::verb(m));
                                }
                            }
                        }
                    }
                    Some(AgentCommand::EnableWol { id, nics }) if id > st.handled_command => {
                        st.handled_command = id;
                        let present = crate::power::existing_nics(sys, &nics);
                        let missing: Vec<&String> = nics.iter().filter(|n| !present.contains(n)).collect();
                        if !missing.is_empty() {
                            eprintln!("Wake-on-LAN: no card {missing:?} on this PC");
                        }
                        let on = crate::power::enable_wol(exec.as_ref(), &present);
                        if !on.is_empty() {
                            eprintln!("Wake-on-LAN on (from the NAS), kept at every start: {}", on.join(", "));
                        }
                        st.wol_kept = crate::power::merge_kept(&st.wol_kept, &on);
                        let kept = st.wol_kept.clone();
                        let _ = State::update(|s| {
                            s.handled_command = id;
                            s.wol_kept = kept;
                        });
                        facts.soon();
                    }
                    _ => {}
                }
                // the PC's facts: after the first good contact, then every 6 h when changed
                let now = now_ms();
                if facts.addresses_due(now) {
                    facts.check_addresses(now, &crate::facts::ipv4_by_name());
                }
                if facts.due(now) {
                    let f = crate::facts::collect(exec.as_ref(), sys, &crate::facts::ipv4_by_name(), &st.wol_kept, root_btrfs);
                    facts.tick(now, f, |f| client_of(&st).and_then(|c| crate::facts::post(&c, f)));
                }
                last_poll_error = None;
            }
            Err(e) => {
                if e.contains("401") || e.contains("revoked") {
                    eprintln!("the NAS no longer knows this PC ({e}): pair it again");
                }
                if last_poll_error.is_none_or(|t| t.elapsed() > Duration::from_secs(600)) {
                    eprintln!("NAS: {e}");
                    last_poll_error = Some(Instant::now());
                }
                pause(Duration::from_secs(30));
            }
        }
        // the running backup
        if let Some(r) = &mut running
            && let Ok(Some(status)) = r.child.try_wait()
        {
            CHILD_PID.store(0, Ordering::SeqCst);
            end_inhibit(r);
            let took = r.since.elapsed().as_secs();
            // the child wrote the outcome into the state
            let mut fresh = State::load();
            // it ended without one (stopped by Cancel, killed, or failed before its run):
            // a failed run, tried again after the retry pause, never at once in a loop
            if !status.success()
                && let Ok(f) = &fresh
                && f.last_ok_ms == st.last_ok_ms
                && f.failures == st.failures
            {
                let at = now_ms();
                fresh = State::update(|s| {
                    s.failures += 1;
                    s.last_attempt_ms = Some(at);
                });
            }
            // what a stopped or killed child left (snapshots, a frozen filesystem): now, not
            // at the next run
            if !status.success() && !mode.fake {
                crate::backup::cleanup_leftovers();
            }
            if let Ok(fresh) = fresh {
                take_outcome(&mut st, &fresh);
            }
            eprintln!("backup finished ({status}) in {took} s");
            running = None;
            after_wake(&mut st, woken_run.take(), exec.as_ref());
        }
        if running.is_none() {
            // a backup run by hand (`rynas-agent backup` in a terminal) wrote its outcome
            // too: a good one is not made again
            if let Ok(fresh) = State::load() {
                take_outcome(&mut st, &fresh);
            }
            let now = now_ms();
            match decide(&st, effective_start(&st, started, now), now, &zone(&st.plan)) {
                Decision::Idle => {}
                Decision::Start(trigger) => {
                    woken_run = crate::power::recent_wake(&st.plan, now);
                    if woken_run.is_some() {
                        eprintln!("scheduled backup (the NAS woke this PC for it)");
                    } else {
                        eprintln!("scheduled backup");
                    }
                    running = start_backup(trigger, mode).map_err(|e| eprintln!("{e}")).ok();
                    if running.is_none() {
                        woken_run = None;
                    }
                    if running.is_some() && st.skip_noted.is_some() {
                        st.skip_noted = None;
                        let _ = State::update(|s| s.skip_noted = None);
                    }
                }
                Decision::Skip(reason) => {
                    match reason {
                        SkipReason::Paused => eprintln!("a backup is due, but scheduled backups are paused"),
                        _ => eprintln!("a backup is due outside the allowed window: it starts when the window opens"),
                    }
                    if let Err(e) = report_skip(&st, reason, now) {
                        eprintln!("report to the NAS: {e}");
                    }
                    // noted even when the NAS did not answer: one report per wait
                    st.skip_noted = Some((now, reason));
                    let _ = State::update(|s| s.skip_noted = Some((now, reason)));
                }
            }
        }
    }
    if let Some(r) = &mut running {
        stop_child(r);
        end_inhibit(r);
    }
    Ok(())
}

/// After the backup the NAS woke the PC for (good or failed): the plan's "after a wake"
/// action, once per wake, only when nobody used the PC since the wake
/// ([`crate::power::may_sleep_after_wake`]).
fn after_wake(st: &mut State, woken: Option<u64>, exec: &dyn crate::power::Exec) {
    let Some(mode) = crate::power::after_wake_mode(st.plan.plan.after_wake, woken, st.after_wake_done) else {
        return;
    };
    let Some(w) = woken else { return };
    // decided once, whatever comes of it
    st.after_wake_done = woken;
    let _ = State::update(|s| s.after_wake_done = woken);
    let verb = crate::power::verb(mode);
    let Some(sessions) = crate::power::sessions_now(exec) else {
        eprintln!("after the wake: the PC stays on (its sessions cannot be read)");
        return;
    };
    if !crate::power::may_sleep_after_wake(mode, &sessions, crate::power::wake_time(w)) {
        eprintln!("after the wake: the PC stays on (someone is logged in or used it since the wake)");
        return;
    }
    eprintln!("after the wake: {verb} (nobody used the PC since the wake)");
    if let Err(e) = crate::power::sleep_now(exec, mode) {
        eprintln!("{verb}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::scheduler::TaskTrigger;

    #[test]
    fn due_times() {
        let mut st = State::default();
        st.plan.plan.interval_hours = 24;
        st.plan.plan.start_delay_min = 10;
        // never backed up: after the start delay
        assert_eq!(next_due(&st, 1_000_000, 0), Some(1_000_000 + 600_000));
        st.last_ok_ms = Some(10_000_000);
        assert_eq!(next_due(&st, 0, 0), Some(10_000_000 + 86_400_000));
        st.failures = 1;
        st.last_attempt_ms = Some(200_000_000);
        assert_eq!(next_due(&st, 0, 0), Some(200_000_000 + 600_000));
        st.plan.plan.paused = true;
        assert_eq!(next_due(&st, 0, 0), None);
    }

    fn rome_ms(s: &str) -> u64 {
        let dt: jiff::civil::DateTime = s.parse().unwrap();
        TimeZone::get("Europe/Rome").unwrap().to_ambiguous_zoned(dt).compatible().unwrap().timestamp().as_millisecond() as u64
    }

    /// 22:00–24:00 every day.
    fn night() -> String {
        let mut w = vec![b'0'; 168];
        for d in 0..7 {
            w[d * 24 + 22] = b'1';
            w[d * 24 + 23] = b'1';
        }
        String::from_utf8(w).unwrap()
    }

    /// S6 (R5): an older NAS (no trigger) = the old fields and no window, even when the plan
    /// carries one; a new NAS = the trigger and its window, in the NAS's zone.
    #[test]
    fn the_window_and_older_nas() {
        let tz = TimeZone::get("Europe/Rome").unwrap();
        let mut st = State::default();
        st.plan.plan.interval_hours = 6;
        st.plan.plan.start_delay_min = 0;
        st.plan.plan.window = Some(night());
        st.last_ok_ms = Some(rome_ms("2026-11-17T08:00"));
        let afternoon = rome_ms("2026-11-17T15:00");
        // older NAS: due at 14:00, started at once, no skipped starts ever reported
        assert_eq!(next_due(&st, 0, 0), Some(rome_ms("2026-11-17T14:00")));
        assert_eq!(decide(&st, 0, afternoon, &tz), Decision::Start(RunTrigger::Timer));
        st.plan.plan.paused = true;
        assert_eq!(decide(&st, 0, afternoon, &tz), Decision::Idle);
        st.plan.plan.paused = false;
        // new NAS: the trigger wins over the old fields; waits for 22:00 in the NAS's zone
        st.plan.trigger = Some(TaskTrigger::Interval { hours: 6, after_boot_min: 0 });
        st.plan.tz = Some("Europe/Rome".into());
        st.plan.plan.interval_hours = 99;
        assert_eq!(next_due(&st, 0, 0), Some(rome_ms("2026-11-17T22:00")));
        // overdue since the night and the window closed now: the next opening, never the past
        assert_eq!(next_due(&st, 0, rome_ms("2026-11-18T09:00")), Some(rome_ms("2026-11-18T22:00")));
        assert_eq!(decide(&st, 0, afternoon, &tz), Decision::Skip(SkipReason::Window));
        // reported once per wait
        st.skip_noted = Some((afternoon, SkipReason::Window));
        assert_eq!(decide(&st, 0, afternoon + 3_600_000, &tz), Decision::Idle);
        // the window opens: the backup starts (a retry after a failure)
        assert_eq!(decide(&st, 0, rome_ms("2026-11-17T22:00"), &tz), Decision::Start(RunTrigger::Timer));
        st.failures = 1;
        st.last_attempt_ms = Some(rome_ms("2026-11-17T22:00"));
        assert_eq!(decide(&st, 0, rome_ms("2026-11-17T22:05"), &tz), Decision::Idle);
        assert_eq!(decide(&st, 0, rome_ms("2026-11-17T22:10"), &tz), Decision::Start(RunTrigger::Retry));
        // a retry due after the window closed waits for the next night
        assert_eq!(decide(&st, 0, rome_ms("2026-11-18T00:10"), &tz), Decision::Idle);
        // still waiting a day later: reported again; another reason at once
        assert_eq!(decide(&st, 0, afternoon + 86_400_000, &tz), Decision::Skip(SkipReason::Window));
        st.plan.plan.paused = true;
        assert_eq!(decide(&st, 0, afternoon + 86_400_000 + 60_000, &tz), Decision::Skip(SkipReason::Paused));
        assert_eq!(next_due(&st, 0, 0), None);
        // the zone: the NAS's, else the PC's
        assert_eq!(zone(&st.plan).iana_name(), Some("Europe/Rome"));
        st.plan.tz = Some("Not/AZone".into());
        assert_eq!(zone(&st.plan), TimeZone::try_system().unwrap_or(TimeZone::UTC));
        // no name the PC knows: the NAS's POSIX rule
        st.plan.tz_posix = Some("CET-1CEST,M3.5.0,M10.5.0/3".into());
        let z = zone(&st.plan);
        assert!(!rynas_schedule::window::is_open(&night(), &z, rome_ms("2026-07-01T21:30")));
        assert!(rynas_schedule::window::is_open(&night(), &z, rome_ms("2026-07-01T22:30")));
    }

    #[test]
    fn triggers_of_runs() {
        assert_eq!(trigger_of(Some("manual")), RunTrigger::Manual);
        assert_eq!(trigger_of(Some("retry")), RunTrigger::Retry);
        assert_eq!(trigger_of(None), RunTrigger::Timer);
        for t in [RunTrigger::Manual, RunTrigger::Retry, RunTrigger::Timer] {
            assert_eq!(trigger_of(Some(trigger_word(t))), t);
        }
    }

    /// A command from a newer NAS: the plan is still read, without it.
    #[test]
    fn unknown_commands_keep_the_plan() {
        let mut v = serde_json::to_value(AgentPlan { rev: 7, ..Default::default() }).unwrap();
        v["command"] = serde_json::json!({ "kind": "sleep", "id": 3, "mode": "suspend" });
        let (p, note) = parse_plan(v.clone()).unwrap();
        assert_eq!(p.command, Some(AgentCommand::Sleep { id: 3, mode: rynas_api_types::pcagent::SleepMode::Suspend }));
        assert!(note.is_none());
        v["command"] = serde_json::json!({ "kind": "reboot_into_mars", "id": 4 });
        let (p, note) = parse_plan(v).unwrap();
        assert_eq!((p.rev, p.command), (7, None));
        assert!(note.unwrap().contains("reboot_into_mars"));
        assert!(parse_plan(serde_json::json!({ "rev": "x" })).is_err());
    }

    /// SIGTERM: exit at once when idle (the PC's shutdown never waits for the long poll), the
    /// backup child first when one runs.
    #[test]
    fn stopping_the_service() {
        assert_eq!(term_action(0), TermAction::ExitNow);
        assert_eq!(term_action(-1), TermAction::ExitNow);
        assert_eq!(term_action(4242), TermAction::StopChild(4242));
        // the pauses of the loop are short (STOP is never set in tests: it would stop others)
        let t = Instant::now();
        pause(Duration::from_millis(50));
        assert!(t.elapsed() < Duration::from_millis(600));
    }

    /// A PC the NAS woke starts its due backup without the start delay (within 30 min).
    #[test]
    fn woken_pcs_skip_the_start_delay() {
        let tz = TimeZone::UTC;
        let mut st = State::default();
        st.plan.plan.interval_hours = 24;
        st.plan.plan.start_delay_min = 10;
        let started = 100_000_000_000;
        st.last_ok_ms = Some(started - 2 * 86_400_000);
        assert_eq!(decide(&st, effective_start(&st, started, started + 1000), started + 1000, &tz), Decision::Idle);
        st.plan.woken_ms = Some(started - 60_000);
        assert_eq!(effective_start(&st, started, started + 1000), started - 600_000);
        assert_eq!(decide(&st, effective_start(&st, started, started + 1000), started + 1000, &tz), Decision::Start(RunTrigger::Timer));
        // an old wake: the delay again
        assert_eq!(effective_start(&st, started, started + 31 * 60_000), started);
        // woken, but nothing due: nothing starts
        st.last_ok_ms = Some(started - 3_600_000);
        assert_eq!(decide(&st, effective_start(&st, started, started + 1000), started + 1000, &tz), Decision::Idle);
    }

    /// Set times (0.8.21): a time missed while the PC was off is caught up once, the delay
    /// after the agent's start; a NAS that sends the calendar is followed, the plan's interval
    /// (the calendar's spacing, for older agents) is not.
    #[test]
    fn calendar_plans() {
        let tz = TimeZone::get("Europe/Rome").unwrap();
        let mut st = State::default();
        st.plan.tz = Some("Europe/Rome".into());
        st.plan.plan.interval_hours = 24;
        st.plan.plan.start_delay_min = 10;
        st.plan.trigger = Some(TaskTrigger::Calendar { on_calendar: "*-*-* 02:00:00".into(), cron: None, persistent: true, randomized_delay_s: 0 });
        // last good backup yesterday 02:05; the PC starts today at 08:00 (02:00 missed)
        st.last_ok_ms = Some(rome_ms("2026-11-16T02:05"));
        let start = rome_ms("2026-11-17T08:00");
        assert_eq!(due_ms_at(&st, start, None), start + 600_000);
        assert_eq!(decide(&st, start, start + 300_000, &tz), Decision::Idle);
        assert_eq!(decide(&st, start, start + 600_000, &tz), Decision::Start(RunTrigger::Timer));
        // that run ends at 08:12: tomorrow at 02:00, no second catch-up
        st.last_ok_ms = Some(start + 720_000);
        assert_eq!(next_due(&st, start, start + 720_000), Some(rome_ms("2026-11-18T02:00")));
        assert_eq!(decide(&st, start, rome_ms("2026-11-17T23:00"), &tz), Decision::Idle);
        // with a window 22–24 the 02:00 time is outside it: the next opening
        st.plan.plan.window = Some(night());
        assert_eq!(next_due(&st, start, start + 720_000), Some(rome_ms("2026-11-18T22:00")));
    }

    #[test]
    fn calendar_catch_up_runs_once_now() {
        let tz = TimeZone::get("Europe/Rome").unwrap();
        let mut st = State::default();
        st.plan.tz = Some("Europe/Rome".into());
        st.plan.plan.interval_hours = 24;
        st.plan.plan.start_delay_min = 1;
        st.plan.trigger = Some(TaskTrigger::Calendar { on_calendar: "*-*-* 03:00:00".into(), cron: None, persistent: true, randomized_delay_s: 0 });
        // a fixed noon: with the clock the next 03:00 is under an hour away between 02:00 and 03:00
        let t = rome_ms("2026-11-18T12:00");
        st.last_ok_ms = Some(t - 2 * 86_400_000);
        assert_eq!(decide(&st, t, t + 61_000, &tz), Decision::Start(RunTrigger::Timer));
        st.last_ok_ms = Some(t + 63_000);
        let d = due_ms_at(&st, t, None);
        assert!(d > t + 3_600_000, "due {} s after start", (d - t) / 1000);
        assert_eq!(decide(&st, t, t + 68_000, &tz), Decision::Idle);
    }

    /// Also at every start of the PC (0.8.21, on top of the interval or the calendar): the
    /// delay after start, however recent the last backup of an earlier boot; once per boot (keyed
    /// by the boot id: a restart of the agent alone runs nothing); the interval then counts from
    /// that backup, a calendar's next time still comes; an unknown boot runs nothing extra.
    #[test]
    fn also_at_every_start() {
        let mut st = State::default();
        st.plan.plan.start_delay_min = 5;
        st.plan.plan.interval_hours = 24;
        st.plan.plan.at_boot = true;
        st.plan.trigger = Some(TaskTrigger::Interval { hours: 24, after_boot_min: 5 });
        let boot = rome_ms("2026-11-17T08:00");
        let started = boot + 20_000;
        let this = Some("boot-b");
        // backed up 10 minutes before this boot (in boot a): due anyway, after the delay
        st.last_ok_ms = Some(boot - 600_000);
        st.last_ok_boot = Some("boot-a".into());
        assert_eq!(due_ms_at(&st, started, this), started + 300_000);
        // without the switch: 24 h after that backup; the boot unknown: nothing extra
        st.plan.plan.at_boot = false;
        assert_eq!(due_ms_at(&st, started, this), boot - 600_000 + 24 * 3_600_000);
        st.plan.plan.at_boot = true;
        assert_eq!(due_ms_at(&st, started, None), boot - 600_000 + 24 * 3_600_000);
        // the backup of this boot done (08:12): the agent restarted at 11:00 runs nothing new;
        // the interval counts from that backup
        st.last_ok_ms = Some(started + 400_000);
        st.last_ok_boot = Some("boot-b".into());
        assert_eq!(due_ms_at(&st, rome_ms("2026-11-17T11:00"), this), started + 400_000 + 24 * 3_600_000);
        // with a calendar at 20:00: the boot's backup does not replace it
        st.plan.trigger = Some(TaskTrigger::Calendar { on_calendar: "*-*-* 20:00:00".into(), cron: None, persistent: true, randomized_delay_s: 0 });
        st.plan.tz = Some("Europe/Rome".into());
        assert_eq!(due_ms_at(&st, started, this), rome_ms("2026-11-17T20:00"));
        // a calendar with no time left: the interval, backups go on
        st.plan.trigger = Some(TaskTrigger::Calendar { on_calendar: "2020-01-01 00:00:00".into(), cron: None, persistent: true, randomized_delay_s: 0 });
        assert_eq!(due_ms_at(&st, started, this), started + 400_000 + 24 * 3_600_000);
        st.plan.trigger = Some(TaskTrigger::Interval { hours: 24, after_boot_min: 5 });
        // a failed boot backup is tried again (10, 20, 40… minutes)
        st.last_ok_ms = Some(boot - 600_000);
        st.last_ok_boot = Some("boot-a".into());
        st.failures = 1;
        st.last_attempt_ms = Some(started + 300_000);
        assert_eq!(due_ms_at(&st, started, this), started + 300_000 + 600_000);
        // the key of this boot can be read here
        assert!(boot_key().is_some());
    }
}
