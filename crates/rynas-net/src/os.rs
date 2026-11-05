//! The system as the network engine sees it: commands (argv only), configuration files below
//! a root, and runtime files (`/sys`, `/run`).
//!
//! - [`HostOs`]: the real machine; with `dry` the commands that change the system are only
//!   logged and configuration files go below a dry root (`rynasd --dry-system`).
//! - [`crate::fake::FakeOs`]: a scripted kernel for the tests.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rynas_tasks::{Cmd, CmdOutput};

pub trait NetOs: Send + Sync {
    /// Runs `argv` (no shell). A non-zero exit is not an error here: callers look at it.
    fn run(&self, argv: &[&str], timeout: Duration) -> Result<CmdOutput, String>;
    /// Configuration files are read and written below this directory (`/` in production).
    fn root(&self) -> &Path;
    /// A runtime file (`/sys/...`, `/run/...`), trimmed.
    fn read_runtime(&self, abs: &str) -> Option<String>;
    /// Whether a runtime path exists.
    fn runtime_exists(&self, abs: &str) -> bool;
    /// Last component of the target of a runtime symlink (`/sys/class/net/x/device/driver`).
    fn runtime_link_name(&self, abs: &str) -> Option<String>;
    /// Names in a runtime directory.
    fn runtime_list(&self, abs: &str) -> Vec<String>;
    /// Commands that change the system are only logged.
    fn dry(&self) -> bool {
        false
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d)
    }
    fn now_ms(&self) -> u64 {
        crate::now_ms()
    }

    /// Real location of a configuration path.
    fn path(&self, abs: &Path) -> PathBuf {
        rynas_config::files::under_root(self.root(), abs)
    }

    /// A configuration file, `None` when missing.
    fn read_config(&self, abs: &Path) -> Option<String> {
        std::fs::read_to_string(self.path(abs)).ok()
    }

    /// Runs and fails on a non-zero exit, with the last line of stderr.
    fn run_ok(&self, argv: &[&str], timeout: Duration) -> Result<CmdOutput, String> {
        let o = self.run(argv, timeout)?;
        if o.success() {
            Ok(o)
        } else {
            let last = o
                .stderr
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .to_string();
            Err(format!(
                "{}: exit code {}{}{last}",
                argv.join(" "),
                o.code.map_or("?".into(), |c| c.to_string()),
                if last.is_empty() { "" } else { ": " }
            ))
        }
    }
}

/// Commands that change the running system (only logged by a dry [`HostOs`]).
pub fn mutating(argv: &[&str]) -> bool {
    match argv.first().copied().unwrap_or("") {
        "networkctl" => matches!(
            argv.get(1).copied(),
            Some("reload" | "reconfigure" | "up" | "down" | "renew" | "forcerenew")
        ),
        "systemctl" => !matches!(
            argv.get(1).copied(),
            Some("is-active" | "is-enabled" | "show" | "status" | "list-units" | "cat")
        ),
        "ip" => argv.iter().any(|a| {
            matches!(
                *a,
                "add" | "del" | "delete" | "flush" | "set" | "replace" | "change"
            )
        }),
        "systemd-run" | "ifup" | "ifdown" | "arping" => true,
        _ => false,
    }
}

/// The real machine.
pub struct HostOs {
    root: PathBuf,
    dry: bool,
}

impl HostOs {
    pub fn new(root: impl Into<PathBuf>, dry: bool) -> Self {
        Self {
            root: root.into(),
            dry,
        }
    }
}

impl NetOs for HostOs {
    fn run(&self, argv: &[&str], timeout: Duration) -> Result<CmdOutput, String> {
        if self.dry && mutating(argv) {
            tracing::info!("dry system: not running {}", argv.join(" "));
            return Ok(CmdOutput {
                code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            });
        }
        let Some((prog, args)) = argv.split_first() else {
            return Err("empty command".into());
        };
        Cmd::new(prog)
            .args(args)
            .env_clear()
            .timeout(timeout)
            .output_blocking()
            .map_err(|e| format!("{}: {e}", argv.join(" ")))
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn read_runtime(&self, abs: &str) -> Option<String> {
        std::fs::read_to_string(abs)
            .ok()
            .map(|s| s.trim().to_string())
    }

    fn runtime_exists(&self, abs: &str) -> bool {
        Path::new(abs).exists()
    }

    fn runtime_link_name(&self, abs: &str) -> Option<String> {
        std::fs::read_link(abs)
            .ok()?
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
    }

    fn runtime_list(&self, abs: &str) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(abs)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    fn dry(&self) -> bool {
        self.dry
    }
}

/// Short timeout for read-only queries.
pub const QUICK: Duration = Duration::from_secs(10);
/// networkctl, systemctl: may wait for a unit job.
pub const SLOW: Duration = Duration::from_secs(60);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_commands_change_the_system() {
        assert!(mutating(&["networkctl", "reload"]));
        assert!(!mutating(&["networkctl", "status"]));
        assert!(mutating(&["systemctl", "restart", "networking"]));
        assert!(!mutating(&["systemctl", "is-active", "systemd-networkd"]));
        assert!(mutating(&[
            "ip", "link", "set", "dev", "eth0", "mtu", "1500"
        ]));
        assert!(mutating(&["ip", "addr", "flush", "dev", "eth0"]));
        assert!(!mutating(&["ip", "-j", "-d", "addr", "show"]));
        assert!(mutating(&["systemd-run", "--on-active=120s", "true"]));
        assert!(!mutating(&["ping", "-c", "1", "192.0.2.1"]));
    }
}
