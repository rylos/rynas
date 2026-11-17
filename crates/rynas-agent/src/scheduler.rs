//! The service (`rynas-agent run`, a systemd unit): follows the NAS' plan with a long poll,
//! runs a backup when one is due (after boot with the plan's delay, then every
//! `interval_hours`, failed runs again after a growing pause) or when the console says
//! "Back up now", and stops the running one on "Cancel" or a signal. Ideas from abb-agent's
//! `scheduler.rs` (same author).
//!
//! Each backup runs as a child process (`rynas-agent backup --service`): its private mount
//! namespace and its freezes end with it, and a cancel is a SIGTERM (the child thaws first).
//! While it runs, a `systemd-inhibit` lock keeps the PC from sleeping.

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use jiff::tz::TimeZone;
use rynas_api_types::pcagent::{AgentCommand, AgentPlan, AgentReport, PcRun};
use rynas_api_types::scheduler::{RunTrigger, SkipReason};

use crate::backup::client_of;
use crate::state::{State, now_ms};

pub static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

pub fn install_signals() {
    // SAFETY: the handler only stores an atomic
    unsafe {
        libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_term as *const () as libc::sighandler_t);
    }
}

/// When the next scheduled backup is due (ms), ignoring the pause and the window: by the
/// plan's calendar (its first time after the last good run) or every so many hours after the
/// last good run, and also at every start of the PC when the plan says so; at least the delay after the start; a failed
/// run again after 5, 10, 20… minutes (never later than the interval). The rules are
/// `rynas_schedule::window`'s, the same as the NAS's overview.
pub fn due_ms(st: &State, started_ms: u64) -> u64 {
    // the boot unknown: the agent's start stands for it
    let boot = boot_ms();
    due_ms_at(st, started_ms, if boot == 0 { started_ms } else { boot })
}

/// [`due_ms`] with the PC's boot time given.
pub fn due_ms_at(st: &State, started_ms: u64, boot_ms: u64) -> u64 {
    use rynas_schedule::window;
    let s = st.plan.schedule();
    let interval = u64::from(s.every_h) * 3_600_000;
    let delay = u64::from(s.delay_min) * 60_000;
    // an expression this agent cannot read (it never should): the interval
    let spec = s.calendar.as_deref().and_then(|c| rynas_schedule::CalendarSpec::parse(c).ok());
    let mut due = window::plan_due(spec.as_ref(), interval, &zone(&st.plan), st.last_ok_ms, started_ms, delay);
    // also at every start of the PC: whichever comes first (after the boot's backup the
    // interval counts from it; a calendar's next time still comes)
    if s.at_boot {
        due = due.min(window::boot_due(boot_ms, st.last_ok_ms, started_ms, delay));
    }
    // a failed run: again after 5, 10, 20… minutes, never later than the interval
    let retry = match (st.failures, st.last_attempt_ms) {
        (n, Some(at)) if n > 0 => at + ((5 * 60_000u64) << n.min(6)).min(interval),
        _ => 0,
    };
    due.max(retry)
}

/// When the PC booted (ms since the epoch: `btime` of `/proc/stat`), 0 when unknown.
pub fn boot_ms() -> u64 {
    std::fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("btime ")?.trim().parse::<u64>().ok()))
        .map_or(0, |s| s * 1000)
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
}

fn end_inhibit(r: &mut Running) {
    if let Some(i) = &mut r.inhibit {
        let _ = i.kill();
        let _ = i.wait();
    }
}

/// The plan, waiting up to `wait` s for a change; and the NAS address that answered.
fn poll(st: &State, wait: u64) -> Result<(AgentPlan, String), String> {
    let c = client_of(st)?;
    let plan = c.get_json(&format!("/api/v1/pcbackup/agent/plan?rev={}&wait={wait}", st.plan.rev))?;
    Ok((plan, c.ep().url()))
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

pub fn run_service(mode: Mode) -> Result<(), String> {
    install_signals();
    if !mode.fake {
        crate::backup::cleanup_leftovers();
    }
    let started = now_ms();
    let mut st = State::load()?;
    eprintln!("rynas-agent {}: PC {} on {}", env!("CARGO_PKG_VERSION"), st.device, st.url);
    let mut running: Option<Running> = None;
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
            next_due(&st, started, now_ms()).map_or(50, |d| (d.saturating_sub(now_ms()) / 1000).clamp(5, 50))
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
                    _ => {}
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
                std::thread::sleep(Duration::from_secs(30));
            }
        }
        // the running backup
        if let Some(r) = &mut running
            && let Ok(Some(status)) = r.child.try_wait()
        {
            end_inhibit(r);
            let took = r.since.elapsed().as_secs();
            // the child wrote the outcome into the state
            if let Ok(fresh) = State::load() {
                st.last_ok_ms = fresh.last_ok_ms;
                st.last_attempt_ms = fresh.last_attempt_ms;
                st.failures = fresh.failures;
            }
            eprintln!("backup finished ({status}) in {took} s");
            running = None;
        }
        if running.is_none() {
            let now = now_ms();
            match decide(&st, started, now, &zone(&st.plan)) {
                Decision::Idle => {}
                Decision::Start(trigger) => {
                    eprintln!("scheduled backup");
                    running = start_backup(trigger, mode).map_err(|e| eprintln!("{e}")).ok();
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
        assert_eq!(due_ms_at(&st, start, start), start + 600_000);
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

    /// Also at every start of the PC (0.8.21, on top of the interval or the calendar): the
    /// delay after start, however recent the last backup of an earlier boot; once per boot (a
    /// restart of the agent alone runs nothing); the interval then counts from that backup, a
    /// calendar's next time still comes.
    #[test]
    fn also_at_every_start() {
        let mut st = State::default();
        st.plan.plan.start_delay_min = 5;
        st.plan.plan.interval_hours = 24;
        st.plan.plan.at_boot = true;
        st.plan.trigger = Some(TaskTrigger::Interval { hours: 24, after_boot_min: 5 });
        let boot = rome_ms("2026-11-17T08:00");
        let started = boot + 20_000;
        // backed up 10 minutes before this boot: due anyway, after the delay
        st.last_ok_ms = Some(boot - 600_000);
        assert_eq!(due_ms_at(&st, started, boot), started + 300_000);
        // without the switch: 24 h after that backup
        st.plan.plan.at_boot = false;
        assert_eq!(due_ms_at(&st, started, boot), boot - 600_000 + 24 * 3_600_000);
        st.plan.plan.at_boot = true;
        // the backup of this boot done (08:12): the agent restarted at 11:00 runs nothing new;
        // the interval counts from that backup
        st.last_ok_ms = Some(started + 400_000);
        assert_eq!(due_ms_at(&st, rome_ms("2026-11-17T11:00"), boot), started + 400_000 + 24 * 3_600_000);
        // with a calendar at 20:00: the boot's backup does not replace it
        st.plan.trigger = Some(TaskTrigger::Calendar { on_calendar: "*-*-* 20:00:00".into(), cron: None, persistent: true, randomized_delay_s: 0 });
        st.plan.tz = Some("Europe/Rome".into());
        assert_eq!(due_ms_at(&st, started, boot), rome_ms("2026-11-17T20:00"));
        // a failed boot backup is tried again (5, 10, 20… minutes)
        st.last_ok_ms = Some(boot - 600_000);
        st.failures = 1;
        st.last_attempt_ms = Some(started + 300_000);
        assert_eq!(due_ms_at(&st, started, boot), started + 300_000 + 600_000);
    }
}
