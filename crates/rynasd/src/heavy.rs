//! The one builder of heavy commands (tree walks, measures, checksums, copies that nobody
//! waits for): see `docs/phase1/tasks.md` "Long work".
//!
//! - **Priority**: `nice 19` and the idle I/O class ([`Cmd::heavy`]), so a measure never slows
//!   the shares, the web interface or a backup that someone is waiting for. What must run in
//!   a unit of its own (outside the sandbox, or with memory limits) uses `rynas_tasks::Outside`
//!   with `Nice=19` and `IOSchedulingClass=idle` instead (the backup and dedup workers do).
//! - **Cancellation**: run it with `TaskCtx::run` (async) or `TaskCtx::run_blocking` (on the
//!   task's blocking thread): both kill the command's whole process group when the task is
//!   cancelled. `Cmd::run_blocking` alone does not know the task.
//! - **No fixed timeout where progress is visible**: a command that prints as it goes, or
//!   whose progress the caller reads elsewhere, gets [`stalled_after`] (stopped only after
//!   [`STALL`] without any sign of life) instead of a guess of how long it may take. The
//!   fixed timeout stays as the last resort ([`HEAVY_TIMEOUT`]); a quiet command (`du -s`)
//!   keeps a timeout of its own.
//! - **Time budget**: a job that stops at its budget calls `TaskCtx::partial`, so the task
//!   ends as partial, never as a plain success.

use std::ffi::OsStr;
use std::time::Duration;

use rynas_tasks::{Activity, Cmd};

/// Last-resort timeout of a heavy command (the stall watchdog or the command's own timeout
/// is what normally stops it).
pub const HEAVY_TIMEOUT: Duration = Duration::from_secs(24 * 3600);
/// A heavy command with visible progress is stopped after this long without any.
pub const STALL: Duration = Duration::from_secs(10 * 60);

/// `program` as a heavy command: lowest CPU and I/O priority, clean environment, the
/// last-resort [`HEAVY_TIMEOUT`]. Add arguments, then a timeout of its own (quiet commands)
/// or [`stalled_after`] (commands whose progress shows).
pub fn heavy_cmd(program: impl AsRef<OsStr>) -> Cmd {
    Cmd::new(program).heavy().env_clear().timeout(HEAVY_TIMEOUT)
}

/// `cmd` stopped after [`STALL`] without output or a bump of `progress` (pass `None` when
/// its output is the only progress there is).
pub fn stalled_after(cmd: Cmd, progress: Option<Activity>) -> Cmd {
    match progress {
        Some(a) => cmd.stall_watch(STALL, a),
        None => cmd.stall_timeout(STALL),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heavy_commands_are_low_priority_with_a_long_last_resort() {
        let o = heavy_cmd("sh").args(["-c", "cut -d' ' -f19 /proc/self/stat"]).run_blocking().unwrap();
        assert_eq!(o.stdout.trim(), "19");
        let o = stalled_after(heavy_cmd("echo").arg("x"), None).run_blocking().unwrap();
        assert_eq!(o.stdout, "x\n");
    }
}
