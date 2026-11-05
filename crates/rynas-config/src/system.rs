//! What an apply does to the running system besides writing files, behind a trait so that
//! tests and `--dry-system` never touch the host.
//!
//! - [`HostSystem`]: systemd over D-Bus (`org.freedesktop.systemd1`, zbus), `hostnamectl`,
//!   validators run as real commands.
//! - [`MemorySystem`]: records every call, keeps the hostname in memory, can be told to fail.

use std::sync::Mutex;
use std::time::Duration;

use rynas_tasks::{Cmd, CmdOutput};

pub trait System: Send + Sync {
    /// Hostname the kernel reports now.
    fn hostname(&self) -> Result<String, String>;
    /// Sets the static and transient hostname.
    fn set_hostname(&self, name: &str) -> Result<(), String>;
    /// `systemctl reload-or-restart <unit>`.
    fn reload_or_restart_unit(&self, unit: &str) -> Result<(), String>;
    /// `systemctl is-active <unit>`.
    fn unit_active(&self, unit: &str) -> Result<bool, String>;
    /// Runs a validator or helper command (non-zero exit is an error).
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, String>;

    /// `systemctl restart <unit>` (when a reload is not enough, e.g. new listening
    /// interfaces).
    fn restart_unit(&self, unit: &str) -> Result<(), String> {
        self.run(&Cmd::new("systemctl").args(["restart", unit]))
            .map(|_| ())
    }

    /// `systemctl enable --now <unit>` or `systemctl disable --now <unit>`.
    fn set_unit_enabled(&self, unit: &str, enabled: bool) -> Result<(), String> {
        let verb = if enabled { "enable" } else { "disable" };
        self.run(&Cmd::new("systemctl").args([verb, "--now", unit]))
            .map(|_| ())
    }

    /// `systemctl daemon-reload` (after unit files changed).
    fn daemon_reload(&self) -> Result<(), String> {
        self.run(&Cmd::new("systemctl").arg("daemon-reload"))
            .map(|_| ())
    }

    /// `systemctl start --no-block <unit>`: queues the start job and returns.
    fn start_unit(&self, unit: &str) -> Result<(), String> {
        self.run(&Cmd::new("systemctl").args(["start", "--no-block", unit]))
            .map(|_| ())
    }

    /// `systemctl stop <unit>`.
    fn stop_unit(&self, unit: &str) -> Result<(), String> {
        self.run(&Cmd::new("systemctl").args(["stop", unit]))
            .map(|_| ())
    }
}

/// The real system. D-Bus calls run on the given tokio runtime: call the methods from a
/// blocking thread (`spawn_blocking`), never from an async task.
pub struct HostSystem {
    rt: tokio::runtime::Handle,
    bus: Mutex<Option<zbus::Connection>>,
}

const SYSTEMD: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER: &str = "org.freedesktop.systemd1.Manager";
const UNIT: &str = "org.freedesktop.systemd1.Unit";
const DBUS_TIMEOUT: Duration = Duration::from_secs(30);

impl HostSystem {
    pub fn new(rt: tokio::runtime::Handle) -> Self {
        Self {
            rt,
            bus: Mutex::new(None),
        }
    }

    fn bus(&self) -> Result<zbus::Connection, String> {
        let mut b = self.bus.lock().unwrap();
        if let Some(c) = b.as_ref() {
            return Ok(c.clone());
        }
        let c = self
            .rt
            .block_on(async { zbus::Connection::system().await })
            .map_err(|e| format!("system bus: {e}"))?;
        *b = Some(c.clone());
        Ok(c)
    }

    fn block_on<T>(
        &self,
        f: impl std::future::Future<Output = zbus::Result<T>>,
    ) -> Result<T, String> {
        self.rt.block_on(async {
            match tokio::time::timeout(DBUS_TIMEOUT, f).await {
                Ok(r) => r.map_err(|e| e.to_string()),
                Err(_) => Err("D-Bus call timed out".to_string()),
            }
        })
    }
}

impl System for HostSystem {
    fn hostname(&self) -> Result<String, String> {
        std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|s| s.trim().to_string())
            .map_err(|e| e.to_string())
    }

    fn set_hostname(&self, name: &str) -> Result<(), String> {
        Cmd::new("hostnamectl")
            .args(["set-hostname", "--static", "--transient", name])
            .timeout(Duration::from_secs(30))
            .run_blocking()
            .map(|_| ())
            .map_err(|e| format!("hostnamectl: {e}"))
    }

    fn reload_or_restart_unit(&self, unit: &str) -> Result<(), String> {
        let conn = self.bus()?;
        let unit = unit.to_string();
        self.block_on(async move {
            let p = zbus::Proxy::new(&conn, SYSTEMD, SYSTEMD_PATH, MANAGER).await?;
            let _job: zbus::OwnedObjectPath = p
                .call("ReloadOrRestartUnit", &(unit.as_str(), "replace"))
                .await?;
            Ok(())
        })
        .map_err(|e| format!("reload {e}"))
    }

    fn unit_active(&self, unit: &str) -> Result<bool, String> {
        let conn = self.bus()?;
        let unit = unit.to_string();
        self.block_on(async move {
            let p = zbus::Proxy::new(&conn, SYSTEMD, SYSTEMD_PATH, MANAGER).await?;
            let path: zbus::OwnedObjectPath = p.call("LoadUnit", &(unit.as_str(),)).await?;
            let u = zbus::Proxy::new(&conn, SYSTEMD, path, UNIT).await?;
            let state: String = u.get_property("ActiveState").await?;
            Ok(matches!(state.as_str(), "active" | "reloading"))
        })
    }

    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, String> {
        cmd.run_blocking()
            .map_err(|e| format!("{}: {e}", cmd.program().to_string_lossy()))
    }
}

/// In-memory system for tests and `--dry-system`: nothing on the host changes.
pub struct MemorySystem {
    state: Mutex<MemoryState>,
}

#[derive(Debug, Default, Clone)]
pub struct MemoryState {
    pub hostname: String,
    /// Every call, e.g. `set-hostname nas`, `reload smbd.service`, `run testparm ...`.
    pub calls: Vec<String>,
    /// Units whose reload fails.
    pub fail_reload: Vec<String>,
    /// Units reported inactive.
    pub inactive: Vec<String>,
    /// Programs that fail when run.
    pub fail_programs: Vec<String>,
    /// `set_hostname` succeeds without changing the hostname (health check fails).
    pub ignore_set_hostname: bool,
}

impl MemorySystem {
    pub fn new(hostname: &str) -> Self {
        Self {
            state: Mutex::new(MemoryState {
                hostname: hostname.into(),
                ..Default::default()
            }),
        }
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, MemoryState> {
        self.state.lock().unwrap()
    }

    fn record(&self, call: String) {
        tracing::info!("dry system: {call}");
        self.state().calls.push(call);
    }
}

impl System for MemorySystem {
    fn hostname(&self) -> Result<String, String> {
        Ok(self.state().hostname.clone())
    }

    fn set_hostname(&self, name: &str) -> Result<(), String> {
        self.record(format!("set-hostname {name}"));
        let mut s = self.state();
        if !s.ignore_set_hostname {
            s.hostname = name.into();
        }
        Ok(())
    }

    fn reload_or_restart_unit(&self, unit: &str) -> Result<(), String> {
        self.record(format!("reload {unit}"));
        if self.state().fail_reload.iter().any(|u| u == unit) {
            return Err(format!("reload {unit}: job failed"));
        }
        Ok(())
    }

    fn unit_active(&self, unit: &str) -> Result<bool, String> {
        Ok(!self.state().inactive.iter().any(|u| u == unit))
    }

    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, String> {
        self.record(format!("run {}", cmd.display()));
        let program = cmd.program().to_string_lossy().into_owned();
        if self.state().fail_programs.contains(&program) {
            return Err(format!("{program}: exit code 1: invalid configuration"));
        }
        Ok(CmdOutput {
            code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}
