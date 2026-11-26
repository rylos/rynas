//! Software HDD standby watcher (`hd-idle` semantics).
//!
//! Many SATA HDDs with Extended Power Conditions (e.g. Seagate SkyHawk/IronWolf) ignore
//! the legacy ATA standby timer (`hdparm -S`), and periodic temperature polling (`drivetemp`
//! in `synohwd`) resets the drive's internal idle timer before it expires. Watching
//! `/proc/diskstats` and issuing ATA `STANDBY IMMEDIATE` (`hdparm -y`) once a rotational
//! ATA disk has had no block I/O for its configured standby timeout spins those drives down
//! reliably without waking drives that are already in standby.
//!
//! The delay of a disk is its `standby_s` in the storage settings (type default, then the
//! disk's own override by serial, as the udev rules resolve it; 0: never), else the default
//! of a hardware module ([`rynas_module::Module::disk_standby_s`]). A disk running a SMART
//! self-test is left spinning: STANDBY IMMEDIATE aborts the test, and a long test makes no
//! block I/O. sysfs, udev and `/proc` are read below the hardware root (`--hw-root`).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rynas_api_types::storage::{DiskKind, PowerState, StorageTuning};
use rynas_hw::under;
use rynas_module::Modules;
use rynas_storage::domain::{TUNING_KEY, effective, parse_tuning};
use rynas_storage::host::{ata_check_power_mode, ata_standby_now};
use rynas_tasks::Cmd;

use crate::ops::Ops;

const TICK: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------------------------
// "are the disks awake?" for work that can wait (docs/phase1/tasks.md, "A NAS at rest")
// ---------------------------------------------------------------------------------------------

/// Bumped by the watcher's turn (every [`TICK`]) when a disk did block I/O since the last
/// turn: work deferred while its disks slept looks again then (piggyback on the first I/O).
static ACTIVITY: std::sync::LazyLock<tokio::sync::watch::Sender<u64>> = std::sync::LazyLock::new(|| tokio::sync::watch::channel(0).0);

/// Whether a disk under the filesystem mounted at a path sleeps (set once at start: the
/// storage service's `mount_asleep`, which reads sysfs, mountinfo and the ATA power mode and
/// never wakes a disk).
type SleepProbe = Box<dyn Fn(&str) -> bool + Send + Sync>;
static SLEEP_PROBE: std::sync::OnceLock<SleepProbe> = std::sync::OnceLock::new();

/// Sets how [`disks_awake`] asks (once, at start).
pub fn set_sleep_probe(f: SleepProbe) {
    let _ = SLEEP_PROBE.set(f);
}

/// The disks of the volume mounted at `mount` are all spinning: reading it now wakes nothing.
/// `true` when it cannot be told (no probe, an unknown mount, no rotational disk). Blocking
/// (sysfs, ioctls): call it from a blocking thread.
pub fn disks_awake(mount: &str) -> bool {
    SLEEP_PROBE.get().is_none_or(|f| !f(mount))
}

/// [`disks_awake`] of the filesystem holding `path` (the longest mount point containing it).
pub fn path_awake(path: &str) -> bool {
    mount_of(path).is_none_or(|m| disks_awake(&m))
}

/// The mount point of the filesystem holding `path` (mountinfo; `None`: unknown).
pub fn mount_of(path: &str) -> Option<String> {
    let mounts = rynas_storage::mounts::parse_mountinfo(&crate::places::mountinfo());
    mounts.into_iter().filter(|m| rynas_services::folders::is_within(path, &m.mountpoint)).max_by_key(|m| m.mountpoint.len()).map(|m| m.mountpoint)
}

/// What [`when_awake`] waited for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Woke {
    /// The disks were awake (at once, or after other work woke them).
    Awake,
    /// The deadline passed with the disks still asleep: run anyway if it must.
    Deadline,
}

/// Waits until the disks of the volume at `mount` are awake, or until `deadline`. No loop of
/// its own: it checks at once, then after each turn of the standby watcher that saw block I/O
/// (the first read of anyone else spins the disks up; the job rides on that).
pub async fn when_awake(mount: &str, deadline: tokio::time::Instant) -> Woke {
    let mut rx = ACTIVITY.subscribe();
    loop {
        let m = mount.to_string();
        if tokio::task::spawn_blocking(move || disks_awake(&m)).await.unwrap_or(true) {
            return Woke::Awake;
        }
        tokio::select! {
            // the sender is a static: it never closes (without a watcher nothing is sent and
            // only the deadline ends the wait)
            _ = rx.changed() => {}
            _ = tokio::time::sleep_until(deadline) => return Woke::Deadline,
        }
    }
}

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

/// Whether `dev` is a rotational disk behind an ATA port (sysfs below `hw_root`): USB flash
/// drives and boot DOMs that report `rotational` 1 are left alone, and so are SSDs.
fn is_rotational_ata(hw_root: &Path, dev: &str) -> bool {
    let rota = fs::read_to_string(under(hw_root, &format!("/sys/block/{dev}/queue/rotational")))
        .map(|s| s.trim() == "1")
        .unwrap_or(false);
    if !rota {
        return false;
    }
    let Ok(real) = fs::canonicalize(under(hw_root, &format!("/sys/block/{dev}/device"))) else {
        return false;
    };
    let base = fs::canonicalize(hw_root).unwrap_or_else(|_| hw_root.to_path_buf());
    // the device's path goes through its port: `/sys/devices/pci…/ata1/host0/target0:0:0/…`
    real.strip_prefix(&base).unwrap_or(&real).components().any(|c| {
        c.as_os_str()
            .to_str()
            .and_then(|s| s.strip_prefix("ata"))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

fn udev_serial(hw_root: &Path, major: u32, minor: u32) -> Option<String> {
    let text = fs::read_to_string(under(hw_root, &format!("/run/udev/data/b{major}:{minor}"))).ok()?;
    serial_from_udev(&text)
}

fn serial_from_udev(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix("E:ID_SERIAL_SHORT="))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The spin-down delay of a rotational disk: its `standby_s` as the udev rules resolve it
/// (type default, then its own override), else the module's default; 0 is never.
fn resolve_timeout(
    serial: Option<&str>,
    tuning: &StorageTuning,
    module_s: Option<u32>,
) -> Option<Duration> {
    let own = serial
        .and_then(|ser| tuning.disks.iter().find(|d| d.serial == ser))
        .map(|d| &d.settings);
    let s = effective(DiskKind::Hdd, own).standby_s.or(module_s)?;
    (s > 0).then(|| Duration::from_secs(u64::from(s)))
}

#[derive(Debug, Default)]
struct Tracker {
    disks: HashMap<String, (u64, u64, Instant)>,
    /// Reads and writes of every disk at the last turn.
    io_total: Option<u64>,
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

/// The disks due for a spin-down at `now`: rotational ATA disks idle for their delay.
fn due_disks(
    hw_root: &Path,
    tracker: &mut Tracker,
    samples: &BTreeMap<String, DiskSample>,
    now: Instant,
    tuning: &StorageTuning,
    module_s: Option<u32>,
) -> Vec<(String, Duration)> {
    tracker.due(samples, now, |dev, s| {
        if !is_rotational_ata(hw_root, dev) {
            return None;
        }
        let serial = udev_serial(hw_root, s.major, s.minor);
        resolve_timeout(serial.as_deref(), tuning, module_s)
    })
}

/// The disks whose long SMART test, started by the scheduler, still runs in the drive.
fn held_disks() -> Vec<String> {
    crate::gate::running()
        .map(|g| g.disk_tests().into_iter().map(|d| d.disk).collect())
        .unwrap_or_default()
}

/// Whether a SMART self-test runs on `dev`: one rynas started and still follows, else what
/// the drive says (`ask`, only asked when rynas does not know of one).
fn self_test_running(dev: &str, held: &[String], ask: impl FnOnce() -> Option<bool>) -> bool {
    held.iter().any(|d| d == dev) || ask() == Some(true)
}

/// The drive's self-test execution status (`smartctl -n standby -c`: never spins it up).
fn drive_self_test(ops: &Ops, path: &str) -> Option<bool> {
    let cmd = Cmd::new("smartctl").args(["-n", "standby", "-c", path]).env_clear();
    match ops.config.system().output(&cmd) {
        // smartctl's exit code is a bit mask of disk states: read the output whatever it is
        Ok(o) => crate::claims::parse_self_test(&o.stdout).in_progress,
        Err(e) => {
            tracing::debug!("disk standby: smartctl -c {path}: {e}");
            None
        }
    }
}

/// Spins `dev` down when it is awake and runs no SMART self-test.
fn spin_down(ops: &Ops, held: &[String], dev: &str, idle: Duration) {
    let path = format!("/dev/{dev}");
    if !matches!(ata_check_power_mode(&path), Some(PowerState::Active | PowerState::Idle)) {
        return;
    }
    if self_test_running(dev, held, || drive_self_test(ops, &path)) {
        tracing::debug!("disk standby: {path}: a SMART self-test runs, left spinning");
        return;
    }
    match ata_standby_now(&path) {
        Ok(()) => tracing::info!("disk standby: spun down {path} after {}s idle", idle.as_secs()),
        Err(e) => tracing::debug!("disk standby: {path}: {e}"),
    }
}

/// One turn of the watcher (blocking: sysfs, the configuration store, ioctls, smartctl).
fn tick(ops: &Ops, modules: &Modules, hw_root: &Path, tracker: &mut Tracker, now: Instant) {
    let Ok(text) = fs::read_to_string(under(hw_root, "/proc/diskstats")) else {
        return;
    };
    let samples = parse_diskstats(&text);
    if samples.is_empty() {
        return;
    }
    // block I/O on a spinning disk since the last turn: deferred work looks again (SSDs and
    // flash never sleep here, their I/O says nothing)
    let io: u64 = samples
        .iter()
        .filter(|(dev, _)| is_rotational_ata(hw_root, dev))
        .map(|(_, s)| s.reads.wrapping_add(s.writes))
        .fold(0, u64::wrapping_add);
    if tracker.io_total.replace(io).is_some_and(|before| before != io) {
        ACTIVITY.send_modify(|n| *n = n.wrapping_add(1));
    }
    // applied values only (a database read): nothing is imported from files on every turn
    let tuning = ops
        .config
        .applied(TUNING_KEY)
        .ok()
        .flatten()
        .and_then(|v| parse_tuning(Some(&v)).ok())
        .unwrap_or_default();
    let module_s = modules.disk_standby_s(&ops.config);
    let due = due_disks(hw_root, tracker, &samples, now, &tuning, module_s);
    if due.is_empty() {
        return;
    }
    let held = held_disks();
    for (dev, idle) in due {
        spin_down(ops, &held, &dev, idle);
    }
}

/// Spins down idle rotational ATA disks after their spin-down delay (storage settings, else
/// the hardware module's default).
pub async fn watch(ops: Arc<Ops>, modules: Modules, hw_root: PathBuf) {
    let mut tracker = Tracker::default();
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let (o, m, h) = (ops.clone(), modules.clone(), hw_root.clone());
        let mut t = std::mem::take(&mut tracker);
        match tokio::task::spawn_blocking(move || {
            tick(&o, &m, &h, &mut t, Instant::now());
            t
        })
        .await
        {
            Ok(t) => tracker = t,
            Err(e) => tracing::debug!("disk standby: {e}"),
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

    /// A fake `/sys` + `/run/udev` + `/proc` tree: `sda` a rotational disk on an ATA port,
    /// `sdb` an SSD on an ATA port, `sdc` a USB flash drive that says it is rotational.
    fn fake_tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        let disk = |name: &str, rota: &str, port: &str, serial: &str, minor: u32| {
            let dev = r.join(format!("sys/devices/pci0000:00/0000:00:17.0/{port}/0:0:0:{minor}"));
            fs::create_dir_all(&dev).unwrap();
            let blk = r.join(format!("sys/block/{name}"));
            fs::create_dir_all(blk.join("queue")).unwrap();
            fs::write(blk.join("queue/rotational"), format!("{rota}\n")).unwrap();
            std::os::unix::fs::symlink(&dev, blk.join("device")).unwrap();
            fs::create_dir_all(r.join("run/udev/data")).unwrap();
            fs::write(r.join(format!("run/udev/data/b8:{minor}")), format!("E:ID_SERIAL_SHORT={serial}\n")).unwrap();
        };
        disk("sda", "1", "ata1/host0/target0:0:0", "HDD1", 0);
        disk("sdb", "0", "ata2/host1/target1:0:0", "SSD1", 16);
        disk("sdc", "1", "usb1/1-1/1-1:1.0/host6/target6:0:0", "USB1", 32);
        fs::create_dir_all(r.join("proc")).unwrap();
        fs::write(
            r.join("proc/diskstats"),
            "   8       0 sda 10 0 0 0 20 0 0 0 0 0 0\n   8      16 sdb 10 0 0 0 20 0 0 0 0 0 0\n   8      32 sdc 10 0 0 0 20 0 0 0 0 0 0\n",
        )
        .unwrap();
        d
    }

    #[test]
    fn only_rotational_ata_disks_in_a_fake_tree() {
        let d = fake_tree();
        let r = d.path();
        assert!(is_rotational_ata(r, "sda"));
        assert!(!is_rotational_ata(r, "sdb"), "SSD");
        assert!(!is_rotational_ata(r, "sdc"), "USB flash that says rotational");
        assert!(!is_rotational_ata(r, "sdz"), "no such disk");
        assert_eq!(udev_serial(r, 8, 0).as_deref(), Some("HDD1"));

        let samples = parse_diskstats(&fs::read_to_string(r.join("proc/diskstats")).unwrap());
        assert_eq!(samples.len(), 3);
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let tuning = StorageTuning::default();
        assert!(due_disks(r, &mut t, &samples, t0, &tuning, Some(600)).is_empty());
        // the module's default: only the rotational ATA disk is due
        let due = due_disks(r, &mut t, &samples, t0 + Duration::from_secs(600), &tuning, Some(600));
        assert_eq!(due.iter().map(|(d, _)| d.as_str()).collect::<Vec<_>>(), ["sda"]);
        // its own override wins, 0 is never
        let off = StorageTuning {
            disks: vec![DiskOverride {
                serial: "HDD1".into(),
                settings: DiskSettings { standby_s: Some(0), ..Default::default() },
            }],
            ..Default::default()
        };
        assert!(due_disks(r, &mut t, &samples, t0 + Duration::from_secs(1800), &off, Some(600)).is_empty());
        // no delay anywhere: nothing
        assert!(due_disks(r, &mut t, &samples, t0 + Duration::from_secs(3600), &tuning, None).is_empty());
    }

    #[test]
    fn a_running_self_test_keeps_the_disk_spinning() {
        let held = vec!["sdb".to_string()];
        // a long test rynas follows: the drive is not even asked
        assert!(self_test_running("sdb", &held, || panic!("asked the drive")));
        assert!(self_test_running("sda", &held, || Some(true)));
        assert!(!self_test_running("sda", &held, || Some(false)));
        assert!(!self_test_running("sda", &held, || None), "unknown: spun down");
        // what the drive says, as `smartctl -n standby -c` prints it
        let out = "Self-test execution status:      ( 249)\tSelf-test routine in progress...\n\t\t\t\t\t90% of test remaining.\n";
        assert_eq!(crate::claims::parse_self_test(out).in_progress, Some(true));
        let out = "Self-test execution status:      (   0)\tThe previous self-test routine completed\n";
        assert_eq!(crate::claims::parse_self_test(out).in_progress, Some(false));
    }
}
