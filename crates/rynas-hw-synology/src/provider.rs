//! The provider: [`HardwareProvider`] (status) and [`HardwareControl`] (beep, mute, identify,
//! fan curves) over synohwd and the LED class devices, below a configurable root (`/` in
//! production, a fake `sys/` + `run/` tree in tests).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rynas_api_types::HardwareStatus;
use rynas_api_types::synology::{
    BeepKind, FanCurves, FanMode, SensorClass, SensorReading, SynologyStatus,
};
use rynas_hw::{ControlError, HardwareControl, HardwareProvider};

use crate::status::{Raw, STATUS};
use crate::{cmd, leds, model};

/// Longest identify (the API default is 30 s).
pub const MAX_IDENTIFY: Duration = Duration::from_secs(300);

struct Cached {
    stamp: (SystemTime, u64),
    raw: Raw,
}

pub struct Synology {
    root: PathBuf,
    /// DMI product, e.g. "DS920+".
    product: String,
    /// "Synology Inc. DS920+".
    model: String,
    status_path: PathBuf,
    commander: cmd::Commander,
    identifier: leds::Identifier,
    cache: Mutex<Option<Cached>>,
    /// The fan mode the curves call active (set from the applied settings).
    active_mode: Mutex<FanMode>,
}

pub fn now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Synology {
    /// The provider of the machine below `root`, `None` when it is not a Synology.
    pub fn detect(root: impl Into<PathBuf>) -> Option<Self> {
        let root = root.into();
        let product = model::detect(&root)?;
        Some(Self::new(root, product))
    }

    pub fn new(root: impl Into<PathBuf>, product: impl Into<String>) -> Self {
        let root = root.into();
        Self {
            model: rynas_hw::dmi_model(&root),
            product: product.into(),
            status_path: rynas_hw::under(&root, STATUS),
            commander: cmd::Commander::under(&root),
            identifier: leds::Identifier::new(&root),
            root,
            cache: Mutex::new(None),
            active_mode: Mutex::new(FanMode::Low),
        }
    }

    /// Only the status file (rynas-web's `/api/v1/hardware`, any machine).
    pub fn status_reader(path: impl Into<PathBuf>) -> Self {
        let mut s = Self::new(PathBuf::from("/"), rynas_hw::dmi(Path::new("/")).1);
        s.status_path = path.into();
        s
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn product(&self) -> &str {
        &self.product
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn bays(&self) -> u32 {
        model::bay_count(&self.product)
    }

    /// Microcontroller commands (control socket, FIFO fallback).
    pub fn commander(&self) -> &cmd::Commander {
        &self.commander
    }

    pub fn identifier(&self) -> &leds::Identifier {
        &self.identifier
    }

    pub fn set_active_mode(&self, m: FanMode) {
        *self.active_mode.lock().unwrap() = m;
    }

    /// synohwd's status (parsed again only when the file changes); `Ok(None)`: synohwd is not
    /// running (the file exists only while it runs).
    pub fn raw(&self) -> Result<Option<Raw>, String> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let meta = match std::fs::metadata(&self.status_path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                *cache = None;
                return Ok(None);
            }
            Err(e) => return Err(format!("{}: {e}", self.status_path.display())),
        };
        let stamp = (meta.modified().unwrap_or(UNIX_EPOCH), meta.len());
        if let Some(c) = cache.as_ref().filter(|c| c.stamp == stamp) {
            return Ok(Some(c.raw.clone()));
        }
        let text = match std::fs::read_to_string(&self.status_path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", self.status_path.display())),
        };
        let raw = Raw::parse(&text)?;
        *cache = Some(Cached {
            stamp,
            raw: raw.clone(),
        });
        Ok(Some(raw))
    }

    /// The full status; a stopped synohwd gives `running: false` and empty lists.
    pub fn synology_status(&self) -> Result<SynologyStatus, String> {
        Ok(match self.raw()? {
            Some(r) => r.synology(now_s(), self.product.clone()),
            None => SynologyStatus {
                model: self.product.clone(),
                stale: true,
                ..Default::default()
            },
        })
    }
}

impl HardwareProvider for Synology {
    fn status(&self) -> Result<Option<HardwareStatus>, String> {
        Ok(self.raw()?.map(|r| r.hardware(now_s(), self.model.clone())))
    }
}

/// Readings as the fan controller groups them.
pub fn readings(s: &SynologyStatus) -> Vec<SensorReading> {
    let mut v = vec![SensorReading {
        class: SensorClass::Cpu,
        sensor: "cpu".into(),
        temperature_c: s.cpu_temperature_c,
    }];
    for b in s.bays.iter().filter(|b| b.device.is_some()) {
        v.push(SensorReading {
            class: SensorClass::Disk,
            sensor: format!("bay{}", b.number),
            temperature_c: b.temperature_c,
        });
    }
    for (class, list) in [(SensorClass::Nvme, &s.nvme), (SensorClass::Nic, &s.nic)] {
        for t in list {
            v.push(SensorReading {
                class,
                sensor: t.name.clone(),
                temperature_c: t.temperature_c,
            });
        }
    }
    v
}

impl HardwareControl for Synology {
    fn beep(&self, kind: BeepKind) -> Result<(), ControlError> {
        let c = match kind {
            BeepKind::Short => cmd::BEEP_SHORT,
            BeepKind::Long => cmd::BEEP_LONG,
        };
        self.commander.send(c)
    }

    /// synohwd's beeps are single (`2`, `3`): no sequence it accepts stops a sound, and none
    /// is ever sounding for long. Nothing to send.
    fn mute(&self) -> Result<bool, ControlError> {
        Ok(false)
    }

    fn identify(&self, bay: u32, duration: Duration) -> Result<(), ControlError> {
        if bay == 0 || bay > self.bays() {
            return Err(ControlError::NotFound(format!(
                "bay {bay}: the {} has bays 1 to {}",
                self.product,
                self.bays()
            )));
        }
        if duration.is_zero() || duration > MAX_IDENTIFY {
            return Err(ControlError::Invalid(format!(
                "the duration is 1 to {} s",
                MAX_IDENTIFY.as_secs()
            )));
        }
        self.identifier.identify(bay, duration)
    }

    fn fan_curves(&self) -> Option<FanCurves> {
        let status = self.synology_status().ok();
        Some(FanCurves {
            model: self.product.clone(),
            active_mode: *self.active_mode.lock().unwrap(),
            modes: vec![
                model::mode_curves(&self.product, FanMode::Low),
                model::mode_curves(&self.product, FanMode::High),
            ],
            readings: status.as_ref().map(readings).unwrap_or_default(),
            duty_pct: status.and_then(|s| s.fan_duty_pct),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_root() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::create_dir_all(r.join("sys/devices/platform/synology-nas")).unwrap();
        std::fs::create_dir_all(r.join("sys/class/dmi/id")).unwrap();
        std::fs::write(r.join("sys/class/dmi/id/sys_vendor"), "Synology Inc.\n").unwrap();
        std::fs::write(r.join("sys/class/dmi/id/product_name"), "DS920+\n").unwrap();
        std::fs::create_dir_all(r.join("run/synohwd")).unwrap();
        d
    }

    #[test]
    fn detect_status_and_curves() {
        let d = fake_root();
        let p = Synology::detect(d.path()).unwrap();
        assert_eq!(p.product(), "DS920+");
        assert_eq!(p.model(), "Synology Inc. DS920+");
        assert_eq!(p.status().unwrap(), None, "synohwd not running");
        assert!(!p.synology_status().unwrap().running);
        std::fs::write(
            d.path().join("run/synohwd/status.json"),
            include_str!("../tests/fixtures/synohwd-status.json"),
        )
        .unwrap();
        let s = p.synology_status().unwrap();
        assert_eq!(s.bays.len(), 4);
        let c = p.fan_curves().unwrap();
        assert_eq!(c.model, "DS920+");
        assert_eq!(c.modes.len(), 2);
        assert_eq!(c.modes[0].curves[2].shutdown_c, 90);
        assert_eq!(c.duty_pct, Some(30));
        // cpu, three bays with a disk, two NVMe
        assert_eq!(c.readings.len(), 6);
        p.set_active_mode(FanMode::High);
        assert_eq!(p.fan_curves().unwrap().active_mode, FanMode::High);
    }

    #[test]
    fn actions_are_validated() {
        let d = fake_root();
        let p = Synology::detect(d.path()).unwrap();
        assert!(matches!(
            p.beep(BeepKind::Short),
            Err(ControlError::Unavailable(_))
        ));
        assert!(matches!(
            p.identify(5, Duration::from_secs(30)),
            Err(ControlError::NotFound(_))
        ));
        assert!(matches!(
            p.identify(1, Duration::from_secs(301)),
            Err(ControlError::Invalid(_))
        ));
        assert_eq!(p.mute(), Ok(false));
    }
}
