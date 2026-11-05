//! Alerts and storage health derived from the data the UI already has (metrics sample,
//! hardware status, storage overview). Pure functions: the notification center and the
//! dashboard widgets localize the result, the tests run on the host.

use rynas_api_types::{HardwareStatus, MetricsSample, RaidArray, StorageOverview};

use crate::ui::{Tone, temp_limits, usage_tone};

/// One active alert. `title`/`text` are i18n keys, `args` fill the `{name}` placeholders.
#[derive(Clone, Debug, PartialEq)]
pub struct Alert {
    /// Stable id, e.g. "temp:cpu:warn", "array:md2:degraded": the "read" state hangs on it.
    pub key: String,
    pub tone: Tone,
    pub title: &'static str,
    pub text: &'static str,
    pub args: Vec<(&'static str, String)>,
    /// Page that shows the details.
    pub route: &'static str,
}

/// A temperature reading from any source: (id, label, °C). Bay ids are "bay{n}".
pub type Reading = (String, String, f32);

/// Every temperature once: the sample's sensors plus the provider's bays that the server did
/// not already merge into the sample (it does on Synology, the demo does not).
pub fn all_temps(sample: Option<&MetricsSample>, hw: Option<&HardwareStatus>) -> Vec<Reading> {
    let mut v: Vec<Reading> = sample
        .map(|s| s.temps.iter().filter_map(|t| t.celsius.map(|c| (t.id.clone(), t.label.clone(), c))).collect())
        .unwrap_or_default();
    if let Some(h) = hw {
        for b in &h.bays {
            let id = format!("bay{}", b.number);
            if let Some(c) = b.celsius
                && !v.iter().any(|(i, _, _)| *i == id)
            {
                v.push((id, format!("{} {}", crate::i18n::t("sto.bay"), b.number), c));
            }
        }
    }
    v
}

/// The array's state as (tone, i18n key).
pub fn array_status(a: &RaidArray) -> (Tone, &'static str) {
    let s = a.state.as_str();
    if s.contains("degraded") {
        (Tone::Warn, "health.degraded")
    } else if s.contains("fail") || s.contains("inactive") {
        (Tone::Err, "health.failed")
    } else if s.contains("resync") || s.contains("recover") || s.contains("check") || s.contains("reshape") {
        (Tone::Info, "health.syncing")
    } else if s.contains("clean") || s.contains("active") {
        (Tone::Ok, "health.healthy")
    } else {
        (Tone::Unknown, "health.unknown")
    }
}

/// Remote file systems (SMB/NFS/SSHFS mounts…): listed apart, never counted as local storage.
pub fn is_network_fs(fstype: &str) -> bool {
    matches!(fstype, "cifs" | "smb3" | "smbfs" | "nfs" | "nfs4" | "fuse.sshfs" | "sshfs" | "9p" | "ceph" | "glusterfs" | "fuse.glusterfs" | "davfs" | "fuse.rclone")
}

pub fn fs_pct(used: u64, size: u64) -> f64 {
    if size == 0 { 0.0 } else { used as f64 * 100.0 / size as f64 }
}

/// Overall storage state as (tone, i18n key): failure > degraded/full > resync > healthy.
pub fn storage_health(st: &StorageOverview) -> (Tone, &'static str) {
    let arrays: Vec<Tone> = st.arrays.iter().map(|a| array_status(a).0).collect();
    if arrays.contains(&Tone::Err) {
        (Tone::Err, "h.err")
    } else if arrays.contains(&Tone::Warn) || st.filesystems.iter().filter(|f| !is_network_fs(&f.fstype)).any(|f| usage_tone(fs_pct(f.used, f.size)) == Tone::Err) {
        (Tone::Warn, "h.warn")
    } else if arrays.contains(&Tone::Info) {
        (Tone::Info, "h.syncing")
    } else {
        (Tone::Ok, "h.ok")
    }
}

/// Degrees a sensor must drop below its warning threshold before its alert clears, so a
/// reading hovering on the threshold does not flap.
const HYSTERESIS: f32 = 3.0;

fn c_str(c: f32) -> String {
    crate::i18n::celsius(c)
}

/// Active alerts, most severe first. `prev` is the previous result (hysteresis).
pub fn derive(
    sample: Option<&MetricsSample>,
    hw: Option<&HardwareStatus>,
    st: Option<&StorageOverview>,
    cpu_offset: f32,
    prev: &[Alert],
) -> Vec<Alert> {
    let mut out = Vec::new();
    let hw_route = if hw.is_some_and(|h| h.provider.is_some()) { "/hardware" } else { "/" };

    if let Some(h) = hw {
        let mut any_fan = false;
        for f in h.fans.iter().filter(|f| f.alarm) {
            any_fan = true;
            out.push(Alert {
                key: format!("fan:{}", f.label),
                tone: Tone::Err,
                title: "notif.fan_fail",
                text: "notif.fan_fail_text",
                args: vec![("label", f.label.clone())],
                route: "/hardware",
            });
        }
        if h.fan_fail && !any_fan {
            out.push(Alert {
                key: "fan".into(),
                tone: Tone::Err,
                title: "notif.fan_fail",
                text: "notif.fan_fail_any",
                args: vec![],
                route: "/hardware",
            });
        }
    }

    for (id, label, c) in all_temps(sample, hw) {
        let (warn, crit) = temp_limits(&id, cpu_offset);
        let was = prev.iter().any(|a| a.key.starts_with(&format!("temp:{id}:")));
        let (tone, level, limit) = if c >= crit {
            (Tone::Err, "crit", crit)
        } else if c >= warn || (was && c >= warn - HYSTERESIS) {
            (Tone::Warn, "warn", warn)
        } else {
            continue;
        };
        out.push(Alert {
            key: format!("temp:{id}:{level}"),
            tone,
            title: if tone == Tone::Err { "notif.temp_crit" } else { "notif.temp_warn" },
            text: "notif.temp_text",
            args: vec![("label", label), ("value", c_str(c)), ("limit", c_str(limit))],
            route: hw_route,
        });
    }

    if let Some(st) = st {
        for a in &st.arrays {
            let level = a.level.to_uppercase();
            let (key, title, text, args) = match array_status(a).0 {
                Tone::Warn => ("degraded", "notif.array_degraded", "notif.array_degraded_text", vec![]),
                Tone::Err => ("failed", "notif.array_failed", "notif.array_failed_text", vec![]),
                Tone::Info => (
                    "sync",
                    "notif.array_sync",
                    "notif.array_sync_text",
                    vec![("pct", a.sync_pct.map(|p| format!("{}%", crate::i18n::num(p as f64, 1))).unwrap_or_else(|| "—".into()))],
                ),
                _ => continue,
            };
            let mut args = args;
            args.extend([("name", a.name.clone()), ("level", level)]);
            out.push(Alert {
                key: format!("array:{}:{key}", a.name),
                tone: array_status(a).0,
                title,
                text,
                args,
                route: "/storage",
            });
        }
        for f in st.filesystems.iter().filter(|f| !is_network_fs(&f.fstype)) {
            let pct = fs_pct(f.used, f.size);
            let tone = usage_tone(pct);
            if tone == Tone::Ok {
                continue;
            }
            out.push(Alert {
                key: format!("fs:{}:{}", f.mountpoint, tone.class()),
                tone,
                title: "notif.volume_full",
                text: "notif.volume_full_text",
                args: vec![("mount", f.mountpoint.clone()), ("pct", crate::i18n::pct(pct)), ("free", crate::i18n::bytes(f.avail))],
                route: "/storage",
            });
        }
    }

    out.sort_by_key(|a| match a.tone {
        Tone::Err => 0,
        Tone::Warn => 1,
        Tone::Info => 2,
        _ => 3,
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::{Bay, Fan, Filesystem, Temperature};

    fn sample(temps: &[(&str, f32)]) -> MetricsSample {
        MetricsSample {
            temps: temps.iter().map(|(id, c)| Temperature { id: id.to_string(), label: id.to_uppercase(), celsius: Some(*c) }).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn ryzen_tctl_63_raises_nothing() {
        let s = sample(&[("cpu", 63.0), ("nvme0", 33.0)]);
        assert!(derive(Some(&s), None, None, 20.0, &[]).is_empty());
        assert!(derive(Some(&s), None, None, 0.0, &[]).is_empty());
    }

    #[test]
    fn temperature_levels_and_hysteresis() {
        let hot = sample(&[("cpu", 85.0)]);
        let a = derive(Some(&hot), None, None, 0.0, &[]);
        assert_eq!(a.len(), 1);
        assert_eq!((a[0].key.as_str(), a[0].tone), ("temp:cpu:warn", Tone::Warn));
        // 78 °C: below the threshold but inside the hysteresis band, the alert stays
        let cooling = sample(&[("cpu", 78.0)]);
        assert_eq!(derive(Some(&cooling), None, None, 0.0, &a).len(), 1);
        assert!(derive(Some(&cooling), None, None, 0.0, &[]).is_empty());
        let cool = sample(&[("cpu", 76.0)]);
        assert!(derive(Some(&cool), None, None, 0.0, &a).is_empty());
        let crit = sample(&[("cpu", 95.0)]);
        assert_eq!(derive(Some(&crit), None, None, 0.0, &[])[0].tone, Tone::Err);
    }

    #[test]
    fn fans_arrays_and_volumes() {
        let hw = HardwareStatus {
            provider: Some("synology".into()),
            fans: vec![Fan { label: "Fan 1".into(), rpm: Some(0), alarm: true }, Fan { label: "Fan 2".into(), rpm: Some(1100), alarm: false }],
            fan_fail: true,
            bays: vec![Bay { number: 2, celsius: Some(62.0), ..Default::default() }],
            ..Default::default()
        };
        let st = StorageOverview {
            arrays: vec![
                RaidArray { name: "md2".into(), level: "raid5".into(), state: "clean, degraded".into(), ..Default::default() },
                RaidArray { name: "md3".into(), level: "raid1".into(), state: "active, resyncing".into(), sync_pct: Some(42.0), ..Default::default() },
                RaidArray { name: "md0".into(), level: "raid1".into(), state: "clean".into(), ..Default::default() },
            ],
            filesystems: vec![
                Filesystem { mountpoint: "/srv".into(), size: 100, used: 96, ..Default::default() },
                Filesystem { mountpoint: "/".into(), size: 100, used: 40, ..Default::default() },
                Filesystem { mountpoint: "/mnt/nas".into(), fstype: "cifs".into(), size: 100, used: 99, ..Default::default() },
            ],
            ..Default::default()
        };
        let a = derive(None, Some(&hw), Some(&st), 0.0, &[]);
        let keys: Vec<&str> = a.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(keys, ["fan:Fan 1", "temp:bay2:crit", "fs:/srv:err", "array:md2:degraded", "array:md3:sync"]);
        assert_eq!(storage_health(&st), (Tone::Warn, "h.warn"));
    }

    #[test]
    fn bays_are_not_counted_twice() {
        let hw = HardwareStatus { bays: vec![Bay { number: 1, celsius: Some(35.0), ..Default::default() }], ..Default::default() };
        let merged = sample(&[("cpu", 50.0), ("bay1", 35.0)]);
        assert_eq!(all_temps(Some(&merged), Some(&hw)).len(), 2);
        let plain = sample(&[("cpu", 50.0)]);
        assert_eq!(all_temps(Some(&plain), Some(&hw)).len(), 2);
    }

    #[test]
    fn health_order() {
        let mut st = StorageOverview::default();
        assert_eq!(storage_health(&st).0, Tone::Ok);
        st.arrays.push(RaidArray { state: "resyncing".into(), ..Default::default() });
        assert_eq!(storage_health(&st).0, Tone::Info);
        st.arrays.push(RaidArray { state: "inactive".into(), ..Default::default() });
        assert_eq!(storage_health(&st).0, Tone::Err);
    }
}
