//! Temperatures from hwmon (CPU, NVMe, NICs, drivetemp) and from the hardware provider (bays).
//!
//! Disks are never woken up: drivetemp is read only for disks that did I/O in the last
//! [`DRIVE_ACTIVE`] and at most every [`DRIVE_PERIOD`] (reading SCT may reset the spin-down
//! timer), otherwise the last value is kept. Bays covered by the hardware provider (synohwd,
//! which checks the power mode before reading) use its values instead.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rynas_api_types::{Bay, Temperature};

const REDISCOVER: Duration = Duration::from_secs(60);
pub const DRIVE_ACTIVE: Duration = Duration::from_secs(30);
const DRIVE_PERIOD: Duration = Duration::from_secs(30);
const PLAUSIBLE: std::ops::RangeInclusive<f32> = -20.0..=125.0;

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Cpu,
    Nvme,
    Nic,
    /// drivetemp of this block device
    Drive(String),
}

#[derive(Debug, Clone)]
struct Source {
    id: String,
    label: String,
    input: PathBuf,
    kind: Kind,
}

pub struct Temps {
    hwmon_root: PathBuf,
    sources: Vec<Source>,
    discovered: Option<Instant>,
    /// disk → (last value, when read)
    drive_cache: HashMap<String, (Option<f32>, Instant)>,
}

fn read_trim(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn read_celsius(p: &Path) -> Option<f32> {
    let milli: i64 = read_trim(p)?.parse().ok()?;
    let c = milli as f32 / 1000.0;
    PLAUSIBLE.contains(&c).then_some(c)
}

/// `temp*_input` files of a hwmon dir, sorted, with their labels.
fn inputs(dir: &Path) -> Vec<(PathBuf, Option<String>)> {
    let mut v: Vec<_> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let stem = name
                .strip_prefix("temp")?
                .strip_suffix("_input")?
                .to_string();
            let label = read_trim(&dir.join(format!("temp{stem}_label")));
            Some((stem.parse::<u32>().unwrap_or(u32::MAX), e.path(), label))
        })
        .collect();
    v.sort_by_key(|(n, ..)| *n);
    v.into_iter().map(|(_, p, l)| (p, l)).collect()
}

/// The input with this label, else the first one.
fn pick(dir: &Path, label: &str) -> Option<PathBuf> {
    let ins = inputs(dir);
    ins.iter()
        .find(|(_, l)| l.as_deref() == Some(label))
        .or(ins.first())
        .map(|(p, _)| p.clone())
}

fn device_basename(dir: &Path) -> Option<String> {
    fs::canonicalize(dir.join("device"))
        .ok()?
        .file_name()?
        .to_str()
        .map(str::to_string)
}

fn first_entry(dir: &Path) -> Option<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names.into_iter().next()
}

/// "nvme0" → "NVMe 0".
pub fn nvme_label(id: &str) -> String {
    match id.strip_prefix("nvme") {
        Some(n) => format!("NVMe {n}"),
        None => id.to_string(),
    }
}

fn discover(root: &Path) -> Vec<Source> {
    let mut out: Vec<Source> = Vec::new();
    let mut dirs: Vec<PathBuf> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    dirs.sort();
    for dir in dirs {
        let Some(name) = read_trim(&dir.join("name")) else {
            continue;
        };
        let src = match name.as_str() {
            "k10temp" | "coretemp" if !out.iter().any(|s| s.kind == Kind::Cpu) => {
                let label = if name == "k10temp" {
                    "Tctl"
                } else {
                    "Package id 0"
                };
                pick(&dir, label).map(|input| Source {
                    id: "cpu".into(),
                    label: "CPU".into(),
                    input,
                    kind: Kind::Cpu,
                })
            }
            "nvme" => {
                let id = device_basename(&dir).unwrap_or_else(|| "nvme".into());
                pick(&dir, "Composite").map(|input| Source {
                    label: nvme_label(&id),
                    id,
                    input,
                    kind: Kind::Nvme,
                })
            }
            "drivetemp" => {
                let disk = first_entry(&dir.join("device/block"));
                match (disk, pick(&dir, "")) {
                    (Some(disk), Some(input)) => Some(Source {
                        id: disk.clone(),
                        label: format!("Disk {disk}"),
                        input,
                        kind: Kind::Drive(disk),
                    }),
                    _ => None,
                }
            }
            "atlantic" | "aqc" | "r8169" | "ixgbe" | "igb" => {
                let iface = first_entry(&dir.join("device/net"));
                match (iface, pick(&dir, "")) {
                    (Some(i), Some(input)) => Some(Source {
                        id: format!("nic:{i}"),
                        label: format!("NIC {i}"),
                        input,
                        kind: Kind::Nic,
                    }),
                    _ => None,
                }
            }
            _ => None,
        };
        out.extend(src);
    }
    out
}

fn rank(t: &Temperature) -> u8 {
    if t.id == "cpu" {
        0
    } else if t.id.starts_with("nvme") {
        1
    } else if t.id.starts_with("bay") {
        2
    } else if t.id.starts_with("nic:") {
        4
    } else {
        3
    }
}

impl Temps {
    pub fn new(hwmon_root: impl Into<PathBuf>) -> Self {
        Self {
            hwmon_root: hwmon_root.into(),
            sources: Vec::new(),
            discovered: None,
            drive_cache: HashMap::new(),
        }
    }

    /// One reading of every sensor. `bays` come from the hardware provider; `disk_active`
    /// says whether a disk did I/O recently (from the diskstats deltas).
    pub fn read(
        &mut self,
        now: Instant,
        bays: &[Bay],
        disk_active: impl Fn(&str) -> bool,
    ) -> Vec<Temperature> {
        if self
            .discovered
            .is_none_or(|t| now.duration_since(t) > REDISCOVER)
        {
            self.sources = discover(&self.hwmon_root);
            self.discovered = Some(now);
        }
        let mut out = Vec::with_capacity(self.sources.len() + bays.len());
        for b in bays.iter().filter(|b| b.present) {
            out.push(Temperature {
                id: format!("bay{}", b.number),
                label: format!("Bay {}", b.number),
                celsius: b.celsius,
            });
        }
        for s in &self.sources {
            let celsius = match &s.kind {
                Kind::Drive(disk) => {
                    if bays
                        .iter()
                        .any(|b| b.device.as_deref() == Some(disk.as_str()))
                    {
                        continue; // shown as its bay
                    }
                    let cached = self.drive_cache.get(disk).copied();
                    let due = cached.is_none_or(|(_, t)| now.duration_since(t) >= DRIVE_PERIOD);
                    if due && disk_active(disk) {
                        let v = read_celsius(&s.input);
                        self.drive_cache.insert(disk.clone(), (v, now));
                        v
                    } else {
                        cached.and_then(|(v, _)| v)
                    }
                }
                _ => read_celsius(&s.input),
            };
            out.push(Temperature {
                id: s.id.clone(),
                label: s.label.clone(),
                celsius,
            });
        }
        out.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.id.cmp(&b.id)));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_order() {
        assert_eq!(nvme_label("nvme0"), "NVMe 0");
        let t = |id: &str| Temperature {
            id: id.into(),
            ..Default::default()
        };
        let mut v = [
            t("nic:eth0"),
            t("sda"),
            t("bay2"),
            t("nvme0"),
            t("cpu"),
            t("bay1"),
        ];
        v.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.id.cmp(&b.id)));
        let ids: Vec<_> = v.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["cpu", "nvme0", "bay1", "bay2", "sda", "nic:eth0"]);
    }
}
