//! Manual restart and shut down (`/api/v1/system/power`): the parts that need no root.
//!
//! The action goes through systemd's `shutdown` (`shutdown -r|-P +N "message"`, `shutdown -c`):
//! systemd-logind keeps the schedule (`/run/systemd/shutdown/scheduled`), warns the logged-in
//! users with a wall message, and a schedule set over SSH shows in rynas too.

use std::path::Path;

use rynas_api_types::power::{SYSTEM_POWER_DELAYS_MIN, SystemPowerAction};

/// logind's schedule file, below the `/run` root.
pub const SCHEDULED: &str = "/run/systemd/shutdown/scheduled";

/// Processes that hold the package manager.
const APT_PROCESSES: [&str; 5] = ["apt", "apt-get", "aptitude", "dpkg", "unattended-upgr"];

/// A scheduled shutdown of logind (`USEC=`, `MODE=`): the action and its time in ms.
/// `halt`, `kexec` and unknown modes are none of rynas's actions.
pub fn parse_scheduled(text: &str) -> Option<(SystemPowerAction, u64)> {
    let (mut usec, mut mode) = (None, None);
    for line in text.lines() {
        match line.split_once('=') {
            Some(("USEC", v)) => usec = v.trim().parse::<u64>().ok(),
            Some(("MODE", v)) => mode = Some(v.trim()),
            _ => {}
        }
    }
    let action = match mode? {
        "reboot" => SystemPowerAction::Restart,
        "poweroff" => SystemPowerAction::Shutdown,
        _ => return None,
    };
    Some((action, usec? / 1000))
}

/// The arguments of `shutdown` for an action (`delay_min` checked by [`valid_delay`]).
pub fn shutdown_args(action: SystemPowerAction, delay_min: u32, message: &str) -> Vec<String> {
    let flag = match action {
        SystemPowerAction::Restart => "-r",
        SystemPowerAction::Shutdown => "-P",
    };
    let when = if delay_min == 0 {
        "now".to_string()
    } else {
        format!("+{delay_min}")
    };
    vec![flag.into(), when, message.into()]
}

pub fn valid_delay(delay_min: u32) -> bool {
    SYSTEM_POWER_DELAYS_MIN.contains(&delay_min)
}

/// The package manager processes running (`/proc/<pid>/comm` below `proc`), sorted, once each.
/// Debian's `unattended-upgrade-shutdown --wait-for-signal` (the always-running helper that
/// makes a shutdown wait for an upgrade) has the same 15-character name as an upgrade: it is
/// told apart by its command line and never counts.
pub fn apt_processes(proc: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(proc)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|e| {
            let comm = std::fs::read_to_string(e.path().join("comm")).ok()?.trim().to_string();
            if !APT_PROCESSES.contains(&comm.as_str()) {
                return None;
            }
            let helper = comm == "unattended-upgr"
                && std::fs::read(e.path().join("cmdline"))
                    .is_ok_and(|c| c.split(|b| *b == 0).any(|a| a.ends_with(b"unattended-upgrade-shutdown")));
            (!helper).then_some(comm)
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduled_file() {
        let t = "USEC=1762510000000000\nWARN_WALL=1\nMODE=reboot\nUID=0\nWALL_MESSAGE=x\n";
        assert_eq!(
            parse_scheduled(t),
            Some((SystemPowerAction::Restart, 1_762_510_000_000))
        );
        let t = "USEC=1762510000000000\nMODE=poweroff\n";
        assert_eq!(
            parse_scheduled(t).map(|p| p.0),
            Some(SystemPowerAction::Shutdown)
        );
        assert_eq!(parse_scheduled("USEC=1\nMODE=halt\n"), None);
        assert_eq!(parse_scheduled("MODE=reboot\n"), None);
        assert_eq!(parse_scheduled(""), None);
    }

    #[test]
    fn the_unattended_upgrades_shutdown_helper_is_no_upgrade() {
        let t = tempfile::tempdir().unwrap();
        let proc = |pid: &str, comm: &str, cmdline: &[&str]| {
            let d = t.path().join(pid);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("comm"), format!("{comm}\n")).unwrap();
            std::fs::write(d.join("cmdline"), cmdline.join("\0") + "\0").unwrap();
        };
        proc("835", "unattended-upgr", &["/usr/bin/python3", "/usr/share/unattended-upgrades/unattended-upgrade-shutdown", "--wait-for-signal"]);
        proc("900", "bash", &["bash"]);
        assert!(apt_processes(t.path()).is_empty());
        // a real run, and dpkg under it
        proc("1200", "unattended-upgr", &["/usr/bin/python3", "/usr/bin/unattended-upgrade"]);
        proc("1201", "dpkg", &["/usr/bin/dpkg", "--configure", "-a"]);
        assert_eq!(apt_processes(t.path()), ["dpkg", "unattended-upgr"]);
    }

    #[test]
    fn args() {
        assert_eq!(
            shutdown_args(SystemPowerAction::Restart, 0, "m"),
            ["-r", "now", "m"]
        );
        assert_eq!(
            shutdown_args(SystemPowerAction::Shutdown, 5, "m"),
            ["-P", "+5", "m"]
        );
        assert!(valid_delay(0) && valid_delay(1) && valid_delay(5));
        assert!(!valid_delay(2) && !valid_delay(60));
    }

    #[test]
    fn apt() {
        let d = tempfile::tempdir().unwrap();
        for (pid, comm) in [("1", "systemd"), ("20", "dpkg"), ("21", "apt-get"), ("22", "dpkg")] {
            std::fs::create_dir_all(d.path().join(pid)).unwrap();
            std::fs::write(d.path().join(pid).join("comm"), format!("{comm}\n")).unwrap();
        }
        std::fs::create_dir_all(d.path().join("self")).unwrap();
        assert_eq!(apt_processes(d.path()), ["apt-get", "dpkg"]);
        assert!(apt_processes(&d.path().join("missing")).is_empty());
    }
}
