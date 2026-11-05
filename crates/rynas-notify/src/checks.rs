//! Stateful checks that turn readings into events, emitted only on transitions.
//!
//! Each watcher remembers the level of every condition it saw (`None` = fine) and emits
//! `firing` when a condition appears or changes level and `resolved` when it goes away. The
//! first observation of a condition after a restart is always emitted (the engine keeps
//! its own state per key and ignores repeats and resolutions of keys that were not firing),
//! so a problem that started while rynas was down is reported and one that ended is closed.
//! Temperatures and volume usage use hysteresis so that a value hovering around a limit
//! does not flap.

use std::collections::HashMap;

use rynas_api_types::storage::{ArrayHealth, BtrfsFilesystem, DiskSummary, HealthVerdict, MdArray};
use rynas_api_types::{HardwareStatus, TaskInfo, TaskStatus, Temperature};

use crate::event::{field, firing, oneshot, resolved};
use crate::{NewEvent, NotifyCategory as Cat, NotifySeverity as Sev};

/// Most conditions a watcher remembers (M1): keys are bounded by the hardware (sensors,
/// disks, arrays, devices); past the cap the memory is cleared and the next pass emits the
/// current state again (the engine deduplicates).
pub const MAX_TRACKED: usize = 1024;

/// The last level of each condition.
#[derive(Debug, Default)]
pub struct Tracker {
    levels: HashMap<String, Option<Sev>>,
}

/// What changed for one condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// Seen for the first time, or its level changed: emit firing (`Some`) or resolved (`None`).
    Changed(Option<Sev>),
    Same,
}

impl Tracker {
    pub fn level(&self, key: &str) -> Option<Sev> {
        self.levels.get(key).copied().flatten()
    }

    pub fn observe(&mut self, key: &str, level: Option<Sev>) -> Transition {
        if self.levels.len() >= MAX_TRACKED && !self.levels.contains_key(key) {
            self.levels.clear();
        }
        match self.levels.insert(key.to_string(), level) {
            Some(prev) if prev == level => Transition::Same,
            _ => Transition::Changed(level),
        }
    }

    /// Emits `make(level)` for a changed condition, or its resolution.
    fn emit(
        &mut self,
        out: &mut Vec<NewEvent>,
        key: &str,
        level: Option<Sev>,
        make: impl FnOnce(Sev) -> NewEvent,
        resolved: impl FnOnce() -> NewEvent,
    ) {
        if let Transition::Changed(l) = self.observe(key, level) {
            out.push(match l {
                Some(s) => make(s),
                None => resolved(),
            });
        }
    }
}

// ------------------------------------------------------------------------------- hardware

/// Warning and critical limits of a sensor, °C.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TempLimits {
    pub warn_c: f32,
    pub crit_c: f32,
}

/// A temperature must fall this far below a limit before its level drops.
pub const TEMP_HYSTERESIS_C: f32 = 3.0;

/// Default limits by sensor id: CPU (Celeron/Atom Tjmax 105 °C), NVMe (throttling ~75 °C),
/// hard disks in bays (datasheets: 55–60 °C operating maximum). Other sensors (NICs, ACPI
/// zones) are not checked.
pub fn temp_limits(id: &str) -> Option<TempLimits> {
    if id == "cpu" {
        Some(TempLimits {
            warn_c: 85.0,
            crit_c: 95.0,
        })
    } else if id.starts_with("nvme") {
        Some(TempLimits {
            warn_c: 70.0,
            crit_c: 80.0,
        })
    } else if id.starts_with("bay") || id.starts_with("disk:") {
        Some(TempLimits {
            warn_c: 50.0,
            crit_c: 58.0,
        })
    } else {
        None
    }
}

/// Level of a temperature with hysteresis.
pub fn temp_level(prev: Option<Sev>, t: f32, lim: TempLimits) -> Option<Sev> {
    let h = TEMP_HYSTERESIS_C;
    if t >= lim.crit_c || (prev == Some(Sev::Critical) && t > lim.crit_c - h) {
        Some(Sev::Critical)
    } else if t >= lim.warn_c || (prev.is_some() && t > lim.warn_c - h) {
        Some(Sev::Warning)
    } else {
        None
    }
}

/// Provider data older than this means the provider stopped updating.
pub const HW_STALE_S: u64 = 300;

/// Fans, temperatures and the provider itself (synohwd).
#[derive(Debug, Default)]
pub struct HardwareWatch {
    t: Tracker,
}

const HW: &str = "hardware";

impl HardwareWatch {
    /// `hw`: the provider status (None: no provider); `temps`: the live temperatures.
    pub fn check(&mut self, hw: Option<&HardwareStatus>, temps: &[Temperature]) -> Vec<NewEvent> {
        let mut out = Vec::new();
        let mut seen: Vec<(String, String, f32)> = temps
            .iter()
            .filter_map(|t| Some((t.id.clone(), t.label.clone(), t.celsius?)))
            .collect();
        if let Some(hw) = hw {
            // bays the live sample does not have (asleep disks report no temperature)
            for b in &hw.bays {
                let id = format!("bay{}", b.number);
                if let Some(c) = b.celsius
                    && !seen.iter().any(|(i, ..)| *i == id)
                {
                    seen.push((id, format!("Bay {}", b.number), c));
                }
            }
            for f in &hw.fans {
                let key = format!("hardware:fan:{}", f.label);
                let label = f.label.clone();
                self.t.emit(
                    &mut out,
                    &key,
                    f.alarm.then_some(Sev::Error),
                    |s| {
                        field(
                            firing(Cat::Hardware, s, HW, &key, format!("Fan {label} stopped")),
                            "fan",
                            &label,
                        )
                    },
                    || {
                        resolved(
                            Cat::Hardware,
                            HW,
                            &key,
                            format!("Fan {label} is running again"),
                        )
                    },
                );
            }
            let key = "hardware:fan_fail";
            self.t.emit(
                &mut out,
                key,
                hw.fan_fail.then_some(Sev::Critical),
                |s| {
                    let mut e = firing(Cat::Hardware, s, HW, key, "Fan failure");
                    e.body = "The hardware controller reports a fan failure: check the fans before the disks overheat.".into();
                    e
                },
                || resolved(Cat::Hardware, HW, key, "Fans back to normal"),
            );
            let key = "hardware:provider:stale";
            let stale = hw.age_s.is_some_and(|a| a > HW_STALE_S);
            let age = hw.age_s.unwrap_or(0);
            self.t.emit(
                &mut out,
                key,
                stale.then_some(Sev::Warning),
                |s| {
                    let mut e = firing(Cat::Hardware, s, HW, key, "Hardware monitor not updating");
                    e.body = format!("The hardware provider ({}) has not written its status for {age} s: fan and temperature alarms are blind.", hw.provider.as_deref().unwrap_or("unknown"));
                    e
                },
                || resolved(Cat::Hardware, HW, key, "Hardware monitor updating again"),
            );
        }
        for (id, label, c) in seen {
            let Some(lim) = temp_limits(&id) else {
                continue;
            };
            let key = format!("hardware:temp:{id}");
            let level = temp_level(self.t.level(&key), c, lim);
            self.t.emit(
                &mut out,
                &key,
                level,
                |s| {
                    let mut e = firing(
                        Cat::Hardware,
                        s,
                        HW,
                        &key,
                        format!("{label} temperature {c:.0} °C"),
                    );
                    e.body = format!(
                        "{label} is at {c:.1} °C (warning {:.0} °C, critical {:.0} °C).",
                        lim.warn_c, lim.crit_c
                    );
                    let e = field(e, "sensor", &id);
                    field(e, "celsius", format!("{c:.1}"))
                },
                || {
                    field(
                        resolved(
                            Cat::Hardware,
                            HW,
                            &key,
                            format!("{label} temperature back to normal ({c:.0} °C)"),
                        ),
                        "sensor",
                        &id,
                    )
                },
            );
        }
        out
    }
}

/// A temperature next to the shutdown threshold of the hardware controller (synohwd powers
/// off after two readings at or above it).
#[derive(Debug, Clone, PartialEq)]
pub struct ThermalReading {
    pub id: String,
    pub label: String,
    pub celsius: f32,
    pub shutdown_c: f32,
}

/// "Thermal shutdown imminent" from this far below the threshold.
pub const THERMAL_MARGIN_C: f32 = 2.0;

impl HardwareWatch {
    /// Sensors close to the controller's shutdown threshold: one critical condition per
    /// sensor (`hardware:thermal:<id>`), resolved [`TEMP_HYSTERESIS_C`] below the margin.
    pub fn check_thermal(&mut self, readings: &[ThermalReading]) -> Vec<NewEvent> {
        let mut out = Vec::new();
        for r in readings {
            let key = format!("hardware:thermal:{}", r.id);
            let near = r.shutdown_c - THERMAL_MARGIN_C;
            let was = self.t.level(&key).is_some();
            let hot = r.celsius >= near || (was && r.celsius > near - TEMP_HYSTERESIS_C);
            let (label, c, limit) = (r.label.clone(), r.celsius, r.shutdown_c);
            self.t.emit(
                &mut out,
                &key,
                hot.then_some(Sev::Critical),
                |s| {
                    let mut e = firing(
                        Cat::Hardware,
                        s,
                        HW,
                        &key,
                        format!("Thermal shutdown imminent: {label} {c:.0} °C"),
                    );
                    e.body = format!(
                        "{label} is at {c:.1} °C; the hardware controller powers the NAS off at {limit:.0} °C (when thermal shutdown is on). Check the fans and the airflow."
                    );
                    let e = field(e, "sensor", &r.id);
                    field(e, "celsius", format!("{c:.1}"))
                },
                || {
                    resolved(
                        Cat::Hardware,
                        HW,
                        &key,
                        format!("{label} cooled down ({c:.0} °C)"),
                    )
                },
            );
        }
        out
    }
}

// -------------------------------------------------------------------------------- storage

/// Volume usage limits, percent: warning, error, critical.
pub const USAGE_LEVELS: [(f32, Sev); 3] = [
    (95.0, Sev::Critical),
    (90.0, Sev::Error),
    (80.0, Sev::Warning),
];
/// Usage must fall this many points below a limit before its level drops.
pub const USAGE_HYSTERESIS_PCT: f32 = 2.0;

/// Level of a volume usage with hysteresis.
pub fn usage_level(prev: Option<Sev>, pct: f32) -> Option<Sev> {
    for (limit, sev) in USAGE_LEVELS {
        let keep = prev.is_some_and(|p| p >= sev) && pct > limit - USAGE_HYSTERESIS_PCT;
        if pct >= limit || keep {
            return Some(sev);
        }
    }
    None
}

/// Disk verdicts, md arrays, btrfs device errors and volume usage, from the storage
/// inventory (which never wakes a disk: SMART is only the cached on-demand reading).
#[derive(Debug, Default)]
pub struct StorageWatch {
    t: Tracker,
    /// Last error total per btrfs device, to report increases.
    btrfs_errors: HashMap<String, u64>,
}

const ST: &str = "storage";

fn disk_id(d: &DiskSummary) -> &str {
    if d.serial.is_empty() {
        &d.name
    } else {
        &d.serial
    }
}

impl StorageWatch {
    pub fn check(
        &mut self,
        disks: &[DiskSummary],
        arrays: &[MdArray],
        filesystems: &[BtrfsFilesystem],
    ) -> Vec<NewEvent> {
        let mut out = Vec::new();
        for d in disks {
            let level = match d.health {
                HealthVerdict::Unknown => continue,
                HealthVerdict::Healthy => None,
                HealthVerdict::Warning => Some(Sev::Warning),
                HealthVerdict::Failing => Some(Sev::Critical),
            };
            let key = format!("storage:disk:{}:health", disk_id(d));
            let name = format!(
                "{} ({})",
                d.name,
                if d.model.is_empty() { "disk" } else { &d.model }
            );
            let with_fields = |mut e: NewEvent| {
                e.fields.insert("disk".into(), d.name.clone());
                e.fields.insert("serial".into(), d.serial.clone());
                if let Some(b) = d.bay {
                    e.fields.insert("bay".into(), b.to_string());
                }
                e
            };
            self.t.emit(
                &mut out,
                &key,
                level,
                |s| {
                    let word = if s == Sev::Critical {
                        "failing"
                    } else {
                        "needs attention"
                    };
                    let mut e = firing(Cat::Storage, s, ST, &key, format!("Disk {name} {word}"));
                    e.body = d
                        .health_reasons
                        .iter()
                        .map(|r| format!("- {}", r.message))
                        .collect::<Vec<_>>()
                        .join("\n");
                    with_fields(e)
                },
                || {
                    with_fields(resolved(
                        Cat::Storage,
                        ST,
                        &key,
                        format!("Disk {name} healthy again"),
                    ))
                },
            );
        }
        for a in arrays {
            let level = match a.health {
                ArrayHealth::Unknown => continue,
                ArrayHealth::Healthy => None,
                ArrayHealth::Degraded | ArrayHealth::Failed => Some(Sev::Critical),
                ArrayHealth::Inactive => Some(Sev::Error),
            };
            let key = format!("storage:md:{}:health", a.name);
            let word = match a.health {
                ArrayHealth::Degraded => "degraded",
                ArrayHealth::Failed => "failed",
                _ => "inactive",
            };
            self.t.emit(
                &mut out,
                &key,
                level,
                |s| {
                    let mut e =
                        firing(Cat::Storage, s, ST, &key, format!("RAID {} {word}", a.name));
                    let faulty: Vec<_> = a
                        .members
                        .iter()
                        .filter(|m| !matches!(m.role, rynas_api_types::storage::MemberRole::Active))
                        .map(|m| format!("{} ({:?})", m.device, m.role).to_lowercase())
                        .collect();
                    e.body = format!(
                        "{} ({}): state \"{}\", {} of {} members missing.{}",
                        a.name,
                        a.level,
                        a.state,
                        a.degraded,
                        a.raid_disks,
                        if faulty.is_empty() {
                            String::new()
                        } else {
                            format!(" Members: {}.", faulty.join(", "))
                        }
                    );
                    field(field(e, "array", &a.name), "level", &a.level)
                },
                || {
                    field(
                        resolved(
                            Cat::Storage,
                            ST,
                            &key,
                            format!("RAID {} healthy again", a.name),
                        ),
                        "array",
                        &a.name,
                    )
                },
            );
        }
        for fs in filesystems {
            let name = fs
                .label
                .clone()
                .or_else(|| fs.mountpoint.clone())
                .unwrap_or_else(|| fs.uuid.chars().take(8).collect());
            for dev in &fs.devices {
                let e = &dev.errors;
                let total = e.write_io + e.read_io + e.flush_io + e.corruption + e.generation;
                let id = format!("{}:{}", fs.uuid, dev.devid);
                let key = format!("storage:btrfs:{id}:errors");
                let before = self.btrfs_errors.insert(id.clone(), total);
                if self.btrfs_errors.len() > MAX_TRACKED {
                    self.btrfs_errors.clear();
                }
                let level = (total > 0).then_some(Sev::Error);
                let grew = before.is_some_and(|b| total > b);
                let body = format!(
                    "Device {} of volume {name}: write {} read {} flush {} corruption {} generation {} errors.",
                    dev.path, e.write_io, e.read_io, e.flush_io, e.corruption, e.generation
                );
                let make = |s| {
                    let mut ev = firing(
                        Cat::Storage,
                        s,
                        ST,
                        &key,
                        format!("Volume {name}: device errors on {}", dev.path),
                    );
                    ev.body = body.clone();
                    field(field(ev, "volume", &name), "device", &dev.path)
                };
                if grew && self.t.level(&key).is_some() {
                    // still firing, more errors: the engine counts the repeat
                    out.push(make(Sev::Error));
                    continue;
                }
                self.t.emit(&mut out, &key, level, make, || {
                    resolved(
                        Cat::Storage,
                        ST,
                        &key,
                        format!("Volume {name}: device error counters cleared"),
                    )
                });
                let key = format!("storage:btrfs:{id}:missing");
                self.t.emit(
                    &mut out,
                    &key,
                    dev.missing.then_some(Sev::Critical),
                    |s| {
                        field(
                            firing(
                                Cat::Storage,
                                s,
                                ST,
                                &key,
                                format!("Volume {name}: device {} missing", dev.devid),
                            ),
                            "volume",
                            &name,
                        )
                    },
                    || {
                        resolved(
                            Cat::Storage,
                            ST,
                            &key,
                            format!("Volume {name}: device {} back", dev.devid),
                        )
                    },
                );
            }
            // usage: only for mounted volumes with sizes
            let total = fs.used + fs.free_estimated;
            if fs.mountpoint.is_none() || total == 0 {
                continue;
            }
            let pct = fs.used as f64 * 100.0 / total as f64;
            let pct = pct as f32;
            let key = format!("storage:volume:{}:usage", fs.uuid);
            let level = usage_level(self.t.level(&key), pct);
            self.t.emit(
                &mut out,
                &key,
                level,
                |s| {
                    let mut e = firing(
                        Cat::Storage,
                        s,
                        ST,
                        &key,
                        format!("Volume {name} {pct:.0}% full"),
                    );
                    e.body = format!(
                        "{name} uses {:.1} GiB, about {:.1} GiB free ({pct:.1}%).",
                        fs.used as f64 / 1073741824.0,
                        fs.free_estimated as f64 / 1073741824.0
                    );
                    field(field(e, "volume", &name), "used_pct", format!("{pct:.1}"))
                },
                || {
                    resolved(
                        Cat::Storage,
                        ST,
                        &key,
                        format!(
                            "Volume {name} back under {:.0}% ({pct:.0}%)",
                            USAGE_LEVELS[2].0
                        ),
                    )
                },
            );
        }
        out
    }
}

// ---------------------------------------------------------------------------------- tasks

/// The event of a finished task: a failure opens `task:<type>` (config applies:
/// `config:apply`, category system), the next success of the same type closes it;
/// cancellations say nothing.
pub fn task_event(t: &TaskInfo) -> Option<NewEvent> {
    let (cat, source, key, what) = if t.kind == "config.apply" {
        (
            Cat::System,
            "config",
            "config:apply".to_string(),
            "Configuration apply".to_string(),
        )
    } else {
        (
            Cat::Task,
            "task",
            format!("task:{}", t.kind),
            format!("Task {}", t.kind),
        )
    };
    let e = match t.status {
        TaskStatus::Error => {
            let mut e = firing(cat, Sev::Error, source, &key, format!("{what} failed"));
            e.body = t.error.clone().unwrap_or_default();
            e
        }
        TaskStatus::Ok => resolved(cat, source, &key, format!("{what} succeeded")),
        _ => return None,
    };
    Some(field(
        field(field(e, "task_id", &t.id), "type", &t.kind),
        "user",
        &t.user,
    ))
}

// ------------------------------------------------------------------------------- security

/// A login lockout (rate limit reached) for `user` from `ip`; repeats within the dedup
/// window of the engine are counted.
pub fn auth_lockout(user: &str, ip: &str, retry_after_s: u64) -> NewEvent {
    let user = safe_word(user);
    let mut e = oneshot(
        Cat::Security,
        Sev::Warning,
        "auth",
        format!("auth:lockout:{user}"),
        format!("Login locked for {user} after failed attempts"),
    );
    e.body = format!(
        "Too many failed logins for {user} from {ip}: further attempts are refused for {retry_after_s} s."
    );
    field(
        field(field(e, "user", user), "ip", ip),
        "retry_after_s",
        retry_after_s,
    )
}

/// Two-factor authentication turned on or off for `user`.
pub fn totp_changed(user: &str, enabled: bool) -> NewEvent {
    let user = safe_word(user);
    let (sev, title) = if enabled {
        (
            Sev::Notice,
            format!("Two-factor authentication enabled for {user}"),
        )
    } else {
        (
            Sev::Warning,
            format!("Two-factor authentication disabled for {user}"),
        )
    };
    let e = oneshot(
        Cat::Security,
        sev,
        "auth",
        format!("auth:totp:{user}:{enabled}"),
        title,
    );
    field(e, "user", user)
}

/// A user name as it may appear in a title: printable, at most 64 characters (names in
/// lockout events come from the login form).
fn safe_word(s: &str) -> &str {
    let end = s
        .char_indices()
        .take_while(|(i, c)| *i < 64 && !c.is_control())
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventState;
    use rynas_api_types::Fan;
    use rynas_api_types::storage::{BtrfsDevice, BtrfsDeviceErrors};

    #[test]
    fn temperature_hysteresis() {
        let lim = temp_limits("bay1").unwrap();
        assert_eq!(temp_level(None, 49.0, lim), None);
        assert_eq!(temp_level(None, 50.0, lim), Some(Sev::Warning));
        assert_eq!(
            temp_level(Some(Sev::Warning), 48.0, lim),
            Some(Sev::Warning)
        );
        assert_eq!(temp_level(Some(Sev::Warning), 47.0, lim), None);
        assert_eq!(
            temp_level(Some(Sev::Warning), 58.5, lim),
            Some(Sev::Critical)
        );
        assert_eq!(
            temp_level(Some(Sev::Critical), 56.0, lim),
            Some(Sev::Critical)
        );
        assert_eq!(
            temp_level(Some(Sev::Critical), 55.0, lim),
            Some(Sev::Warning)
        );
        assert!(temp_limits("nic:eth0").is_none());
    }

    #[test]
    fn thermal_shutdown_imminent() {
        let mut w = HardwareWatch::default();
        let r = |c: f32| {
            vec![ThermalReading {
                id: "bay2".into(),
                label: "Bay 2".into(),
                celsius: c,
                shutdown_c: 61.0,
            }]
        };
        // the first observation is always emitted (the engine ignores unknown resolutions)
        assert_eq!(w.check_thermal(&r(55.0))[0].state, EventState::Resolved);
        assert!(w.check_thermal(&r(55.0)).is_empty());
        let e = w.check_thermal(&r(59.0));
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].state, EventState::Firing);
        assert_eq!(e[0].severity, Sev::Critical);
        assert_eq!(e[0].key.as_deref(), Some("hardware:thermal:bay2"));
        assert!(e[0].title.contains("Thermal shutdown imminent"));
        // repeats and the hysteresis band stay silent
        assert!(w.check_thermal(&r(60.0)).is_empty());
        assert!(w.check_thermal(&r(56.5)).is_empty());
        let e = w.check_thermal(&r(55.5));
        assert_eq!(e[0].state, EventState::Resolved);
    }

    #[test]
    fn hardware_transitions_only() {
        let mut w = HardwareWatch::default();
        let mut hw = HardwareStatus {
            provider: Some("synology".into()),
            fans: vec![Fan {
                label: "1".into(),
                rpm: Some(900),
                alarm: false,
            }],
            age_s: Some(5),
            ..Default::default()
        };
        let temp = |c| {
            vec![Temperature {
                id: "cpu".into(),
                label: "CPU".into(),
                celsius: Some(c),
            }]
        };
        // first pass: everything reported (resolutions are no-ops in the engine)
        let first = w.check(Some(&hw), &temp(50.0));
        assert!(first.iter().all(|e| e.state == EventState::Resolved));
        assert_eq!(first.len(), 4);
        assert!(w.check(Some(&hw), &temp(51.0)).is_empty());
        hw.fans[0].alarm = true;
        let ev = w.check(Some(&hw), &temp(86.0));
        assert_eq!(ev.len(), 2);
        assert!(
            ev.iter()
                .any(|e| e.key.as_deref() == Some("hardware:fan:1") && e.severity == Sev::Error)
        );
        assert!(
            ev.iter().any(
                |e| e.key.as_deref() == Some("hardware:temp:cpu") && e.severity == Sev::Warning
            )
        );
        // 83 °C: within the hysteresis, nothing new
        assert!(w.check(Some(&hw), &temp(83.0)).is_empty());
        let ev = w.check(Some(&hw), &temp(81.0));
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].state, EventState::Resolved);
    }

    #[test]
    fn usage_levels_with_hysteresis() {
        assert_eq!(usage_level(None, 79.9), None);
        assert_eq!(usage_level(None, 80.0), Some(Sev::Warning));
        assert_eq!(usage_level(Some(Sev::Warning), 78.5), Some(Sev::Warning));
        assert_eq!(usage_level(Some(Sev::Warning), 77.9), None);
        assert_eq!(usage_level(Some(Sev::Warning), 91.0), Some(Sev::Error));
        assert_eq!(usage_level(Some(Sev::Error), 88.5), Some(Sev::Error));
        assert_eq!(usage_level(Some(Sev::Error), 87.0), Some(Sev::Warning));
        assert_eq!(usage_level(None, 99.0), Some(Sev::Critical));
        assert_eq!(usage_level(Some(Sev::Critical), 93.5), Some(Sev::Critical));
    }

    #[test]
    fn storage_events() {
        let mut w = StorageWatch::default();
        let disk = |h| DiskSummary {
            name: "sda".into(),
            serial: "S1".into(),
            model: "WD".into(),
            health: h,
            ..Default::default()
        };
        let md = |h| MdArray {
            name: "md2".into(),
            level: "raid1".into(),
            health: h,
            raid_disks: 2,
            ..Default::default()
        };
        let fs = |used, errs| BtrfsFilesystem {
            uuid: "u1".into(),
            label: Some("vol1".into()),
            mountpoint: Some("/vol1".into()),
            used,
            free_estimated: 100 - used,
            devices: vec![BtrfsDevice {
                devid: 1,
                path: "/dev/md2".into(),
                errors: BtrfsDeviceErrors {
                    read_io: errs,
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        let ev = w.check(
            &[disk(HealthVerdict::Unknown)],
            &[md(ArrayHealth::Healthy)],
            &[fs(50, 0)],
        );
        // md resolved, btrfs errors + missing resolved, usage resolved; unknown disk skipped
        assert_eq!(ev.len(), 4);
        let ev = w.check(
            &[disk(HealthVerdict::Failing)],
            &[md(ArrayHealth::Degraded)],
            &[fs(91, 2)],
        );
        let sev = |k: &str| {
            ev.iter()
                .find(|e| e.key.as_deref() == Some(k))
                .map(|e| e.severity)
        };
        assert_eq!(sev("storage:disk:S1:health"), Some(Sev::Critical));
        assert_eq!(sev("storage:md:md2:health"), Some(Sev::Critical));
        assert_eq!(sev("storage:btrfs:u1:1:errors"), Some(Sev::Error));
        assert_eq!(sev("storage:volume:u1:usage"), Some(Sev::Error));
        assert_eq!(ev.len(), 4);
        // more errors: a repeat of the firing event; same usage: nothing
        let ev = w.check(
            &[disk(HealthVerdict::Failing)],
            &[md(ArrayHealth::Degraded)],
            &[fs(91, 3)],
        );
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].key.as_deref(), Some("storage:btrfs:u1:1:errors"));
    }

    #[test]
    fn task_events() {
        let mut t = TaskInfo {
            id: "UPID:x".into(),
            kind: "config.apply".into(),
            user: "alice".into(),
            status: TaskStatus::Error,
            error: Some("testparm failed".into()),
            ..Default::default()
        };
        let e = task_event(&t).unwrap();
        assert_eq!(e.key.as_deref(), Some("config:apply"));
        assert_eq!(e.category, Cat::System);
        assert_eq!(e.body, "testparm failed");
        t.status = TaskStatus::Ok;
        assert_eq!(task_event(&t).unwrap().state, EventState::Resolved);
        t.status = TaskStatus::Cancelled;
        assert!(task_event(&t).is_none());
        t.kind = "smb.recycle-cleanup".into();
        t.status = TaskStatus::Error;
        assert_eq!(
            task_event(&t).unwrap().key.as_deref(),
            Some("task:smb.recycle-cleanup")
        );
    }

    #[test]
    fn lockout_names_are_cut() {
        let e = auth_lockout(&"x".repeat(300), "10.0.0.1", 30);
        assert_eq!(e.fields["user"].len(), 64);
        assert_eq!(safe_word("bob\nEvil"), "bob");
    }
}
