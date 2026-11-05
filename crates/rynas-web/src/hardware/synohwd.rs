//! Provider for synohwd (synodeb), which writes `/run/synohwd/status.json` every `period`
//! seconds; the format and its parsing live in `rynas_hw_synology::status`. The file is
//! parsed again only when its mtime or size changes.

use std::path::PathBuf;

use rynas_api_types::HardwareStatus;
use rynas_hw_synology::Synology;

use super::HardwareProvider;

pub use rynas_hw_synology::status::led_word;

/// Parses the status file. `now` (Unix seconds) gives `age_s`.
pub fn parse_status(json: &str, now: u64, model: String) -> Result<HardwareStatus, String> {
    Ok(rynas_hw_synology::status::Raw::parse(json)?.hardware(now, model))
}

/// The status file reader (any machine: no file, no provider data).
pub struct Synohwd {
    inner: Synology,
}

impl Synohwd {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            inner: Synology::status_reader(path),
        }
    }

    /// DMI product, e.g. "DS920+" (picks the fan tables of the model).
    pub fn product(&self) -> &str {
        self.inner.product()
    }
}

impl HardwareProvider for Synohwd {
    fn status(&self) -> Result<Option<HardwareStatus>, String> {
        self.inner.status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::Fan;

    const FIXTURE: &str = include_str!("../../tests/fixtures/synohwd-status.json");

    #[test]
    fn parse_fixture() {
        let raw: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        let epoch = raw["epoch"].as_u64().unwrap();
        let h = parse_status(FIXTURE, epoch + 7, "Synology Inc. DS923+".into()).unwrap();
        assert_eq!(h.provider.as_deref(), Some("synology"));
        assert_eq!(h.age_s, Some(7));
        assert_eq!(h.fan_duty, Some(30));
        assert!(!h.fan_fail);
        assert_eq!(h.status_led.as_deref(), Some("green"));
        assert_eq!(h.fans.len(), 2);
        assert_eq!(
            h.fans[0],
            Fan {
                label: "Fan 1".into(),
                rpm: Some(1180),
                alarm: false
            }
        );
        assert_eq!(h.bays.len(), 4);
        assert_eq!(h.bays[0].device.as_deref(), Some("sda"));
        assert_eq!(h.bays[0].celsius, Some(34.0));
        assert!(h.bays[3].asleep);
        assert_eq!(h.bays[3].celsius, Some(29.0)); // last known value kept by synohwd
        assert!(!h.bays[2].present);
        assert_eq!(h.m2.len(), 2);
        assert_eq!(h.m2[0].number, Some(1));
        // controllers swapped at boot: slot 1 holds nvme1, its temperature is sensors.nvme.nvme1
        assert_eq!(h.m2[0].device.as_deref(), Some("nvme1n1"));
        assert_eq!(h.m2[0].celsius, Some(39.9));
        assert_eq!(h.m2[1].device.as_deref(), Some("nvme0n1"));
        assert_eq!(h.m2[1].celsius, Some(41.9));
        assert_eq!(h.events.last().unwrap().1, "fans ok");
    }

    #[test]
    fn leds() {
        assert_eq!(led_word(";"), Some("orange-blink"));
        assert_eq!(led_word("9"), Some("green-blink"));
        assert_eq!(led_word("x"), None);
    }

    #[test]
    fn missing_file_is_no_provider() {
        let p = Synohwd::new("/nonexistent/synohwd/status.json");
        assert_eq!(p.status(), Ok(None));
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(parse_status("{not json", 0, String::new()).is_err());
    }
}
