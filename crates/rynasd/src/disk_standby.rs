//! Software HDD standby watcher (`hd-idle` semantics).
//!
//! Many SATA HDDs with Extended Power Conditions (e.g. Seagate SkyHawk/IronWolf) ignore
//! the legacy ATA standby timer (`hdparm -S`), and periodic temperature polling (`drivetemp`
//! in `synohwd`) resets the drive's internal idle timer before it expires. Watching
//! `/proc/diskstats` and issuing ATA `STANDBY IMMEDIATE` (`hdparm -y`) once a rotational
//! ATA disk has had no block I/O for its configured standby timeout spins those drives down
//! reliably without waking drives that are already in standby.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rynas_api_types::storage::{PowerState, StorageTuning};
use rynas_storage::domain::{TUNING_KEY, parse_tuning};
use rynas_storage::host::{ata_check_power_mode, ata_standby_now};

use crate::ops::Ops;

const TICK: Duration = Duration::from_secs(30);
const SYNOLOGY_KEY: &str = "synology.hw";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiskSample {
    major: u32,
    minor: u32,
    reads: u64,
    writes: u64,
}

/// Whole `sd[a-z]+` devices from `/proc/diskstats` with `(reads_completed, writes_completed)`.
fn parse_diskstats(text: &str) -> BTreeMap<String, DiskSample> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(maj), Some(min), Some(name), Some(reads)) =
            (it.next(), it.next(), it.next(), it.next())
        else {
            continue;
        };
        if !is_whole_sd(name) {
            continue;
        }
        // fields after name: 1=reads, 2=reads_merged, 3=sectors_read, 4=ms_read, 5=writes
        let Some(writes) = it.nth(3) else { continue };
        if let (Ok(major), Ok(minor), Ok(reads), Ok(writes)) = (
            maj.parse::<u32>(),
            min.parse::<u32>(),
            reads.parse::<u64>(),
            writes.parse::<u64>(),
        ) {
            out.insert(name.to_owned(), DiskSample { major, minor, reads, writes });
        }
    }
    out
}

fn is_whole_sd(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("sd") else {
        return false;
    };
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_lowercase())
}

fn is_rotational_ata(dev: &str) -> bool {
    let rota = fs::read_to_string(format!("/sys/block/{dev}/queue/rotational"))
        .map(|s| s.trim() == "1")
        .unwrap_or(false);
    if !rota {
        return false;
    }
    // exclude USB flash/DOMs that report rotational=1 (e.g. Synology boot DOM on sde)
    fs::canonicalize(format!("/sys/block/{dev}/device"))
        .map(|p| p.to_string_lossy().contains("/ata"))
        .unwrap_or(false)
}

fn udev_serial(major: u32, minor: u32) -> Option<String> {
    let text = fs::read_to_string(format!("/run/udev/data/b{major}:{minor}")).ok()?;
    serial_from_udev(&text)
}

fn serial_from_udev(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix("E:ID_SERIAL_SHORT="))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn resolve_timeout(
    serial: Option<&str>,
    tuning: &StorageTuning,
    default_s: Option<u32>,
) -> Option<Duration> {
    if let Some(ser) = serial
        && let Some(ov) = tuning.disks.iter().find(|d| d.serial == ser)
        && let Some(s) = ov.settings.standby_s
    {
        return (s > 0).then(|| Duration::from_secs(s as u64));
    }
    default_s.filter(|&s| s > 0).map(|s| Duration::from_secs(s as u64))
}

#[derive(Debug, Default)]
struct Tracker {
    disks: HashMap<String, (u64, u64, Instant)>,
}

impl Tracker {
    /// Updates counters and returns `(dev, idle_duration)` for disks whose idle time has
    /// reached their configured timeout.
    fn due<F>(&mut self, samples: &BTreeMap<String, DiskSample>, now: Instant, mut timeout_for: F) -> Vec<(String, Duration)>
    where
        F: FnMut(&str, DiskSample) -> Option<Duration>,
    {
        self.disks.retain(|k, _| samples.contains_key(k));
        let mut ready = Vec::new();
        for (dev, &s) in samples {
            let entry = self.disks.entry(dev.clone()).or_insert((s.reads, s.writes, now));
            if entry.0 != s.reads || entry.1 != s.writes {
                *entry = (s.reads, s.writes, now);
                continue;
            }
            let Some(timeout) = timeout_for(dev, s) else {
                continue;
            };
            let idle = now.saturating_duration_since(entry.2);
            if idle >= timeout {
                // reset timer whether spin-down succeeds or is ignored, so a drive that
                // refuses standby is not hammered on every tick
                entry.2 = now;
                ready.push((dev.clone(), idle));
            }
        }
        ready
    }
}

fn read_config(ops: &Ops) -> (StorageTuning, Option<u32>) {
    let tuning = ops
        .config
        .current(TUNING_KEY)
        .ok()
        .flatten()
        .and_then(|v| parse_tuning(Some(&v)).ok())
        .unwrap_or_default();
    let default_s = ops
        .config
        .current(SYNOLOGY_KEY)
        .ok()
        .flatten()
        .and_then(|v| v.get("disk_standby_minutes")?.as_u64())
        .filter(|&m| m > 0)
        .and_then(|m| u32::try_from(m.saturating_mul(60)).ok());
    (tuning, default_s)
}

/// Spins down idle rotational ATA disks according to `storage.disks` and `synology.hw`.
pub async fn watch(ops: Arc<Ops>) {
    let mut tracker = Tracker::default();
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let Ok(text) = fs::read_to_string("/proc/diskstats") else {
            continue;
        };
        let samples = parse_diskstats(&text);
        if samples.is_empty() {
            continue;
        }
        let (tuning, default_s) = read_config(&ops);
        let due = tracker.due(&samples, Instant::now(), |dev, s| {
            if !is_rotational_ata(dev) {
                return None;
            }
            let serial = udev_serial(s.major, s.minor);
            resolve_timeout(serial.as_deref(), &tuning, default_s)
        });
        for (dev, idle) in due {
            // shortcut: does not check for an in-progress SMART self-test (reading smartctl
            // would reset or wake drives); upgrade if SMART tests are scheduled during idle
            let _ = tokio::task::spawn_blocking(move || {
                let path = format!("/dev/{dev}");
                match ata_check_power_mode(&path) {
                    Some(PowerState::Standby) => {}
                    Some(PowerState::Active | PowerState::Idle) => match ata_standby_now(&path) {
                        Ok(()) => tracing::info!(
                            "disk standby: spun down {path} after {}s idle",
                            idle.as_secs()
                        ),
                        Err(e) => tracing::debug!("disk standby: {path}: {e}"),
                    },
                    _ => {}
                }
            })
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::storage::{DiskOverride, DiskSettings};

    #[test]
    fn parses_whole_sd_devices_only() {
        let text = "\
   8       0 sda 158 0 4579 611 29414 14600 178354204 1150550 0 380844 1202217 0 0 0 0 4568 51054
   8       1 sda1 10 0 20 1 30 0 40 2 0 0 0
   9     126 md126 427 0 13584 5064 41664 0 178340632 21206328 0 677096 21211392
 259       0 nvme0n1 100 0 200 1 300 0 400 2 0 0 0
";
        let m = parse_diskstats(text);
        assert_eq!(m.len(), 1);
        assert_eq!(
            m["sda"],
            DiskSample { major: 8, minor: 0, reads: 158, writes: 29414 }
        );
    }

    #[test]
    fn per_disk_override_beats_global_default() {
        let tuning = StorageTuning {
            disks: vec![
                DiskOverride {
                    serial: "SER_OFF".into(),
                    settings: DiskSettings { standby_s: Some(0), ..Default::default() },
                },
                DiskOverride {
                    serial: "SER_CUSTOM".into(),
                    settings: DiskSettings { standby_s: Some(900), ..Default::default() },
                },
            ],
            ..Default::default()
        };
        assert_eq!(resolve_timeout(Some("SER_OFF"), &tuning, Some(300)), None);
        assert_eq!(
            resolve_timeout(Some("SER_CUSTOM"), &tuning, Some(300)),
            Some(Duration::from_secs(900))
        );
        assert_eq!(
            resolve_timeout(Some("SER_OTHER"), &tuning, Some(300)),
            Some(Duration::from_secs(300))
        );
        assert_eq!(resolve_timeout(None, &tuning, None), None);
    }

    #[test]
    fn tracker_fires_after_idle_and_resets_on_io() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let mut s = BTreeMap::new();
        s.insert("sda".into(), DiskSample { major: 8, minor: 0, reads: 10, writes: 20 });

        assert!(t.due(&s, t0, |_, _| Some(Duration::from_secs(300))).is_empty());
        assert!(t.due(&s, t0 + Duration::from_secs(299), |_, _| Some(Duration::from_secs(300))).is_empty());

        // I/O resets the idle timer
        s.get_mut("sda").unwrap().reads = 11;
        assert!(t.due(&s, t0 + Duration::from_secs(300), |_, _| Some(Duration::from_secs(300))).is_empty());

        // 300s after the I/O it becomes due once, then resets for cooldown
        let due = t.due(&s, t0 + Duration::from_secs(600), |_, _| Some(Duration::from_secs(300)));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, "sda");
        assert!(t.due(&s, t0 + Duration::from_secs(630), |_, _| Some(Duration::from_secs(300))).is_empty());
    }

    #[test]
    fn serial_from_udev_extracts_short_serial() {
        let udev = "S:disk/by-id/ata-ST6000VX001\nE:ID_SERIAL_SHORT=ZCT12345\n";
        assert_eq!(serial_from_udev(udev).as_deref(), Some("ZCT12345"));
    }
}
