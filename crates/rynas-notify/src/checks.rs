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

use rynas_api_types::storage::{
    ArrayHealth, BtrfsFilesystem, DiskSummary, FlexLayout, HealthVerdict, MdArray,
};
use rynas_api_types::{HardwareStatus, TaskInfo, TaskStatus, Temperature};

use crate::event::{code, field, firing, oneshot, resolved};
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

/// Fields of a temperature event: sensor id, its label and the reading (one decimal).
fn temp_fields(e: NewEvent, sensor: &str, label: &str, c: f32) -> NewEvent {
    let e = field(e, "sensor", sensor);
    let e = field(e, "label", label);
    field(e, "celsius", format!("{c:.1}"))
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
                        let e = firing(Cat::Hardware, s, HW, &key, format!("Fan {label} stopped"));
                        field(code(e, "hw.fan_stopped"), "fan", &label)
                    },
                    || {
                        let e = resolved(
                            Cat::Hardware,
                            HW,
                            &key,
                            format!("Fan {label} is running again"),
                        );
                        field(code(e, "hw.fan_running"), "fan", &label)
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
                    code(e, "hw.fan_failure")
                },
                || {
                    code(
                        resolved(Cat::Hardware, HW, key, "Fans back to normal"),
                        "hw.fans_ok",
                    )
                },
            );
            let key = "hardware:provider:stale";
            let stale = hw.age_s.is_some_and(|a| a > HW_STALE_S);
            let age = hw.age_s.unwrap_or(0);
            self.t.emit(
                &mut out,
                key,
                stale.then_some(Sev::Warning),
                |s| {
                    let provider = hw.provider.as_deref().unwrap_or("unknown");
                    let mut e = firing(Cat::Hardware, s, HW, key, "Hardware monitor not updating");
                    e.body = format!("The hardware provider ({provider}) has not written its status for {age} s: fan and temperature alarms are blind.");
                    let e = field(code(e, "hw.monitor_stale"), "provider", provider);
                    field(e, "age_s", age)
                },
                || {
                    code(
                        resolved(Cat::Hardware, HW, key, "Hardware monitor updating again"),
                        "hw.monitor_ok",
                    )
                },
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
                    let e = temp_fields(code(e, "hw.temp_high"), &id, &label, c);
                    let e = field(e, "warn_c", format!("{:.0}", lim.warn_c));
                    field(e, "crit_c", format!("{:.0}", lim.crit_c))
                },
                || {
                    let e = resolved(
                        Cat::Hardware,
                        HW,
                        &key,
                        format!("{label} temperature back to normal ({c:.0} °C)"),
                    );
                    temp_fields(code(e, "hw.temp_ok"), &id, &label, c)
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
                    let e = temp_fields(code(e, "hw.thermal_shutdown"), &r.id, &label, c);
                    field(e, "shutdown_c", format!("{limit:.0}"))
                },
                || {
                    let e = resolved(
                        Cat::Hardware,
                        HW,
                        &key,
                        format!("{label} cooled down ({c:.0} °C)"),
                    );
                    temp_fields(code(e, "hw.thermal_ok"), &r.id, &label, c)
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
        flex: &[FlexLayout],
    ) -> Vec<NewEvent> {
        let mut out = Vec::new();
        let tier_arrays: Vec<&str> = flex
            .iter()
            .flat_map(|l| l.tiers.iter().filter_map(|t| t.array.as_deref()))
            .collect();
        for d in disks {
            let level = match d.health {
                HealthVerdict::Unknown => continue,
                HealthVerdict::Healthy => None,
                HealthVerdict::Warning => Some(Sev::Warning),
                HealthVerdict::Failing => Some(Sev::Critical),
            };
            let key = format!("storage:disk:{}:health", disk_id(d));
            let name = if d.model.is_empty() {
                d.name.clone()
            } else {
                format!("{} ({})", d.name, d.model)
            };
            let with_fields = |mut e: NewEvent| {
                e.fields.insert("disk".into(), d.name.clone());
                e.fields.insert("disk_label".into(), name.clone());
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
                    let (word, c) = if s == Sev::Critical {
                        ("failing", "storage.disk_failing")
                    } else {
                        ("needs attention", "storage.disk_warning")
                    };
                    let mut e = firing(Cat::Storage, s, ST, &key, format!("Disk {name} {word}"));
                    // reasons marked as seen do not weigh in the verdict: not listed either
                    e.body = d
                        .health_reasons
                        .iter()
                        .filter(|r| r.acknowledged.is_none())
                        .map(|r| format!("- {}", r.message))
                        .collect::<Vec<_>>()
                        .join("\n");
                    with_fields(code(e, c))
                },
                || {
                    let e = resolved(Cat::Storage, ST, &key, format!("Disk {name} healthy again"));
                    with_fields(code(e, "storage.disk_ok"))
                },
            );
        }
        // a flexible RAID reports once for all its tiers (below)
        for a in arrays.iter().filter(|a| !tier_arrays.contains(&a.name.as_str())) {
            let level = match a.health {
                ArrayHealth::Unknown => continue,
                ArrayHealth::Healthy => None,
                ArrayHealth::Degraded | ArrayHealth::Failed => Some(Sev::Critical),
                ArrayHealth::Inactive => Some(Sev::Error),
            };
            let key = format!("storage:md:{}:health", a.name);
            let (word, c) = match a.health {
                ArrayHealth::Degraded => ("degraded", "storage.raid_degraded"),
                ArrayHealth::Failed => ("failed", "storage.raid_failed"),
                _ => ("inactive", "storage.raid_inactive"),
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
                    let mut e = field(field(code(e, c), "array", &a.name), "level", &a.level);
                    e = field(e, "state", &a.state);
                    e = field(e, "degraded", a.degraded);
                    e = field(e, "raid_disks", a.raid_disks);
                    if !faulty.is_empty() {
                        e = field(e, "members", faulty.join(", "));
                    }
                    e
                },
                || {
                    let e = resolved(
                        Cat::Storage,
                        ST,
                        &key,
                        format!("RAID {} healthy again", a.name),
                    );
                    field(code(e, "storage.raid_ok"), "array", &a.name)
                },
            );
        }
        for l in flex {
            self.check_flex(&mut out, l);
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
                    let mut ev = field(
                        field(code(ev, "storage.btrfs_errors"), "volume", &name),
                        "device",
                        &dev.path,
                    );
                    for (k, v) in [
                        ("write_io", e.write_io),
                        ("read_io", e.read_io),
                        ("flush_io", e.flush_io),
                        ("corruption", e.corruption),
                        ("generation", e.generation),
                    ] {
                        ev = field(ev, k, v);
                    }
                    ev
                };
                if grew && self.t.level(&key).is_some() {
                    // still firing, more errors: the engine counts the repeat
                    out.push(make(Sev::Error));
                    continue;
                }
                self.t.emit(&mut out, &key, level, make, || {
                    let ev = resolved(
                        Cat::Storage,
                        ST,
                        &key,
                        format!("Volume {name}: device error counters cleared"),
                    );
                    let ev = field(code(ev, "storage.btrfs_errors_cleared"), "volume", &name);
                    field(ev, "device", &dev.path)
                });
                let key = format!("storage:btrfs:{id}:missing");
                self.t.emit(
                    &mut out,
                    &key,
                    dev.missing.then_some(Sev::Critical),
                    |s| {
                        let ev = firing(
                            Cat::Storage,
                            s,
                            ST,
                            &key,
                            format!("Volume {name}: device {} missing", dev.devid),
                        );
                        let ev = field(code(ev, "storage.btrfs_missing"), "volume", &name);
                        field(ev, "devid", dev.devid)
                    },
                    || {
                        let ev = resolved(
                            Cat::Storage,
                            ST,
                            &key,
                            format!("Volume {name}: device {} back", dev.devid),
                        );
                        let ev = field(code(ev, "storage.btrfs_back"), "volume", &name);
                        field(ev, "devid", dev.devid)
                    },
                );
            }
            // a configured volume that cannot be mounted because devices are missing (only
            // filesystems of several devices are watched for it)
            let repair = fs.repair.as_ref();
            let watched = repair.is_some() || fs.devices.len() > 1;
            let key = format!("storage:btrfs:{}:unmounted", fs.uuid);
            let unmounted = repair.filter(|r| !r.mounted);
            let (missing, total) = unmounted.map(|r| (r.missing_count, r.total_devices)).unwrap_or_default();
            if watched {
            self.t.emit(
                &mut out,
                &key,
                unmounted.map(|_| Sev::Critical),
                |s| {
                    let mut ev = firing(Cat::Storage, s, ST, &key, format!("Volume {name} cannot be mounted: devices missing"));
                    ev.body = if total > 0 {
                        format!("{missing} of the {total} devices of volume {name} are missing, so it was not mounted. Reconnect them, or mount the volume degraded from Storage when every chunk still has a copy, then replace the missing device.")
                    } else {
                        format!("Devices of volume {name} are missing, so it was not mounted. Reconnect them, or mount the volume degraded from Storage when every chunk still has a copy, then replace the missing device.")
                    };
                    let ev = field(field(code(ev, "storage.btrfs_unmounted"), "volume", &name), "missing", missing);
                    field(ev, "total", total)
                },
                || {
                    let ev = resolved(Cat::Storage, ST, &key, format!("Volume {name} is mounted"));
                    field(code(ev, "storage.btrfs_unmounted_ok"), "volume", &name)
                },
            );
            // every device back after a degraded mount, no clean scrub since
            let key = format!("storage:btrfs:{}:scrub", fs.uuid);
            self.t.emit(
                &mut out,
                &key,
                repair.filter(|r| r.scrub_needed).map(|_| Sev::Warning),
                |s| {
                    let mut ev = firing(Cat::Storage, s, ST, &key, format!("Volume {name}: run a data scrub"));
                    ev.body = format!("Every device of volume {name} is back after it ran degraded, but the devices that were missing lack the copies of what was written meanwhile: losing the others now costs data. Run a data scrub from Storage; until it finishes without errors, the returned devices are not mounted degraded on their own.");
                    field(code(ev, "storage.btrfs_scrub_needed"), "volume", &name)
                },
                || {
                    let ev = resolved(Cat::Storage, ST, &key, format!("Volume {name}: copies up to date"));
                    field(code(ev, "storage.btrfs_scrub_needed_ok"), "volume", &name)
                },
            );
            // chunks a degraded mount wrote with a weaker profile, once no device is missing
            let key = format!("storage:btrfs:{}:weak", fs.uuid);
            let weak = repair.filter(|r| r.mounted && !r.degraded() && !r.convert.is_empty());
            let kinds = weak
                .map(|r| r.convert.iter().map(|c| format!("{} {}", c.kind, c.from.join("/"))).collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            self.t.emit(
                &mut out,
                &key,
                weak.map(|_| Sev::Warning),
                |s| {
                    let mut ev = firing(Cat::Storage, s, ST, &key, format!("Volume {name}: chunks with less redundancy"));
                    ev.body = format!("Volume {name} has chunks written with a weaker profile while it was degraded ({kinds}). Convert them back from Storage so that a lost device costs no data.");
                    field(field(code(ev, "storage.btrfs_weak_chunks"), "volume", &name), "chunks", &kinds)
                },
                || {
                    let ev = resolved(Cat::Storage, ST, &key, format!("Volume {name}: redundancy restored"));
                    field(code(ev, "storage.btrfs_weak_chunks_ok"), "volume", &name)
                },
            );
            }
            // usage: only for mounted volumes with sizes
            // counted once (a RAID1 holds every byte twice: `used` is raw)
            let (logical, total) = (fs.logical_used(), fs.usable_size());
            if fs.mountpoint.is_none() || total == 0 {
                continue;
            }
            let free_bytes = total.saturating_sub(logical);
            let pct = logical as f64 * 100.0 / total as f64;
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
                    let (used, free) = (
                        format!("{:.1}", logical as f64 / 1073741824.0),
                        format!("{:.1}", free_bytes as f64 / 1073741824.0),
                    );
                    e.body = format!("{name} uses {used} GiB, about {free} GiB free ({pct:.1}%).");
                    let e = field(code(e, "storage.volume_full"), "volume", &name);
                    let e = field(field(e, "used_pct", format!("{pct:.1}")), "used_gib", used);
                    field(e, "free_gib", free)
                },
                || {
                    let limit = USAGE_LEVELS[2].0;
                    let e = resolved(
                        Cat::Storage,
                        ST,
                        &key,
                        format!("Volume {name} back under {limit:.0}% ({pct:.0}%)"),
                    );
                    let e = field(code(e, "storage.volume_ok"), "volume", &name);
                    let e = field(e, "used_pct", format!("{pct:.1}"));
                    field(e, "limit_pct", format!("{limit:.0}"))
                },
            );
        }
        out
    }

    /// One event per flexible RAID, however many tiers a lost disk degrades: its body names
    /// each missing disk once with the tiers it is missing from ("sdb missing from tiers
    /// 1–2"). A disk that vanished has no name left: it is counted from the tiers' missing
    /// members.
    fn check_flex(&mut self, out: &mut Vec<NewEvent>, l: &FlexLayout) {
        let health = l.health();
        let level = match health {
            ArrayHealth::Unknown => return,
            ArrayHealth::Healthy => None,
            ArrayHealth::Degraded | ArrayHealth::Failed => Some(Sev::Critical),
            ArrayHealth::Inactive => Some(Sev::Error),
        };
        let name = l.vg.strip_prefix("rynas_").unwrap_or(&l.vg).to_string();
        let flex_level = l
            .level()
            .map(|lv| if lv == rynas_api_types::storage::RaidLevel::Flex2 { "flex2" } else { "flex1" })
            .unwrap_or("flex");
        let key = format!("storage:flex:{}:health", l.vg);
        let (word, c) = match health {
            ArrayHealth::Degraded => ("degraded", "storage.flex_degraded"),
            ArrayHealth::Failed => ("failed", "storage.flex_failed"),
            _ => ("inactive", "storage.flex_inactive"),
        };
        // disk → tiers it is missing from; the unnamed rest per tier
        let mut by_disk: Vec<(String, Vec<u32>)> = Vec::new();
        let mut unnamed: Vec<u32> = Vec::new();
        for t in &l.tiers {
            for d in &t.missing {
                match by_disk.iter_mut().find(|(x, _)| x == d) {
                    Some((_, v)) => v.push(t.index),
                    None => by_disk.push((d.clone(), vec![t.index])),
                }
            }
            if t.degraded as usize > t.missing.len() {
                unnamed.push(t.index);
            }
        }
        let tiers_text = |v: &[u32]| -> String {
            let contiguous = v.windows(2).all(|w| w[1] == w[0] + 1);
            match v {
                [one] => format!("tier {one}"),
                [first, .., last] if contiguous => format!("tiers {first}–{last}"),
                _ => format!(
                    "tiers {}",
                    v.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
                ),
            }
        };
        let mut lines: Vec<String> = by_disk
            .iter()
            .map(|(d, v)| format!("{d} missing from {}", tiers_text(v)))
            .collect();
        if !unnamed.is_empty() {
            lines.push(format!("a disk missing from {}", tiers_text(&unnamed)));
        }
        let members = lines.join(", ");
        self.t.emit(
            out,
            &key,
            level,
            |s| {
                let mut e = firing(
                    Cat::Storage,
                    s,
                    ST,
                    &key,
                    format!("Flexible RAID {name} {word}"),
                );
                e.body = if lines.is_empty() {
                    format!("{name} ({flex_level}): {word}.")
                } else {
                    format!("{name} ({flex_level}): {}.", lines.join("; "))
                };
                let mut e = field(field(code(e, c), "array", &name), "level", flex_level);
                e = field(e, "state", word);
                if !members.is_empty() {
                    e = field(e, "members", &members);
                }
                e
            },
            || {
                let e = resolved(
                    Cat::Storage,
                    ST,
                    &key,
                    format!("Flexible RAID {name} healthy again"),
                );
                field(code(e, "storage.flex_ok"), "array", &name)
            },
        );
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
    let apply = t.kind == "config.apply";
    let e = match t.status {
        TaskStatus::Error => {
            let mut e = firing(cat, Sev::Error, source, &key, format!("{what} failed"));
            e.body = t.error.clone().unwrap_or_default();
            code(
                e,
                if apply {
                    "config.apply_failed"
                } else {
                    "task.failed"
                },
            )
        }
        TaskStatus::Ok => code(
            resolved(cat, source, &key, format!("{what} succeeded")),
            if apply {
                "config.apply_ok"
            } else {
                "task.succeeded"
            },
        ),
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
        field(field(code(e, "auth.lockout"), "user", user), "ip", ip),
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
    let c = if enabled {
        "auth.totp_enabled"
    } else {
        "auth.totp_disabled"
    };
    field(code(e, c), "user", user)
}

/// A mobile device paired by `user` (`docs/phase2/mobile-devices.md` §5).
pub fn device_paired(user: &str, id: &str, device: &str, ip: &str) -> NewEvent {
    let (user, device) = (safe_word(user), safe_word(device));
    let mut e = oneshot(
        Cat::Security,
        Sev::Notice,
        "devices",
        format!("device:paired:{id}"),
        format!("Device {device} paired for {user}"),
    );
    e.body = format!("The mobile device {device} was paired for {user} from {ip}.");
    field(field(field(code(e, "device.paired"), "user", user), "device", device), "ip", ip)
}

/// A paired device renamed (from the web interface or from the app).
pub fn device_renamed(user: &str, id: &str, old: &str, new: &str) -> NewEvent {
    let (user, old, new) = (safe_word(user), safe_word(old), safe_word(new));
    let e = oneshot(
        Cat::Security,
        Sev::Info,
        "devices",
        format!("device:renamed:{id}"),
        format!("Device {old} of {user} renamed to {new}"),
    );
    field(field(field(code(e, "device.renamed"), "user", user), "old", old), "new", new)
}

/// A paired device revoked. `reason` (`rynas_api_types::devices::revoke_reason`): `user` (from
/// the web interface), `self` (unpaired from the app), `account_removed`, `account_disabled`
/// (older events: `account`), `inactive` (no refresh for 180
/// days), `unclaimed` (approved, never picked up by the phone, or never refreshed after the
/// pairing), `account_replaced` (the user name now belongs to another uid).
pub fn device_revoked(user: &str, id: &str, device: &str, reason: &str) -> NewEvent {
    let (user, device) = (safe_word(user), safe_word(device));
    let mut e = oneshot(
        Cat::Security,
        Sev::Notice,
        "devices",
        format!("device:revoked:{id}"),
        format!("Device {device} of {user} revoked"),
    );
    e.body = format!("The mobile device {device} of {user} can no longer sign in ({reason}).");
    field(
        field(field(code(e, "device.revoked"), "user", user), "device", device),
        "reason",
        reason,
    )
}

/// A refresh token was used twice: someone else may hold a copy; the device was revoked.
pub fn device_refresh_reuse(user: &str, id: &str, device: &str, ip: &str) -> NewEvent {
    let (user, device) = (safe_word(user), safe_word(device));
    let mut e = oneshot(
        Cat::Security,
        Sev::Warning,
        "devices",
        format!("device:refresh_reuse:{id}"),
        format!("Device {device} of {user} revoked: a token was used twice"),
    );
    e.body = format!(
        "An old refresh token of the mobile device {device} ({user}) came back from {ip}: a copy \
         of it may exist elsewhere. The device was revoked; pair it again."
    );
    field(field(field(code(e, "device.refresh_reuse"), "user", user), "device", device), "ip", ip)
}

/// Key of [`device_clock_skew`] and [`device_clock_ok`]: one open alarm for all the devices.
pub const DEVICE_CLOCK_KEY: &str = "device:clock_skew";

/// A device refused because its signed time is far from the NAS' clock while the NAS clock is
/// not synchronized: the NAS is the likely culprit (phones take their time from the network).
pub fn device_clock_skew(user: &str, device: &str) -> NewEvent {
    let (user, device) = (safe_word(user), safe_word(device));
    let mut e = firing(
        Cat::System,
        Sev::Warning,
        "devices",
        DEVICE_CLOCK_KEY,
        format!("Device {device} refused: the NAS clock is not synchronized"),
    );
    e.body = format!(
        "The mobile device {device} ({user}) sent a time more than 2 minutes away from the NAS' \
         clock, and the NAS clock is not synchronized: phones cannot connect until it is. Check \
         the time servers in Date and time."
    );
    e.actions = vec![rynas_api_types::notify::EventAction { label: "Open date and time".into(), url: "/system/time".into() }];
    field(field(code(e, "device.clock_skew"), "user", user), "device", device)
}

/// A device's time was accepted again after [`device_clock_skew`].
pub fn device_clock_ok() -> NewEvent {
    code(resolved(Cat::System, "devices", DEVICE_CLOCK_KEY, "Mobile devices accepted again: the clocks agree"), "device.clock_ok")
}

/// A restart, shutdown or its cancellation asked from a paired device. `action`: `restart`,
/// `shutdown`, `cancel`.
pub fn device_power(user: &str, id: &str, device: &str, action: &str) -> NewEvent {
    let (user, device) = (safe_word(user), safe_word(device));
    let e = oneshot(
        Cat::Security,
        Sev::Info,
        "devices",
        format!("device:power:{id}:{action}"),
        format!("System power by device {device}: {action}"),
    );
    field(
        field(field(code(e, "device.power"), "user", user), "device", device),
        "action",
        action,
    )
}

/// A user's password changed while devices are paired: they keep access (bound to their own
/// key); listing them shows a device someone else may have paired.
pub fn devices_kept_after_password(user: &str, devices: &[String]) -> NewEvent {
    let user = safe_word(user);
    let list = devices.iter().map(|d| safe_word(d)).collect::<Vec<_>>().join(", ");
    let n = devices.len();
    let mut e = oneshot(
        Cat::Security,
        Sev::Info,
        "devices",
        format!("device:password:{user}"),
        format!("Password of {user} changed: paired devices keep access ({n})"),
    );
    e.body = format!("Devices: {list}. Revoke any you do not recognize in Security › Mobile devices.");
    field(
        field(field(code(e, "device.password_kept"), "user", user), "n", n),
        "devices",
        list,
    )
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
    use rynas_api_types::storage::{BtrfsDevice, BtrfsDeviceErrors, BtrfsProfile};

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
            // a RAID1: raw use twice the files, the profile counts them once
            used: used * 2,
            free_estimated: 100 - used,
            data_ratio: 2.0,
            profiles: vec![BtrfsProfile {
                kind: "data".into(),
                profile: "raid1".into(),
                size: 100,
                used,
            }],
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
            &[],
        );
        // md resolved, btrfs errors + missing resolved, usage resolved; unknown disk skipped
        assert_eq!(ev.len(), 4);
        let ev = w.check(
            &[disk(HealthVerdict::Failing)],
            &[md(ArrayHealth::Degraded)],
            &[fs(91, 2)],
            &[],
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
            &[],
        );
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].key.as_deref(), Some("storage:btrfs:u1:1:errors"));
    }

    /// A btrfs volume left unmounted by a missing device fires until it is mounted; chunks a
    /// degraded mount wrote weak warn once no device is missing, until converted.
    #[test]
    fn btrfs_volume_unmounted_and_weak_chunks() {
        use rynas_api_types::storage::{BtrfsConvert, BtrfsRepair};
        let mut w = StorageWatch::default();
        let fs = |repair: Option<BtrfsRepair>, mounted: bool| BtrfsFilesystem {
            uuid: "u9".into(),
            label: Some("photos".into()),
            mountpoint: mounted.then(|| "/photos".into()),
            repair,
            ..Default::default()
        };
        let waiting = BtrfsRepair { mounted: false, total_devices: 2, missing_count: 1, ..Default::default() };
        let ev = w.check(&[], &[], &[fs(Some(waiting.clone()), false)], &[]);
        // the first pass also closes what may have been open before a restart
        assert_eq!(ev.iter().map(|e| e.code.as_str()).collect::<Vec<_>>(), ["storage.btrfs_unmounted", "storage.btrfs_scrub_needed_ok", "storage.btrfs_weak_chunks_ok"]);
        assert_eq!(ev[0].severity, Sev::Critical);
        assert_eq!(ev[0].fields["missing"], "1");
        assert_eq!(ev[0].fields["total"], "2");
        assert!(ev[0].body.contains("1 of the 2 devices"), "{}", ev[0].body);
        assert!(w.check(&[], &[], &[fs(Some(waiting), false)], &[]).is_empty(), "once");
        // mounted degraded: resolved (the per-device events tell what is missing)
        let degraded = BtrfsRepair { mounted: true, degraded_mount: true, missing: vec![2], missing_count: 1, ..Default::default() };
        let ev = w.check(&[], &[], &[fs(Some(degraded), true)], &[]);
        assert_eq!(ev.iter().map(|e| e.code.as_str()).collect::<Vec<_>>(), ["storage.btrfs_unmounted_ok"]);
        // repaired, single chunks left
        let weak = BtrfsRepair {
            mounted: true,
            convert: vec![BtrfsConvert { kind: "metadata".into(), from: vec!["single".into()], to: "raid1".into() }],
            ..Default::default()
        };
        let ev = w.check(&[], &[], &[fs(Some(weak), true)], &[]);
        assert_eq!(ev[0].code, "storage.btrfs_weak_chunks");
        assert_eq!(ev[0].fields["chunks"], "metadata single");
        // a device back after the degraded mount, not scrubbed yet: warn until it is
        let scrub = BtrfsRepair { mounted: true, scrub_needed: true, ..Default::default() };
        let ev = w.check(&[], &[], &[fs(Some(scrub), true)], &[]);
        assert!(ev.iter().any(|e| e.code == "storage.btrfs_scrub_needed" && e.severity == Sev::Warning), "{ev:?}");
        assert!(ev.iter().any(|e| e.code == "storage.btrfs_weak_chunks_ok"), "converted meanwhile: {ev:?}");
        // converted: a filesystem of two devices, nothing to repair
        let mut whole = fs(None, true);
        whole.devices = vec![BtrfsDevice { devid: 1, ..Default::default() }, BtrfsDevice { devid: 2, ..Default::default() }];
        let ev = w.check(&[], &[], &[whole], &[]);
        assert!(ev.iter().any(|e| e.code == "storage.btrfs_scrub_needed_ok"), "scrubbed: {ev:?}");
    }

    /// A disk lost from a flexible RAID degrades every tier it was in: one event for the
    /// volume, naming the disk once, and none for the tier arrays.
    #[test]
    fn one_event_per_lost_disk_not_per_tier() {
        use rynas_api_types::storage::{FlexTier, RaidLevel};
        let mut w = StorageWatch::default();
        let md = |name: &str, h| MdArray {
            name: name.into(),
            level: "raid5".into(),
            health: h,
            raid_disks: 3,
            degraded: u32::from(h == ArrayHealth::Degraded),
            ..Default::default()
        };
        let tier = |i: u32, md: &str, h, missing: &[&str]| FlexTier {
            index: i,
            level: Some(RaidLevel::Raid5),
            array: Some(md.into()),
            health: h,
            degraded: u32::from(h == ArrayHealth::Degraded),
            missing: missing.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let layout = |h, missing: &[&str]| FlexLayout {
            tolerance: 1,
            vg: "rynas_vol2".into(),
            lv: "data".into(),
            tiers: vec![tier(1, "md126", h, missing), tier(2, "md127", h, missing)],
            ..Default::default()
        };
        let arrays = |h| [md("md126", h), md("md127", h)];
        let ev = w.check(&[], &arrays(ArrayHealth::Healthy), &[], &[layout(ArrayHealth::Healthy, &[])]);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].key.as_deref(), Some("storage:flex:rynas_vol2:health"));
        let ev = w.check(
            &[],
            &arrays(ArrayHealth::Degraded),
            &[],
            &[layout(ArrayHealth::Degraded, &["sdb"])],
        );
        assert_eq!(ev.len(), 1, "{ev:?}");
        assert_eq!(ev[0].severity, Sev::Critical);
        assert_eq!(ev[0].title, "Flexible RAID vol2 degraded");
        assert_eq!(ev[0].body, "vol2 (flex1): sdb missing from tiers 1–2.");
        assert_eq!(ev[0].fields.get("members").map(String::as_str), Some("sdb missing from tiers 1–2"));
        assert_eq!(ev[0].code, "storage.flex_degraded");
        // a vanished disk has no name
        let mut w = StorageWatch::default();
        let ev = w.check(&[], &arrays(ArrayHealth::Degraded), &[], &[layout(ArrayHealth::Degraded, &[])]);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].body, "vol2 (flex1): a disk missing from tiers 1–2.");
        // back to healthy: resolved once
        let ev = w.check(&[], &arrays(ArrayHealth::Healthy), &[], &[layout(ArrayHealth::Healthy, &[])]);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].code, "storage.flex_ok");
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
