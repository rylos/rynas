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

use rynas_api_types::pcagent::{AgentCommand, AgentPlan};

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

/// When the next scheduled backup is due (ms), `None` when paused.
pub fn next_due(st: &State, started_ms: u64) -> Option<u64> {
    let p = &st.plan.plan;
    if p.paused {
        return None;
    }
    let interval = u64::from(p.interval_hours.max(1)) * 3_600_000;
    let after_start = started_ms + u64::from(p.start_delay_min) * 60_000;
    let due = match st.last_ok_ms {
        Some(ok) => ok + interval,
        None => 0,
    };
    // a failed run: again after 5, 10, 20… minutes, never later than the interval
    let retry = match (st.failures, st.last_attempt_ms) {
        (n, Some(at)) if n > 0 => at + ((5 * 60_000u64) << n.min(6)).min(interval),
        _ => 0,
    };
    Some(due.max(after_start).max(retry))
}

struct Running {
    child: Child,
    inhibit: Option<Child>,
    since: Instant,
}

fn start_backup() -> Result<Running, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let child = Command::new(&exe)
        .args(["backup", "--service"])
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start the backup: {e}"))?;
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

/// The plan, waiting up to `wait` s for a change.
fn poll(st: &State, wait: u64) -> Result<AgentPlan, String> {
    let c = client_of(st)?;
    c.get_json(&format!("/api/v1/pcbackup/agent/plan?rev={}&wait={wait}", st.plan.rev))
}

pub fn run_service() -> Result<(), String> {
    install_signals();
    crate::backup::cleanup_leftovers();
    let started = now_ms();
    let mut st = State::load()?;
    eprintln!("rynas-agent {}: PC {} on {}", env!("CARGO_PKG_VERSION"), st.device, st.url);
    let mut running: Option<Running> = None;
    let mut last_poll_error: Option<Instant> = None;
    while !STOP.load(Ordering::SeqCst) {
        // a short poll while a backup runs (to see Cancel), a long one otherwise
        let wait = if running.is_some() { 5 } else { 50 };
        match poll(&st, wait) {
            Ok(plan) => {
                if plan.rev != st.plan.rev {
                    st.plan = plan.clone();
                    let _ = st.save();
                }
                match plan.command {
                    Some(AgentCommand::BackupNow { id }) if id > st.handled_command => {
                        st.handled_command = id;
                        let _ = st.save();
                        if running.is_none() {
                            eprintln!("back up now (from the NAS)");
                            running = start_backup().map_err(|e| eprintln!("{e}")).ok();
                        }
                    }
                    Some(AgentCommand::Cancel { id }) if id > st.handled_command => {
                        st.handled_command = id;
                        let _ = st.save();
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
        if running.is_none()
            && let Some(due) = next_due(&st, started)
            && now_ms() >= due
        {
            eprintln!("scheduled backup");
            running = start_backup().map_err(|e| eprintln!("{e}")).ok();
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

    #[test]
    fn due_times() {
        let mut st = State::default();
        st.plan.plan.interval_hours = 24;
        st.plan.plan.start_delay_min = 10;
        // never backed up: after the start delay
        assert_eq!(next_due(&st, 1_000_000), Some(1_000_000 + 600_000));
        st.last_ok_ms = Some(10_000_000);
        assert_eq!(next_due(&st, 0), Some(10_000_000 + 86_400_000));
        st.failures = 1;
        st.last_attempt_ms = Some(200_000_000);
        assert_eq!(next_due(&st, 0), Some(200_000_000 + 600_000));
        st.plan.plan.paused = true;
        assert_eq!(next_due(&st, 0), None);
    }
}
