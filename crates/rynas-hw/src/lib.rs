//! The hardware provider interface (`rynas-hw`, D-HW / D-MONITOR provider model).
//!
//! A provider knows one family of machines (Synology through synohwd today; UGREEN, plain
//! hwmon, IPMI later). The core only talks to it through two traits, so that a provider can
//! move from a built-in crate to a plugin process (JSON-RPC, D-PLUG) without changing the
//! callers:
//! - [`HardwareProvider`]: the read side (fans, bays, LED), what `/api/v1/hardware` shows;
//! - [`HardwareControl`]: actions (beep, mute, identify a bay) and model data (fan curves).
//!
//! Configuration (e.g. `synology.hw` → `/etc/synohwd.toml`) is not part of the traits: it goes
//! through the core configuration pipeline (pending → apply → revisions) like every domain.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rynas_api_types::synology::{BeepKind, FanCurves};
use rynas_api_types::{HardwareStatus, Temperature};

pub trait HardwareProvider: Send + Sync {
    /// Current status. `Ok(None)`: no provider data on this machine (not an error).
    fn status(&self) -> Result<Option<HardwareStatus>, String>;

    /// Temperatures next to the hardware controller's own shutdown thresholds (the controller
    /// powers the machine off by itself above them), from the provider's status and the
    /// sensor readings; empty when the hardware has no such controller.
    fn thermal_limits(
        &self,
        _status: &HardwareStatus,
        _temps: &[Temperature],
    ) -> Vec<ThermalLimit> {
        Vec::new()
    }
}

/// A temperature and the threshold at which the hardware controller shuts the machine down.
#[derive(Debug, Clone, PartialEq)]
pub struct ThermalLimit {
    /// Sensor id (`cpu`, `nvme0`, `bay2`).
    pub id: String,
    pub label: String,
    pub celsius: f32,
    pub shutdown_c: f32,
}

/// Machines without platform hardware support.
pub struct NoProvider;

impl HardwareProvider for NoProvider {
    fn status(&self) -> Result<Option<HardwareStatus>, String> {
        Ok(None)
    }
}

/// Why an action failed; the API maps each to one error code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlError {
    /// No such bay, LED... (404 `not_found`).
    NotFound(String),
    /// Refused input (400 `invalid_value`).
    Invalid(String),
    /// The executor (synohwd) is not running (503 `backend_unavailable`).
    Unavailable(String),
    /// Busy, try again (409 `busy`).
    Busy(String),
    /// Anything else (500 `internal`).
    Failed(String),
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControlError::NotFound(m)
            | ControlError::Invalid(m)
            | ControlError::Unavailable(m)
            | ControlError::Busy(m)
            | ControlError::Failed(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ControlError {}

/// Actions on the hardware. Every call is short (a FIFO write, a sysfs write): callers run
/// them on a blocking thread.
pub trait HardwareControl: Send + Sync {
    /// Test beep.
    fn beep(&self, kind: BeepKind) -> Result<(), ControlError>;

    /// Silences an alarm that is sounding; `Ok(false)` when nothing was sounding.
    fn mute(&self) -> Result<bool, ControlError>;

    /// Blinks the LED of bay `bay` (1-based) for `duration`, then restores it. Returns at
    /// once; calling it again while it blinks extends the blinking.
    fn identify(&self, bay: u32, duration: Duration) -> Result<(), ControlError>;

    /// Fan curves of the model with the current readings, when the provider has them.
    fn fan_curves(&self) -> Option<FanCurves> {
        None
    }
}

/// `root` + absolute `path` (tests give a fake root with a `sys/` tree).
pub fn under(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches('/'))
}

/// DMI `(vendor, product)` below `root`, junk values ("To Be Filled By O.E.M.") as empty.
pub fn dmi(root: &Path) -> (String, String) {
    let read = |f: &str| {
        let v = std::fs::read_to_string(under(root, &format!("/sys/class/dmi/id/{f}")))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if v.eq_ignore_ascii_case("To Be Filled By O.E.M.") {
            String::new()
        } else {
            v
        }
    };
    (read("sys_vendor"), read("product_name"))
}

/// "Vendor Product" from DMI, e.g. "Synology Inc. DS923+"; empty if unknown.
pub fn dmi_model(root: &Path) -> String {
    match dmi(root) {
        (v, p) if !v.is_empty() && !p.is_empty() => format!("{v} {p}"),
        (v, p) if v.is_empty() => p,
        (v, _) => v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dmi_from_a_fake_root() {
        let d = std::env::temp_dir().join(format!("rynas-hw-dmi-{}", std::process::id()));
        let id = d.join("sys/class/dmi/id");
        std::fs::create_dir_all(&id).unwrap();
        std::fs::write(id.join("sys_vendor"), "Synology Inc.\n").unwrap();
        std::fs::write(id.join("product_name"), "DS920+\n").unwrap();
        assert_eq!(dmi_model(&d), "Synology Inc. DS920+");
        std::fs::write(id.join("sys_vendor"), "To Be Filled By O.E.M.\n").unwrap();
        assert_eq!(dmi_model(&d), "DS920+");
        std::fs::remove_dir_all(&d).unwrap();
        assert_eq!(dmi_model(&d), "");
        assert_eq!(under(Path::new("/x"), "/sys/a"), PathBuf::from("/x/sys/a"));
    }
}
