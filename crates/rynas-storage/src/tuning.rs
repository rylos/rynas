//! Per-disk-type recommended settings compared with the current values
//! (`docs/research/system-reliability-tweaks.md` §6, decision D-SYSTUNE). The per-disk
//! overrides themselves are the `storage.disks` domain ([`crate::domain`]).
//!
//! Every check carries codes next to its English texts (`note_code`, `current_value`,
//! `recommended_value`) so the interface can translate them; literal values (scheduler
//! names, KiB, ALPM policies) have no code and are shown as they are.

use rynas_api_types::storage::{CheckStatus, CheckValue, DiskKind, SettingCheck};

use crate::host::Host;
use crate::smart::temp_limits;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TuningInput {
    pub kind: DiskKind,
    /// Active scheduler (the bracketed one).
    pub scheduler: Option<String>,
    pub read_ahead_kb: Option<u32>,
    pub write_cache: Option<bool>,
    /// APM level; Some(None) = supported but disabled.
    pub apm: Option<Option<u32>>,
    /// The device accepts discards (`queue/discard_max_bytes` > 0).
    pub discard_supported: bool,
    /// `fstrim.timer` enabled.
    pub fstrim_timer: Option<bool>,
    /// A filesystem on this disk is mounted with online discard (`discard`/`discard=async`).
    pub online_discard: bool,
    /// The disk is a member of a RAID5/6 array (md drops discards there by default).
    pub under_parity_raid: bool,
    /// SATA link power policy of the disk's host.
    pub alpm: Option<String>,
    /// `nvme_core.default_ps_max_latency_us`; 0 disables APST.
    pub apst_latency_us: Option<u64>,
    pub temperature_c: Option<f32>,
    pub transport: String,
    /// SMART was read from the drive: an absent APM then means the drive has none.
    pub smart_read: bool,
    /// The disk is an md member or holds btrfs: both do their own read-ahead.
    pub stacked: bool,
    /// An encrypted mapping above the disk does not pass discards (crypttab without
    /// `discard`): TRIM cannot reach the device.
    pub crypt_blocks_discard: bool,
    /// (warning, critical) limits the drive reports itself (NVMe WCTEMP/CCTEMP, drivetemp
    /// SCT limits); the type's table otherwise, or when the table is lower.
    pub drive_temp_limits: Option<(f32, f32)>,
    /// SMART 193 Load_Cycle_Count and 9 Power_On_Hours (HDD head parking).
    pub load_cycles: Option<u64>,
    pub power_on_hours: Option<u64>,
}

/// Head load cycles per power-on hour above which a drive parks its heads too often: the
/// rated 600,000 cycles over five years are about 13 per hour.
pub const PARKING_PER_HOUR: f64 = 10.0;
/// Load cycles above which head parking is worth a look whatever the rate.
pub const PARKING_TOTAL: u64 = 300_000;

/// (warning, critical) temperature limits of a disk: the type's table, lowered to the
/// drive's own limits when it reports lower ones (NVMe: the drive's limits replace the
/// table, which is only the fallback).
pub fn temp_limits_for(kind: DiskKind, drive: Option<(f32, f32)>) -> (f32, f32) {
    let table = temp_limits(kind);
    match drive {
        Some(d) if kind == DiskKind::Nvme => d,
        Some((w, c)) => (w.min(table.0), c.min(table.1)),
        None => table,
    }
}

/// The bracketed entry of `queue/scheduler`: "[mq-deadline] kyber bfq none".
pub fn active_scheduler(s: &str) -> Option<String> {
    s.split_whitespace()
        .find_map(|w| w.strip_prefix('[')?.strip_suffix(']'))
        .map(str::to_string)
        .or_else(|| {
            let t = s.trim();
            (!t.is_empty() && !t.contains(' ')).then(|| t.to_string())
        })
}

/// A value of a check: its English text and, when it is not a literal, its code.
struct Val {
    text: String,
    value: Option<CheckValue>,
}

/// A literal value (shown as it is in every language).
fn lit(text: impl Into<String>) -> Val {
    Val {
        text: text.into(),
        value: None,
    }
}

/// A translatable value.
fn coded(text: impl Into<String>, value: CheckValue) -> Val {
    Val {
        text: text.into(),
        value: Some(value),
    }
}

fn on_off(on: bool) -> Val {
    if on {
        coded("on", CheckValue::code("on"))
    } else {
        coded("off", CheckValue::code("off"))
    }
}

/// The note of a check: its code and its English text.
type Note = (&'static str, &'static str);

fn check(
    key: &str,
    label: &str,
    current: Option<Val>,
    recommended: Val,
    status: CheckStatus,
    note: Option<Note>,
) -> SettingCheck {
    let (current, current_value) = match current {
        Some(v) => (Some(v.text), v.value),
        None => (None, None),
    };
    SettingCheck {
        key: key.into(),
        label: label.into(),
        current,
        recommended: recommended.text,
        status,
        note: note.map(|n| n.1).unwrap_or_default().into(),
        current_value,
        recommended_value: recommended.value,
        note_code: note.map(|n| n.0.to_string()),
    }
}

fn ok_if(cond: Option<bool>) -> CheckStatus {
    match cond {
        Some(true) => CheckStatus::Ok,
        Some(false) => CheckStatus::Differs,
        None => CheckStatus::Unknown,
    }
}

pub fn checks(i: &TuningInput) -> Vec<SettingCheck> {
    use DiskKind::*;
    let mut v = Vec::new();
    let flash = matches!(i.kind, Ssd | Nvme);

    let (want_sched, sched_ok): (&str, &[&str]) = match i.kind {
        Nvme => ("none", &["none"]),
        // a single NCQ queue: the difference between the two is noise
        Ssd => ("mq-deadline / none", &["mq-deadline", "none"]),
        _ => ("mq-deadline", &["mq-deadline"]),
    };
    v.push(check(
        "scheduler",
        "I/O scheduler",
        i.scheduler.clone().map(lit),
        lit(want_sched),
        ok_if(i.scheduler.as_ref().map(|s| sched_ok.contains(&s.as_str()))),
        Some(match i.kind {
            Nvme => (
                "sched_nvme",
                "NVMe has deep hardware queues: no scheduler is fastest",
            ),
            Hdd => (
                "sched_hdd",
                "mq-deadline keeps throughput on RAID; bfq only helps desktop interactivity",
            ),
            _ => (
                "sched_flash",
                "a single queue: mq-deadline and none perform the same",
            ),
        }),
    ));

    // the kernel's default is right (6.17+ sizes it from the device); btrfs and md read
    // ahead on their own device, so the disk's value does not matter under them
    let (ra_lo, ra_hi) = match i.kind {
        Hdd | Unknown => (128, 8192),
        Ssd | Nvme => (128, 4096),
    };
    let (ra_status, ra_note) = if i.stacked {
        (
            CheckStatus::NotApplicable,
            (
                "ra_stacked",
                "btrfs and md read ahead on their own device: the disk's value does not matter",
            ),
        )
    } else if i.kind == Hdd {
        (
            ok_if(i.read_ahead_kb.map(|kb| (ra_lo..=ra_hi).contains(&kb))),
            (
                "ra_hdd",
                "the kernel's default suits spinning disks; very large values only waste cache",
            ),
        )
    } else {
        (
            ok_if(i.read_ahead_kb.map(|kb| (ra_lo..=ra_hi).contains(&kb))),
            (
                "ra_flash",
                "flash needs little read-ahead; more only wastes cache",
            ),
        )
    };
    v.push(check(
        "read_ahead_kb",
        "Read-ahead (KiB)",
        i.read_ahead_kb.map(|k| lit(k.to_string())),
        coded(
            format!("kernel default ({ra_lo}-{ra_hi})"),
            CheckValue::with("ra_default", ra_hi as f64),
        ),
        ra_status,
        Some(ra_note),
    ));

    if i.kind != Nvme {
        v.push(check(
            "write_cache",
            "Write cache",
            i.write_cache.map(on_off),
            on_off(true),
            ok_if(i.write_cache),
            Some((
                "wcache",
                "leave it on: the filesystem's flushes keep it safe, off costs a lot of speed",
            )),
        ));
    }

    if i.kind == Hdd {
        let current = i.apm.map(|a| match a {
            Some(l) => lit(l.to_string()),
            None => coded("disabled", CheckValue::code("disabled")),
        });
        // 254 or off: no drive-initiated spin-down, no aggressive head unloading; spin-down,
        // when wanted, is the system's job (the standby override)
        let (status, note) = match i.apm {
            Some(Some(l)) if l <= 127 => (
                CheckStatus::Differs,
                (
                    "apm",
                    "levels up to 127 let the drive park its heads and spin down by itself",
                ),
            ),
            Some(_) => (
                CheckStatus::Ok,
                (
                    "apm",
                    "levels up to 127 let the drive park its heads and spin down by itself",
                ),
            ),
            None if i.smart_read => (
                CheckStatus::NotApplicable,
                ("apm_none", "the drive has no APM: nothing to set"),
            ),
            None => (
                CheckStatus::Unknown,
                (
                    "apm",
                    "levels up to 127 let the drive park its heads and spin down by itself",
                ),
            ),
        };
        v.push(check(
            "apm",
            "Power management (APM)",
            current,
            coded("254 or off", CheckValue::code("apm_254_off")),
            status,
            Some(note),
        ));

        // head parking: rated 600,000 load cycles, about 13 per power-on hour over 5 years
        if let Some(lc) = i.load_cycles {
            let rate = i
                .power_on_hours
                .filter(|&h| h > 0)
                .map(|h| lc as f64 / h as f64);
            let too_many = lc > PARKING_TOTAL || rate.is_some_and(|r| r > PARKING_PER_HOUR);
            let current = match rate {
                Some(r) => {
                    let r = (r * 10.0).round() / 10.0;
                    coded(
                        format!("{r} per hour ({lc} in total)"),
                        CheckValue::with("parking_rate", r),
                    )
                }
                None => lit(lc.to_string()),
            };
            v.push(check(
                "head_parking",
                "Head parking",
                Some(current),
                coded(
                    "at most 10 per hour",
                    CheckValue::with("parking_max", PARKING_PER_HOUR),
                ),
                if too_many {
                    CheckStatus::Differs
                } else {
                    CheckStatus::Ok
                },
                Some((
                    "parking",
                    "frequent head unloads wear the drive: raise APM or the drive's idle timers",
                )),
            ));
        }
    }

    if flash {
        let (cur, status, note): (Val, CheckStatus, Option<Note>) =
            match (i.fstrim_timer, i.online_discard) {
                (Some(true), true) => (
                    coded(
                        "fstrim.timer + discard mount",
                        CheckValue::code("trim_timer_discard"),
                    ),
                    CheckStatus::Ok,
                    Some((
                        "trim_both",
                        "async discard does the work, the weekly timer is a cheap safety net",
                    )),
                ),
                (Some(true), false) => (
                    coded("fstrim.timer", CheckValue::code("trim_timer")),
                    CheckStatus::Ok,
                    Some(("trim_timer", "weekly batched TRIM")),
                ),
                (_, true) => (
                    coded("discard mount option", CheckValue::code("trim_discard")),
                    CheckStatus::Ok,
                    Some((
                        "trim_discard",
                        "btrfs discard=async (default since 6.2) trims continuously",
                    )),
                ),
                (Some(false), false) => (
                    coded("none", CheckValue::code("trim_none")),
                    CheckStatus::Differs,
                    Some((
                        "trim_none",
                        "without TRIM the SSD slows down and wears faster: enable fstrim.timer",
                    )),
                ),
                (None, false) => (
                    coded("unknown", CheckValue::code("unknown")),
                    CheckStatus::Unknown,
                    None,
                ),
            };
        let (status, note) = if !i.discard_supported {
            (
                CheckStatus::NotApplicable,
                Some(("trim_unsupported", "the device does not accept discards")),
            )
        } else if i.under_parity_raid {
            (
                CheckStatus::NotApplicable,
                Some((
                    "trim_parity_raid",
                    "md RAID5/6 does not pass TRIM through (devices_handle_discard_safely is off)",
                )),
            )
        } else if i.crypt_blocks_discard {
            (
                CheckStatus::NotApplicable,
                Some((
                    "trim_crypt",
                    "the encrypted volume does not pass TRIM through: leave 10-20% of a new SSD unpartitioned",
                )),
            )
        } else {
            (status, note)
        };
        v.push(check(
            "trim",
            "TRIM",
            Some(cur),
            coded("fstrim.timer weekly", CheckValue::code("trim_weekly")),
            status,
            note,
        ));
    }

    if i.kind == Ssd && i.transport == "sata" {
        let want = "med_power_with_dipm";
        v.push(check(
            "alpm",
            "SATA link power (ALPM)",
            i.alpm.clone().map(lit),
            lit(want),
            ok_if(i.alpm.as_ref().map(|a| a == want)),
            Some(("alpm", "saves about 1 W per SSD with no measurable cost")),
        ));
    }

    if i.kind == Nvme {
        v.push(check(
            "apst",
            "NVMe power states (APST)",
            i.apst_latency_us.map(|l| {
                if l == 0 {
                    coded("disabled", CheckValue::code("disabled"))
                } else {
                    coded(
                        format!("enabled (max latency {l} µs)"),
                        CheckValue::with("apst_enabled", l as f64),
                    )
                }
            }),
            coded("enabled", CheckValue::code("enabled")),
            ok_if(i.apst_latency_us.map(|l| l > 0)),
            Some((
                "apst",
                "disable only for models with known APST firmware bugs",
            )),
        ));
    }

    let (warn, crit) = temp_limits_for(i.kind, i.drive_temp_limits);
    let temp_status = match i.temperature_c {
        Some(t) if t >= crit => CheckStatus::Critical,
        Some(t) if t >= warn => CheckStatus::Differs,
        Some(_) => CheckStatus::Ok,
        None => CheckStatus::Unknown,
    };
    v.push(check(
        "temperature",
        "Temperature",
        i.temperature_c
            .map(|t| coded(format!("{t:.0} °C"), CheckValue::with("temp", t.round() as f64))),
        coded(
            format!("below {warn:.0} °C"),
            CheckValue::with("temp_below", warn as f64),
        ),
        temp_status,
        Some(match i.kind {
            Hdd => (
                "temp_hdd",
                "failures rise with heat: keep hard disks below 50 °C, never above 60 °C",
            ),
            Nvme => (
                "temp_nvme",
                "the drive slows itself down above its warning temperature",
            ),
            _ => ("temp_other", "keep airflow over the drive"),
        }),
    ));
    v
}

/// Gather the inputs of [`checks`] that live in sysfs. SMART-derived fields (write cache,
/// APM, temperature) and the mount/RAID context are filled by the caller.
pub fn read_sysfs(host: &dyn Host, disk: &str, input: &mut TuningInput) {
    let q = format!("/sys/block/{disk}/queue");
    input.scheduler = host
        .read(&format!("{q}/scheduler"))
        .and_then(|s| active_scheduler(&s));
    input.read_ahead_kb = host
        .read(&format!("{q}/read_ahead_kb"))
        .and_then(|s| s.trim().parse().ok());
    input.discard_supported = host
        .read(&format!("{q}/discard_max_bytes"))
        .and_then(|s| s.trim().parse::<u64>().ok())
        .is_some_and(|n| n > 0);
    input.fstrim_timer = Some(
        host.read("/etc/systemd/system/timers.target.wants/fstrim.timer")
            .is_some(),
    );
    if let Some(p) = host.canonical(&format!("/sys/block/{disk}"))
        && let Some(h) = p
            .split('/')
            .find(|c| c.starts_with("host") && c[4..].parse::<u32>().is_ok())
    {
        input.alpm = host
            .read(&format!(
                "/sys/class/scsi_host/{h}/link_power_management_policy"
            ))
            .map(|s| s.trim().to_string());
    }
    input.apst_latency_us = host
        .read("/sys/module/nvme_core/parameters/default_ps_max_latency_us")
        .and_then(|s| s.trim().parse().ok());
    input.drive_temp_limits = drive_temp_limits(host, disk);
}

/// The drive's own temperature limits from its hwmon device: NVMe WCTEMP/CCTEMP
/// (`device/hwmonN`) or the drivetemp SCT limits of an ATA disk (`device/hwmon/hwmonN`),
/// `temp1_max`/`temp1_crit` in m°C.
pub fn drive_temp_limits(host: &dyn Host, disk: &str) -> Option<(f32, f32)> {
    let dev = format!("/sys/block/{disk}/device");
    let mut dirs: Vec<String> = host
        .list(&dev)
        .into_iter()
        .filter(|e| e.starts_with("hwmon"))
        .map(|e| format!("{dev}/{e}"))
        .collect();
    dirs.extend(
        host.list(&format!("{dev}/hwmon"))
            .into_iter()
            .map(|e| format!("{dev}/hwmon/{e}")),
    );
    dirs.sort();
    let milli = |p: &str| {
        host.read(p)
            .and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|&v| v > 0)
            .map(|v| v as f32 / 1000.0)
    };
    dirs.iter().find_map(|d| {
        let max = milli(&format!("{d}/temp1_max"))?;
        let crit = milli(&format!("{d}/temp1_crit")).unwrap_or(max + 10.0);
        Some((max, crit.max(max)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn by_key<'a>(v: &'a [SettingCheck], k: &str) -> &'a SettingCheck {
        v.iter().find(|c| c.key == k).unwrap()
    }

    #[test]
    fn scheduler_parse() {
        assert_eq!(
            active_scheduler("[mq-deadline] kyber bfq none\n").as_deref(),
            Some("mq-deadline")
        );
        assert_eq!(
            active_scheduler("mq-deadline kyber bfq [none]").as_deref(),
            Some("none")
        );
        assert_eq!(active_scheduler("none").as_deref(), Some("none"));
    }

    #[test]
    fn hdd_defaults() {
        let i = TuningInput {
            kind: DiskKind::Hdd,
            scheduler: Some("mq-deadline".into()),
            read_ahead_kb: Some(128),
            write_cache: Some(true),
            apm: Some(Some(254)),
            temperature_c: Some(38.0),
            transport: "sata".into(),
            ..Default::default()
        };
        let v = checks(&i);
        assert_eq!(by_key(&v, "scheduler").status, CheckStatus::Ok);
        // the kernel's default is fine
        assert_eq!(by_key(&v, "read_ahead_kb").status, CheckStatus::Ok);
        // 254: no drive-initiated spin-down, no head parking
        assert_eq!(by_key(&v, "apm").status, CheckStatus::Ok);
        assert_eq!(by_key(&v, "write_cache").status, CheckStatus::Ok);
        assert_eq!(by_key(&v, "temperature").status, CheckStatus::Ok);
        assert!(v.iter().all(|c| c.key != "trim" && c.key != "head_parking"));
    }

    #[test]
    fn hdd_research_rules() {
        let mut i = TuningInput {
            kind: DiskKind::Hdd,
            read_ahead_kb: Some(16384),
            apm: Some(Some(127)),
            temperature_c: Some(52.0),
            load_cycles: Some(50_000),
            power_on_hours: Some(2_000),
            ..Default::default()
        };
        let v = checks(&i);
        assert_eq!(by_key(&v, "read_ahead_kb").status, CheckStatus::Differs);
        assert_eq!(by_key(&v, "apm").status, CheckStatus::Differs);
        assert_eq!(by_key(&v, "temperature").status, CheckStatus::Differs);
        let p = by_key(&v, "head_parking");
        assert_eq!(p.status, CheckStatus::Differs);
        assert_eq!(p.current_value, Some(CheckValue::with("parking_rate", 25.0)));
        // under btrfs/md the disk's read-ahead does not matter; APM off is fine
        i.stacked = true;
        i.apm = Some(None);
        i.temperature_c = Some(61.0);
        i.load_cycles = Some(10_000);
        let v = checks(&i);
        assert_eq!(by_key(&v, "read_ahead_kb").status, CheckStatus::NotApplicable);
        assert_eq!(by_key(&v, "apm").status, CheckStatus::Ok);
        assert_eq!(by_key(&v, "temperature").status, CheckStatus::Critical);
        assert_eq!(by_key(&v, "head_parking").status, CheckStatus::Ok);
        // no APM at all on a drive whose SMART was read
        i.apm = None;
        i.smart_read = true;
        assert_eq!(by_key(&checks(&i), "apm").status, CheckStatus::NotApplicable);
    }

    #[test]
    fn nvme_temperature_from_the_drive() {
        let mut h = crate::host::FakeHost::default();
        h.file("/sys/block/nvme0n1/device/hwmon3/temp1_max", "84850\n")
            .file("/sys/block/nvme0n1/device/hwmon3/temp1_crit", "87850\n");
        let lim = drive_temp_limits(&h, "nvme0n1");
        assert_eq!(lim, Some((84.85, 87.85)));
        assert_eq!(temp_limits_for(DiskKind::Nvme, lim), (84.85, 87.85));
        assert_eq!(temp_limits_for(DiskKind::Nvme, None), (70.0, 80.0));
        // an ATA drive's own limits only lower the table
        assert_eq!(temp_limits_for(DiskKind::Hdd, Some((55.0, 58.0))), (50.0, 58.0));
    }

    #[test]
    fn checks_carry_codes_for_translation() {
        let i = TuningInput {
            kind: DiskKind::Hdd,
            scheduler: Some("bfq".into()),
            write_cache: Some(false),
            apm: Some(None),
            temperature_c: Some(41.4),
            ..Default::default()
        };
        let v = checks(&i);
        // every check has a translatable note
        assert!(v.iter().all(|c| c.note_code.is_some() && !c.note.is_empty()));
        let s = by_key(&v, "scheduler");
        assert_eq!(s.note_code.as_deref(), Some("sched_hdd"));
        // literal values have no code
        assert_eq!((s.current_value.clone(), s.recommended_value.clone()), (None, None));
        let w = by_key(&v, "write_cache");
        assert_eq!(w.current_value, Some(CheckValue::code("off")));
        assert_eq!(w.recommended_value, Some(CheckValue::code("on")));
        let a = by_key(&v, "apm");
        assert_eq!(a.current.as_deref(), Some("disabled"));
        assert_eq!(a.current_value, Some(CheckValue::code("disabled")));
        assert_eq!(a.recommended_value, Some(CheckValue::code("apm_254_off")));
        let t = by_key(&v, "temperature");
        assert_eq!(t.current_value, Some(CheckValue::with("temp", 41.0)));
        assert_eq!(t.recommended_value, Some(CheckValue::with("temp_below", 50.0)));
        assert_eq!(t.recommended, "below 50 °C");
    }

    #[test]
    fn ssd_trim_overlap_and_raid5() {
        let mut i = TuningInput {
            kind: DiskKind::Ssd,
            scheduler: Some("mq-deadline".into()),
            read_ahead_kb: Some(128),
            discard_supported: true,
            fstrim_timer: Some(true),
            online_discard: true,
            alpm: Some("max_performance".into()),
            transport: "sata".into(),
            ..Default::default()
        };
        let v = checks(&i);
        // async discard plus the timer: harmless, the timer finds nothing to do
        assert_eq!(by_key(&v, "trim").status, CheckStatus::Ok);
        assert_eq!(by_key(&v, "alpm").status, CheckStatus::Differs);
        i.scheduler = Some("none".into());
        assert_eq!(by_key(&checks(&i), "scheduler").status, CheckStatus::Ok);
        i.under_parity_raid = true;
        assert_eq!(
            by_key(&checks(&i), "trim").status,
            CheckStatus::NotApplicable
        );
        i.under_parity_raid = false;
        i.crypt_blocks_discard = true;
        let t = checks(&i);
        assert_eq!(by_key(&t, "trim").status, CheckStatus::NotApplicable);
        assert_eq!(by_key(&t, "trim").note_code.as_deref(), Some("trim_crypt"));
    }

    #[test]
    fn nvme() {
        let i = TuningInput {
            kind: DiskKind::Nvme,
            scheduler: Some("none".into()),
            read_ahead_kb: Some(128),
            discard_supported: true,
            fstrim_timer: Some(false),
            online_discard: true,
            apst_latency_us: Some(100_000),
            temperature_c: Some(41.0),
            transport: "nvme".into(),
            ..Default::default()
        };
        let v = checks(&i);
        assert!(v.iter().all(|c| c.key != "write_cache" && c.key != "apm"));
        assert_eq!(by_key(&v, "scheduler").status, CheckStatus::Ok);
        assert_eq!(by_key(&v, "trim").status, CheckStatus::Ok);
        assert_eq!(by_key(&v, "apst").status, CheckStatus::Ok);
    }

    #[test]
    fn sysfs_inputs() {
        let mut h = crate::host::FakeHost::default();
        h.file("/sys/block/sda/queue/scheduler", "[mq-deadline] none\n")
            .file("/sys/block/sda/queue/read_ahead_kb", "2048\n")
            .file("/sys/block/sda/queue/discard_max_bytes", "0\n")
            .file(
                "/sys/class/scsi_host/host2/link_power_management_policy",
                "med_power_with_dipm\n",
            );
        h.links.insert(
            "/sys/block/sda".into(),
            "/sys/devices/pci0000:00/0000:00:17.0/ata3/host2/target2:0:0/2:0:0:0/block/sda".into(),
        );
        let mut i = TuningInput::default();
        read_sysfs(&h, "sda", &mut i);
        assert_eq!(i.scheduler.as_deref(), Some("mq-deadline"));
        assert_eq!(i.read_ahead_kb, Some(2048));
        assert!(!i.discard_supported);
        assert_eq!(i.fstrim_timer, Some(false));
        assert_eq!(i.alpm.as_deref(), Some("med_power_with_dipm"));
    }
}
