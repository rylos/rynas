//! Per-disk-type recommended settings compared with the current values, read-only
//! (`docs/research/system-reliability-tweaks.md` §6, decision D-SYSTUNE). Applying them is a
//! later job (generated udev rules with per-disk overrides).

use rynas_api_types::storage::{CheckStatus, DiskKind, SettingCheck};

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

fn check(
    key: &str,
    label: &str,
    current: Option<String>,
    recommended: &str,
    status: CheckStatus,
    note: &str,
) -> SettingCheck {
    SettingCheck {
        key: key.into(),
        label: label.into(),
        current,
        recommended: recommended.into(),
        status,
        note: note.into(),
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

    let want_sched = match i.kind {
        Nvme => "none",
        _ => "mq-deadline",
    };
    v.push(check(
        "scheduler",
        "I/O scheduler",
        i.scheduler.clone(),
        want_sched,
        ok_if(i.scheduler.as_ref().map(|s| s == want_sched)),
        match i.kind {
            Nvme => "NVMe has deep hardware queues: no scheduler is fastest",
            Hdd => "mq-deadline keeps throughput on RAID; bfq only helps desktop interactivity",
            _ => "mq-deadline: low overhead, bounded latency",
        },
    ));

    let (ra_label, ra_ok): (&str, Box<dyn Fn(u32) -> bool>) = match i.kind {
        Hdd => (
            "2048 (1024-4096)",
            Box::new(|kb| (1024..=4096).contains(&kb)),
        ),
        Ssd => ("256 (128-512)", Box::new(|kb| (128..=512).contains(&kb))),
        Nvme => ("128", Box::new(|kb| (128..=512).contains(&kb))),
        Unknown => ("128", Box::new(|_| true)),
    };
    v.push(check(
        "read_ahead_kb",
        "Read-ahead (KiB)",
        i.read_ahead_kb.map(|k| k.to_string()),
        ra_label,
        ok_if(i.read_ahead_kb.map(ra_ok)),
        if i.kind == Hdd {
            "large read-ahead helps streaming and RAID reads on spinning disks"
        } else {
            "flash needs little read-ahead; more only wastes cache"
        },
    ));

    if i.kind != Nvme {
        v.push(check(
            "write_cache",
            "Write cache",
            i.write_cache.map(|w| if w { "on" } else { "off" }.into()),
            "on",
            ok_if(i.write_cache),
            "leave it on: the filesystem's flushes keep it safe, off costs a lot of speed",
        ));
    }

    if i.kind == Hdd {
        let current = i.apm.map(|a| match a {
            Some(l) => l.to_string(),
            None => "disabled".into(),
        });
        let status = match i.apm {
            Some(Some(l)) => {
                if l <= 127 {
                    CheckStatus::Ok
                } else {
                    CheckStatus::Differs
                }
            }
            // no APM: spin-down is left to hd-idle / -S
            Some(None) => CheckStatus::NotApplicable,
            None => CheckStatus::Unknown,
        };
        v.push(check(
            "apm",
            "Power management (APM)",
            current,
            "127 (allows spin-down)",
            status,
            "levels above 127 forbid standby; many NAS drives ignore -S timers, then hd-idle does it",
        ));
    }

    if flash {
        let (cur, status, note) = match (i.fstrim_timer, i.online_discard) {
            (Some(true), true) => (
                "fstrim.timer + discard mount".to_string(),
                CheckStatus::Differs,
                "both online discard and the weekly timer: keep only one",
            ),
            (Some(true), false) => (
                "fstrim.timer".to_string(),
                CheckStatus::Ok,
                "weekly batched TRIM",
            ),
            (_, true) => (
                "discard mount option".to_string(),
                CheckStatus::Ok,
                "btrfs discard=async (default since 6.2) trims continuously",
            ),
            (Some(false), false) => (
                "none".to_string(),
                CheckStatus::Differs,
                "without TRIM the SSD slows down and wears faster: enable fstrim.timer",
            ),
            (None, false) => ("unknown".to_string(), CheckStatus::Unknown, ""),
        };
        let (status, note) = if !i.discard_supported {
            (
                CheckStatus::NotApplicable,
                "the device does not accept discards",
            )
        } else if i.under_parity_raid {
            (
                CheckStatus::NotApplicable,
                "md RAID5/6 does not pass TRIM through (devices_handle_discard_safely is off)",
            )
        } else {
            (status, note)
        };
        v.push(check(
            "trim",
            "TRIM",
            Some(cur),
            "fstrim.timer weekly",
            status,
            note,
        ));
    }

    if i.kind == Ssd && i.transport == "sata" {
        let want = "med_power_with_dipm";
        v.push(check(
            "alpm",
            "SATA link power (ALPM)",
            i.alpm.clone(),
            want,
            ok_if(i.alpm.as_ref().map(|a| a == want)),
            "saves about 1 W per SSD with no measurable cost",
        ));
    }

    if i.kind == Nvme {
        v.push(check(
            "apst",
            "NVMe power states (APST)",
            i.apst_latency_us.map(|l| {
                if l == 0 {
                    "disabled".into()
                } else {
                    format!("enabled (max latency {l} µs)")
                }
            }),
            "enabled",
            ok_if(i.apst_latency_us.map(|l| l > 0)),
            "disable only for models with known APST firmware bugs",
        ));
    }

    let (warn, _) = temp_limits(i.kind);
    v.push(check(
        "temperature",
        "Temperature",
        i.temperature_c.map(|t| format!("{t:.0} °C")),
        &format!("below {warn:.0} °C"),
        ok_if(i.temperature_c.map(|t| t < warn)),
        match i.kind {
            Hdd => "HDDs age faster above 50-55 °C",
            Nvme => "NVMe throttles at 70-80 °C",
            _ => "keep airflow over the drive",
        },
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
        assert_eq!(by_key(&v, "read_ahead_kb").status, CheckStatus::Differs);
        assert_eq!(by_key(&v, "apm").status, CheckStatus::Differs);
        assert_eq!(by_key(&v, "write_cache").status, CheckStatus::Ok);
        assert_eq!(by_key(&v, "temperature").status, CheckStatus::Ok);
        assert!(v.iter().all(|c| c.key != "trim"));
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
        assert_eq!(by_key(&v, "trim").status, CheckStatus::Differs);
        assert_eq!(by_key(&v, "alpm").status, CheckStatus::Differs);
        i.under_parity_raid = true;
        assert_eq!(
            by_key(&checks(&i), "trim").status,
            CheckStatus::NotApplicable
        );
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
