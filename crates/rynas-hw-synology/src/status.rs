//! synohwd's status file (`/run/synohwd/status.json`, `Daemon.write_status` in synohwd.py),
//! written every `period` seconds and removed when the daemon stops:
//!
//! ```json
//! {"time": "YYYY-MM-DD HH:MM:SS", "epoch": 1762250000, "period": 20,
//!  "sensors": {"cpu": 52.1, "disk": {"bay1": 34.0}, "nvme": {"nvme0": 41.9},
//!              "nic": {}, "asleep": ["bay4"]},
//!  "bays": {"1": {"present": true, "power": true, "device": "sda"}, …},
//!  "m2": {"1": {"present": true, "controller": "nvme1", "device": "nvme1n1",
//!               "pci": "0000:05:00.0", "port": "0000:00:14.1"}, …},
//!  "fan_duty": 30, "fans": [{"fan": 1, "rpm": 1234, "alarm": 0}, …], "fan_fail": false,
//!  "up_current": 870, "status_led": "8", "events": [["YYYY-MM-DD HH:MM:SS", "text"], …]}
//! ```
//!
//! `m2` (the M.2 slot of each NVMe drive; `controller` is the key into `sensors.nvme`, an
//! empty slot has only `port`) is newer than the rest: without it, or with `{}` (a model
//! without a slot map), the slots come from the `sensors.nvme` keys, with no slot number.
//!
//! Two views: [`Raw::hardware`] (the provider-neutral `HardwareStatus` of `/api/v1/hardware`)
//! and [`Raw::synology`] (everything, with API units).

use std::collections::{BTreeMap, HashMap};

use rynas_api_types::synology::{
    FanAlarm, HardwareEvent, LedMode, NamedTemperature, StatusLed, SynologyBay, SynologyDiagnostics,
    SynologyFan, SynologyStatus,
};
use rynas_api_types::{Bay, Fan, HardwareStatus, M2Slot};
use serde::Deserialize;

pub const STATUS: &str = "/run/synohwd/status.json";

#[derive(Deserialize, Debug, Clone, Default)]
pub struct Raw {
    #[serde(default)]
    pub time: Option<String>,
    pub epoch: Option<u64>,
    #[serde(default)]
    pub period: Option<u32>,
    #[serde(default)]
    pub sensors: Sensors,
    #[serde(default)]
    pub bays: BTreeMap<String, RawBay>,
    /// None: a synohwd older than the slot map.
    #[serde(default)]
    pub m2: Option<BTreeMap<String, RawM2>>,
    pub fan_duty: Option<u8>,
    #[serde(default)]
    pub fans: Vec<RawFan>,
    #[serde(default)]
    pub fan_fail: bool,
    #[serde(default)]
    pub up_current: Option<u64>,
    pub status_led: Option<String>,
    #[serde(default)]
    pub events: Vec<(String, String)>,
    /// Front LED level chip (synohwd ≥ synodeb 4.8); absent on older ones.
    #[serde(default)]
    pub led_schedule_supported: bool,
    #[serde(default)]
    pub led_level_min: Option<i64>,
    #[serde(default)]
    pub led_level_max: Option<i64>,
    #[serde(default)]
    pub led_level_default: Option<i64>,
    #[serde(default)]
    pub led_level_now: Option<i64>,
    /// "normal" | "reduced" | "off".
    #[serde(default)]
    pub led_mode_now: Option<String>,
}

/// A level of synohwd's status as the API's `u8` (anything else: unknown).
fn level(v: Option<i64>) -> Option<u8> {
    v.and_then(|v| u8::try_from(v).ok())
}

/// `led_mode_now` as the API's [`LedMode`] (unknown words: none).
pub fn led_mode(s: Option<&str>) -> Option<LedMode> {
    Some(match s? {
        "normal" => LedMode::Normal,
        "reduced" => LedMode::Reduced,
        "off" => LedMode::Off,
        _ => return None,
    })
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct Sensors {
    #[serde(default)]
    pub cpu: Option<f32>,
    #[serde(default)]
    pub disk: HashMap<String, Option<f32>>,
    #[serde(default)]
    pub nvme: BTreeMap<String, Option<f32>>,
    #[serde(default)]
    pub nic: BTreeMap<String, Option<f32>>,
    #[serde(default)]
    pub asleep: Vec<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RawBay {
    #[serde(default)]
    pub present: bool,
    #[serde(default)]
    pub power: bool,
    pub device: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RawM2 {
    #[serde(default)]
    pub present: bool,
    /// The key into `sensors.nvme`, e.g. "nvme1" (controller numbers change between boots).
    #[serde(default)]
    pub controller: Option<String>,
    /// First namespace, e.g. "nvme1n1".
    #[serde(default)]
    pub device: Option<String>,
    /// PCI address of the drive.
    #[serde(default)]
    pub pci: Option<String>,
    /// PCI address of the slot's root port (set even for an empty slot).
    #[serde(default)]
    pub port: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RawFan {
    pub fan: u32,
    pub rpm: Option<u32>,
    /// 1 = stopped, 0 = ok, null = not measured (yet)
    pub alarm: Option<i64>,
}

/// The controller of an NVMe namespace: "nvme0n1" → "nvme0" (anything else unchanged).
pub fn nvme_controller(dev: &str) -> &str {
    dev.strip_prefix("nvme")
        .and_then(|rest| {
            let i = rest.find('n')?;
            (i > 0 && rest[..i].bytes().all(|b| b.is_ascii_digit())).then(|| &dev[..4 + i])
        })
        .unwrap_or(dev)
}

/// synohwd's status LED code (uP command) as a word.
pub fn led_word(code: &str) -> Option<&'static str> {
    Some(match code {
        "7" => "off",
        "8" => "green",
        "9" => "green-blink",
        ":" => "orange",
        ";" => "orange-blink",
        _ => return None,
    })
}

pub fn led(code: &str) -> Option<StatusLed> {
    Some(match code {
        "7" => StatusLed::Off,
        "8" => StatusLed::Green,
        "9" => StatusLed::GreenBlink,
        ":" => StatusLed::Orange,
        ";" => StatusLed::OrangeBlink,
        _ => return None,
    })
}

/// synohwd's own staleness rule (`print_status`): older than 3 periods + 10 s.
pub fn is_stale(age_s: u64, period_s: u32) -> bool {
    age_s > 3 * period_s as u64 + 10
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// "YYYY-MM-DD HH:MM:SS" as seconds of a civil (time zone-less) timeline.
pub fn civil_seconds(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, time) = s.split_once([' ', 'T'])?;
    let mut d = date.splitn(3, '-').map(|x| x.parse::<i64>().ok());
    let (y, mo, da) = (d.next()??, d.next()??, d.next()??);
    let mut t = time.splitn(3, ':').map(|x| x.parse::<i64>().ok());
    let (h, mi, se) = (t.next()??, t.next()??, t.next()??);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&da) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    Some(days_from_civil(y, mo, da) * 86_400 + h * 3600 + mi * 60 + se)
}

/// The instant of an event: synohwd writes local times only, so the event is placed relative
/// to the status file's `time`/`epoch` pair (exact unless a DST change lies in between).
fn event_ms(time: Option<&str>, epoch: Option<u64>, at: &str) -> Option<u64> {
    let anchor = civil_seconds(time?)?;
    let ev = civil_seconds(at)?;
    let epoch = epoch? as i64;
    u64::try_from((epoch - (anchor - ev)) * 1000).ok()
}

impl Raw {
    pub fn parse(json: &str) -> Result<Raw, String> {
        serde_json::from_str(json).map_err(|e| format!("synohwd status: {e}"))
    }

    pub fn age_s(&self, now_s: u64) -> Option<u64> {
        self.epoch.map(|e| now_s.saturating_sub(e))
    }

    fn bay_temp(&self, n: u32) -> (Option<f32>, bool) {
        let key = format!("bay{n}");
        (
            self.sensors.disk.get(&key).copied().flatten(),
            self.sensors.asleep.contains(&key),
        )
    }

    fn bay_numbers(&self) -> Vec<(u32, &RawBay)> {
        let mut v: Vec<(u32, &RawBay)> = self
            .bays
            .iter()
            .filter_map(|(n, b)| Some((n.parse().ok()?, b)))
            .collect();
        v.sort_by_key(|(n, _)| *n);
        v
    }

    /// The M.2 slots: synohwd's map when it writes one, else (no map, or `{}` from a model
    /// without one) one unnumbered slot per NVMe sensor (namespace 1 of that controller),
    /// sorted by name. Controller numbers change between boots: only the map ties a drive to
    /// a physical slot.
    pub fn m2_slots(&self) -> Vec<M2Slot> {
        let temp = |s: &RawM2| {
            let ctrl = s
                .controller
                .as_deref()
                .or_else(|| s.device.as_deref().map(nvme_controller))?;
            self.sensors.nvme.get(ctrl).copied().flatten()
        };
        match self.m2.as_ref().filter(|m| !m.is_empty()) {
            Some(map) => {
                let mut v: Vec<M2Slot> = map
                    .iter()
                    .map(|(n, s)| M2Slot {
                        number: n.parse().ok(),
                        present: s.present,
                        device: s.device.clone().filter(|_| s.present),
                        celsius: if s.present { temp(s) } else { None },
                    })
                    .collect();
                v.sort_by_key(|s| (s.number.is_none(), s.number));
                v
            }
            None => self
                .sensors
                .nvme
                .iter()
                .map(|(ctrl, c)| M2Slot {
                    number: None,
                    present: true,
                    device: Some(format!("{ctrl}n1")),
                    celsius: *c,
                })
                .collect(),
        }
    }

    /// The provider-neutral summary (`/api/v1/hardware`).
    pub fn hardware(&self, now_s: u64, model: String) -> HardwareStatus {
        HardwareStatus {
            provider: Some("synology".into()),
            model,
            fans: self
                .fans
                .iter()
                .map(|f| Fan {
                    label: format!("Fan {}", f.fan),
                    rpm: f.rpm,
                    alarm: f.alarm == Some(1),
                })
                .collect(),
            bays: self
                .bay_numbers()
                .into_iter()
                .map(|(number, b)| {
                    let (celsius, asleep) = self.bay_temp(number);
                    Bay {
                        number,
                        present: b.present,
                        powered: b.power,
                        device: b.device.clone(),
                        celsius,
                        asleep,
                    }
                })
                .collect(),
            m2: self.m2_slots(),
            fan_duty: self.fan_duty,
            fan_fail: self.fan_fail,
            status_led: self
                .status_led
                .as_deref()
                .and_then(led_word)
                .map(str::to_string),
            events: self.events.clone(),
            age_s: self.age_s(now_s),
            board_temps: Vec::new(),
        }
    }

    /// Everything synohwd reports.
    pub fn synology(&self, now_s: u64, model: String) -> SynologyStatus {
        let age = self.age_s(now_s);
        let period = self.period.unwrap_or(20);
        let stale = age.is_none_or(|a| is_stale(a, period));
        let named = |m: &BTreeMap<String, Option<f32>>| {
            m.iter()
                .map(|(k, v)| NamedTemperature {
                    name: k.clone(),
                    temperature_c: *v,
                })
                .collect()
        };
        SynologyStatus {
            model,
            running: !stale,
            stale,
            read_at_ms: self.epoch.map(|e| e * 1000),
            age_s: age,
            period_s: self.period,
            fan_duty_pct: self.fan_duty,
            fan_fail: self.fan_fail,
            fans: self
                .fans
                .iter()
                .map(|f| SynologyFan {
                    number: f.fan,
                    rpm: f.rpm,
                    alarm: match f.alarm {
                        Some(0) => FanAlarm::Ok,
                        Some(1) => FanAlarm::Stopped,
                        _ => FanAlarm::Unknown,
                    },
                })
                .collect(),
            status_led: self.status_led.as_deref().and_then(led),
            bays: self
                .bay_numbers()
                .into_iter()
                .map(|(number, b)| {
                    let (temperature_c, asleep) = self.bay_temp(number);
                    SynologyBay {
                        number,
                        present: b.present,
                        powered: b.power,
                        device: b.device.clone(),
                        temperature_c,
                        asleep,
                    }
                })
                .collect(),
            cpu_temperature_c: self.sensors.cpu,
            nvme: named(&self.sensors.nvme),
            nic: named(&self.sensors.nic),
            events: self
                .events
                .iter()
                .map(|(at, text)| HardwareEvent {
                    at_ms: event_ms(self.time.as_deref(), self.epoch, at),
                    text: text.clone(),
                })
                .collect(),
            diagnostics: SynologyDiagnostics {
                up_current_raw: self.up_current,
            },
            boot: None,
            // the scale only means something where the chip is driven
            led_schedule_supported: self.led_schedule_supported,
            led_level_min: level(self.led_level_min).filter(|_| self.led_schedule_supported),
            led_level_max: level(self.led_level_max).filter(|_| self.led_schedule_supported),
            led_level_default: level(self.led_level_default).filter(|_| self.led_schedule_supported),
            led_level_now: level(self.led_level_now).filter(|_| self.led_schedule_supported),
            led_mode_now: led_mode(self.led_mode_now.as_deref()).filter(|_| self.led_schedule_supported),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/synohwd-status.json");

    #[test]
    fn summary_view() {
        let raw = Raw::parse(FIXTURE).unwrap();
        let epoch = raw.epoch.unwrap();
        let h = raw.hardware(epoch + 7, "Synology Inc. DS923+".into());
        assert_eq!(h.provider.as_deref(), Some("synology"));
        assert_eq!(h.age_s, Some(7));
        assert_eq!(h.fan_duty, Some(30));
        assert_eq!(h.status_led.as_deref(), Some("green"));
        assert_eq!(h.bays.len(), 4);
        assert!(h.bays[3].asleep);
        assert_eq!(h.bays[3].celsius, Some(29.0));
    }

    #[test]
    fn m2_slots() {
        // no slot map (this fixture): one unnumbered slot per NVMe sensor
        let h = Raw::parse(FIXTURE).unwrap().hardware(0, String::new());
        assert_eq!(h.m2.len(), 2);
        assert_eq!(h.m2[0].number, None);
        assert_eq!(h.m2[0].device.as_deref(), Some("nvme0n1"));
        assert_eq!(h.m2[1].celsius, Some(39.9));
        // the slot map wins; temperatures come through "controller", not the slot number
        let raw = Raw::parse(
            r#"{"sensors": {"nvme": {"nvme0": 41.0, "nvme1": 38.5}},
               "m2": {"2": {"present": true, "controller": "nvme0", "device": "nvme0n1",
                            "pci": "0000:04:00.0", "port": "0000:00:14.0"},
                      "1": {"present": true, "controller": "nvme1", "device": "nvme1n1",
                            "pci": "0000:05:00.0", "port": "0000:00:14.1"},
                      "3": {"present": false, "controller": null, "device": null, "pci": null,
                            "port": "0000:00:1c.0"}}}"#,
        )
        .unwrap();
        let m = raw.hardware(0, String::new()).m2;
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].number, Some(1));
        assert_eq!(m[0].device.as_deref(), Some("nvme1n1"));
        assert_eq!(m[0].celsius, Some(38.5));
        assert_eq!((m[1].number, m[1].celsius), (Some(2), Some(41.0)));
        assert_eq!((m[2].number, m[2].present, m[2].device.as_deref(), m[2].celsius), (Some(3), false, None, None));
        // "m2": {} (a model without a slot map): the unnumbered fallback
        let raw = Raw::parse(r#"{"sensors": {"nvme": {"nvme0": 41.0}}, "m2": {}}"#).unwrap();
        let m = raw.m2_slots();
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].number, m[0].device.as_deref(), m[0].celsius), (None, Some("nvme0n1"), Some(41.0)));
    }

    #[test]
    fn controllers() {
        assert_eq!(nvme_controller("nvme0n1"), "nvme0");
        assert_eq!(nvme_controller("nvme12n3"), "nvme12");
        assert_eq!(nvme_controller("nvme0"), "nvme0");
        assert_eq!(nvme_controller("sda"), "sda");
    }

    #[test]
    fn full_view() {
        let raw = Raw::parse(FIXTURE).unwrap();
        let epoch = raw.epoch.unwrap();
        let s = raw.synology(epoch + 7, "DS923+".into());
        assert!(s.running && !s.stale);
        assert_eq!(s.read_at_ms, Some(epoch * 1000));
        assert_eq!(s.period_s, Some(20));
        assert_eq!(s.cpu_temperature_c, Some(52.1));
        assert_eq!(s.nvme.len(), 2);
        assert_eq!(s.nvme[0].name, "nvme0");
        assert_eq!(s.status_led, Some(StatusLed::Green));
        assert_eq!(s.fans[0].alarm, FanAlarm::Ok);
        assert_eq!(s.diagnostics.up_current_raw, Some(870));
        assert_eq!(s.bays[2].device, None);
        // "time" 10:15:42, event 08:02:51: 2 h 12 min 51 s before the epoch
        assert_eq!(s.events[1].at_ms, Some((epoch - 7971) * 1000));
        assert_eq!(s.events[1].text, "fans ok");
        // stale after 3 periods + 10 s
        let old = raw.synology(epoch + 71, String::new());
        assert!(old.stale && !old.running);
        assert!(!raw.synology(epoch + 70, String::new()).stale);
    }

    #[test]
    fn unknown_alarm_and_garbage() {
        let raw = Raw::parse(
            r#"{"epoch": 10, "fans": [{"fan": 1, "rpm": null, "alarm": null}], "status_led": "x",
               "events": [["garbage", "button"]]}"#,
        )
        .unwrap();
        let s = raw.synology(11, String::new());
        assert_eq!(s.fans[0].alarm, FanAlarm::Unknown);
        assert_eq!(s.status_led, None);
        assert_eq!(s.events[0].at_ms, None);
        assert!(Raw::parse("{not json").is_err());
    }

    #[test]
    fn front_led_fields() {
        // older synohwd: no fields, nothing reported
        let s = Raw::parse(FIXTURE).unwrap().synology(0, String::new());
        assert!(!s.led_schedule_supported);
        assert_eq!((s.led_level_max, s.led_mode_now), (None, None));
        let raw = Raw::parse(
            r#"{"epoch": 10, "led_schedule_supported": true, "led_level_min": 0,
               "led_level_max": 3, "led_level_default": 3, "led_level_now": 1,
               "led_mode_now": "reduced"}"#,
        )
        .unwrap();
        let s = raw.synology(11, String::new());
        assert!(s.led_schedule_supported);
        assert_eq!(
            (s.led_level_min, s.led_level_max, s.led_level_default, s.led_level_now),
            (Some(0), Some(3), Some(3), Some(1))
        );
        assert_eq!(s.led_mode_now, Some(LedMode::Reduced));
        // unsupported (DS920+ today): nulls, garbage words and levels ignored
        let raw = Raw::parse(
            r#"{"led_schedule_supported": false, "led_level_min": null, "led_level_now": -1,
               "led_mode_now": null}"#,
        )
        .unwrap();
        let s = raw.synology(0, String::new());
        assert_eq!((s.led_level_min, s.led_level_now, s.led_mode_now), (None, None, None));
        assert_eq!(led_mode(Some("dim")), None);
        assert_eq!(led_mode(Some("off")), Some(LedMode::Off));
    }

    #[test]
    fn civil_time() {
        assert_eq!(civil_seconds("1970-01-01 00:00:00"), Some(0));
        assert_eq!(civil_seconds("2000-03-01 00:00:01"), Some(951_868_801));
        assert_eq!(civil_seconds("2026-13-01 00:00:00"), None);
        assert_eq!(civil_seconds("nope"), None);
    }
}
