//! btrfs (btrfs-progs 6.14 on Debian 13): `filesystem show --raw`, `filesystem usage -b`,
//! `device stats`, `subvolume list`, `scrub status --raw`, plus the mount options and the
//! quota mode from sysfs.

use std::collections::{HashMap, HashSet};

use rynas_api_types::storage::{
    BtrfsDeviceErrors, BtrfsProfile, QuotaMode, ScrubState, ScrubStatus, Subvolume,
};

use crate::host::Host;

/// One filesystem of `btrfs filesystem show --raw`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ShowFs {
    pub label: Option<String>,
    pub uuid: String,
    pub total_devices: u32,
    pub used: u64,
    pub devices: Vec<ShowDev>,
    pub missing: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ShowDev {
    pub devid: u64,
    pub size: u64,
    pub used: u64,
    pub path: String,
    pub missing: bool,
}

pub fn parse_show(s: &str) -> Vec<ShowFs> {
    let mut out: Vec<ShowFs> = Vec::new();
    for line in s.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("Label:") {
            // Label: 'vol1'  uuid: 3e6c...   /  Label: none  uuid: ...
            let (label, uuid) = rest.rsplit_once("uuid:").unwrap_or((rest, ""));
            let label = label.trim();
            let label = if label == "none" {
                None
            } else {
                Some(label.trim_matches('\'').to_string())
            };
            out.push(ShowFs {
                label,
                uuid: uuid.trim().to_string(),
                ..Default::default()
            });
            continue;
        }
        let Some(fs) = out.last_mut() else { continue };
        if let Some(rest) = t.strip_prefix("Total devices") {
            // Total devices 1 FS bytes used 584617291776
            let w: Vec<&str> = rest.split_whitespace().collect();
            fs.total_devices = w.first().and_then(|n| n.parse().ok()).unwrap_or(0);
            fs.used = w.last().and_then(|n| n.parse().ok()).unwrap_or(0);
        } else if let Some(rest) = t.strip_prefix("devid") {
            // devid    1 size 999128301568 used 765603086336 path /dev/nvme0n1p2
            let w: Vec<&str> = rest.split_whitespace().collect();
            let get = |k: &str| {
                w.iter()
                    .position(|x| *x == k)
                    .and_then(|i| w.get(i + 1))
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0)
            };
            let path = t
                .split_once(" path ")
                .map(|(_, p)| p.trim().to_string())
                .unwrap_or_default();
            let missing = path.contains("missing") || path.is_empty();
            fs.devices.push(ShowDev {
                devid: w.first().and_then(|n| n.parse().ok()).unwrap_or(0),
                size: get("size"),
                used: get("used"),
                missing,
                path,
            });
            if missing {
                fs.missing = true;
            }
        } else if t.contains("Some devices missing") {
            fs.missing = true;
        }
    }
    out
}

/// `btrfs filesystem usage -b <mnt>`
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Usage {
    pub device_size: u64,
    pub allocated: u64,
    pub unallocated: u64,
    pub missing: u64,
    pub used: u64,
    pub free_estimated: u64,
    pub free_min: u64,
    pub data_ratio: f32,
    pub metadata_ratio: f32,
    pub profiles: Vec<BtrfsProfile>,
    /// device path → bytes allocated (all profiles) on it
    pub per_device: HashMap<String, u64>,
}

pub fn parse_usage(s: &str) -> Usage {
    let mut u = Usage::default();
    let mut in_unallocated = false;
    let num = |v: &str| {
        v.split_whitespace()
            .next()
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(0)
    };
    for line in s.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if let Some((k, v)) = t.split_once(':')
            && !line.starts_with(' ')
            && !line.starts_with('\t')
        {
            // section header: "Data,single: Size:735521538048, Used:576430452736 (78.37%)"
            in_unallocated = k == "Unallocated";
            if let Some((kind, profile)) = k.split_once(',') {
                let field = |name: &str| {
                    v.split(name)
                        .nth(1)
                        .map(|x| x.trim_start_matches(':'))
                        .and_then(|x| {
                            x.split(|c: char| c == ',' || c.is_whitespace())
                                .next()
                                .and_then(|n| n.parse::<u64>().ok())
                        })
                        .unwrap_or(0)
                };
                u.profiles.push(BtrfsProfile {
                    kind: kind.trim().to_ascii_lowercase(),
                    profile: profile.trim().to_ascii_lowercase(),
                    size: field("Size"),
                    used: field("Used"),
                });
            }
            continue;
        }
        if let Some((k, v)) = t.split_once(':') {
            let v = v.trim();
            match k.trim() {
                "Device size" => u.device_size = num(v),
                "Device allocated" => u.allocated = num(v),
                "Device unallocated" => u.unallocated = num(v),
                "Device missing" => u.missing = num(v),
                "Used" => u.used = num(v),
                "Free (estimated)" => {
                    u.free_estimated = num(v);
                    u.free_min = v
                        .split("min:")
                        .nth(1)
                        .map(|x| num(x.trim_end_matches(')')))
                        .unwrap_or(0);
                }
                "Data ratio" => u.data_ratio = v.parse().unwrap_or(0.0),
                "Metadata ratio" => u.metadata_ratio = v.parse().unwrap_or(0.0),
                _ => {}
            }
            continue;
        }
        // per-device line inside a section: "/dev/nvme0n1p2	735521538048"
        if t.starts_with('/') && !in_unallocated {
            let mut w = t.split_whitespace();
            if let (Some(dev), Some(n)) = (w.next(), w.next()) {
                *u.per_device.entry(dev.to_string()).or_default() += n.parse::<u64>().unwrap_or(0);
            }
        }
    }
    u
}

/// `btrfs device stats <mnt>`: `[/dev/md2].write_io_errs    0`
pub fn parse_device_stats(s: &str) -> HashMap<String, BtrfsDeviceErrors> {
    let mut m: HashMap<String, BtrfsDeviceErrors> = HashMap::new();
    for line in s.lines() {
        let Some(rest) = line.trim().strip_prefix('[') else {
            continue;
        };
        let Some((dev, rest)) = rest.split_once("].") else {
            continue;
        };
        let mut w = rest.split_whitespace();
        let (Some(key), Some(val)) = (w.next(), w.next()) else {
            continue;
        };
        let n: u64 = val.parse().unwrap_or(0);
        let e = m.entry(dev.to_string()).or_default();
        match key {
            "write_io_errs" => e.write_io = n,
            "read_io_errs" => e.read_io = n,
            "flush_io_errs" => e.flush_io = n,
            "corruption_errs" => e.corruption = n,
            "generation_errs" => e.generation = n,
            _ => {}
        }
    }
    m
}

pub fn total_errors(e: &BtrfsDeviceErrors) -> u64 {
    e.write_io + e.read_io + e.flush_io + e.corruption + e.generation
}

/// Arguments of the subvolume listing (`-p` parent, `-c` cgen, `-g` gen, `-u` uuid,
/// `-q` parent uuid, `-R` received uuid). Paths are relative to the top level.
pub const SUBVOL_LIST_ARGS: &[&str] = &["subvolume", "list", "-p", "-c", "-g", "-u", "-q", "-R"];

/// `ID 256 gen 1112130 cgen 9 parent 5 top level 5 parent_uuid - received_uuid - uuid 633f... path @`
pub fn parse_subvolume_list(s: &str) -> Vec<Subvolume> {
    let opt = |v: &str| (v != "-" && !v.is_empty()).then(|| v.to_string());
    s.lines()
        .filter_map(|line| {
            let line = line.trim();
            let (left, path) = line.split_once(" path ")?;
            let w: Vec<&str> = left.split_whitespace().collect();
            let get = |k: &str| {
                w.iter()
                    .position(|x| *x == k)
                    .and_then(|i| w.get(i + 1))
                    .copied()
            };
            let num = |k: &str| get(k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            Some(Subvolume {
                id: get("ID")?.parse().ok()?,
                path: path.trim().trim_start_matches("<FS_TREE>/").to_string(),
                parent_id: num("parent"),
                generation: num("gen"),
                creation_generation: num("cgen"),
                uuid: get("uuid").and_then(opt),
                parent_uuid: get("parent_uuid").and_then(opt),
                received_uuid: get("received_uuid").and_then(opt),
                readonly: false,
                mountpoint: None,
            })
        })
        .collect()
}

/// IDs printed by `btrfs subvolume list -r` (read-only subvolumes, i.e. snapshots).
pub fn readonly_ids(s: &str) -> HashSet<u64> {
    s.lines()
        .filter_map(|l| l.trim().strip_prefix("ID "))
        .filter_map(|r| r.split_whitespace().next()?.parse().ok())
        .collect()
}

/// "1:25:12" → seconds; also "0:03:51".
fn hms(s: &str) -> Option<u64> {
    let p: Vec<u64> = s
        .trim()
        .split(':')
        .map(|x| x.parse().ok())
        .collect::<Option<_>>()?;
    Some(match p.as_slice() {
        [h, m, s] => h * 3600 + m * 60 + s,
        [m, s] => m * 60 + s,
        [s] => *s,
        _ => return None,
    })
}

/// `btrfs scrub status --raw <mnt>`
pub fn parse_scrub(s: &str) -> ScrubStatus {
    let mut st = ScrubStatus {
        status: ScrubState::Never,
        error_summary: String::new(),
        ..Default::default()
    };
    let first_num = |v: &str| {
        v.split(|c: char| !c.is_ascii_digit())
            .find(|x| !x.is_empty())
            .and_then(|n| n.parse::<u64>().ok())
    };
    let mut in_errors = false;
    for line in s.lines() {
        let t = line.trim();
        if t == "no stats available" {
            st.status = ScrubState::Never;
            continue;
        }
        let Some((k, v)) = t.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim() {
            "Scrub started" => st.started = Some(v.to_string()),
            "Status" => st.status = ScrubState::from_btrfs(v),
            "Duration" => st.duration_s = hms(v),
            "Time left" => st.time_left_s = hms(v),
            "Total to scrub" => st.total_bytes = first_num(v),
            // "Bytes scrubbed:   305882312704  (11.18%)"
            "Bytes scrubbed" => {
                st.scrubbed_bytes = first_num(v);
                st.pct = v
                    .split('(')
                    .nth(1)
                    .and_then(|p| p.split('%').next())
                    .and_then(|p| p.trim().parse().ok());
            }
            "Rate" => st.rate_bps = first_num(v),
            "Error summary" => {
                st.error_summary = v.to_string();
                in_errors = true;
                if v != "no errors found" {
                    st.errors = v
                        .split_whitespace()
                        .filter_map(|kv| kv.split_once('='))
                        .filter_map(|(_, n)| n.parse::<u64>().ok())
                        .sum();
                }
            }
            "Corrected" if in_errors => st.corrected = first_num(v),
            "Uncorrectable" if in_errors => st.uncorrectable = first_num(v),
            _ => {}
        }
    }
    if st.status == ScrubState::Running
        && st.pct.is_none()
        && let (Some(done), Some(total)) = (st.scrubbed_bytes, st.total_bytes)
        && total > 0
    {
        st.pct = Some((done as f64 * 100.0 / total as f64) as f32);
    }
    st
}

/// Quota mode from `/sys/fs/btrfs/<uuid>/qgroups` (the `mode` file exists since 6.7).
pub fn quota_mode(host: &dyn Host, uuid: &str) -> QuotaMode {
    let base = format!("/sys/fs/btrfs/{uuid}");
    match host.read(&format!("{base}/qgroups/mode")) {
        Some(m) => match m.trim() {
            "qgroup" => QuotaMode::Qgroup,
            "squota" => QuotaMode::Squota,
            _ => QuotaMode::Unknown,
        },
        None if !host.list(&format!("{base}/qgroups")).is_empty() => QuotaMode::Qgroup,
        None if host.read(&format!("{base}/label")).is_some() || !host.list(&base).is_empty() => {
            QuotaMode::None
        }
        None => QuotaMode::Unknown,
    }
}

/// The compression option among mount options: "zstd:1", "lzo", ... (None for "no").
pub fn compression(options: &[String]) -> Option<String> {
    options.iter().rev().find_map(|o| {
        let v = o
            .strip_prefix("compress-force=")
            .or_else(|| o.strip_prefix("compress="))?;
        (v != "no" && v != "none").then(|| v.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show() {
        let v = parse_show(include_str!("../tests/fixtures/btrfs-filesystem-show.txt"));
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].label.as_deref(), Some("root"));
        assert_eq!(v[0].devices[0].path, "/dev/md0");
        let vol1 = &v[1];
        assert_eq!(vol1.label.as_deref(), Some("vol1"));
        assert_eq!(vol1.uuid, "3e6c1f0a-9b2d-4c8e-a7f1-5d0b2c4e6a81");
        assert_eq!(vol1.total_devices, 1);
        assert_eq!(vol1.used, 9_812_337_541_120);
        assert_eq!(vol1.devices[0].path, "/dev/mapper/rynas-vol1");
        assert_eq!(vol1.devices[0].size, 16_002_569_404_416);
        assert!(!vol1.missing);
        let old = &v[2];
        assert_eq!(old.label, None);
        assert!(old.missing);
        assert_eq!(old.devices.len(), 2);
        assert!(old.devices[1].missing);
    }

    #[test]
    fn usage_real_single_device() {
        // captured from btrfs-progs on a single-device root filesystem
        let u = parse_usage(include_str!("../tests/fixtures/btrfs-usage-root.txt"));
        assert_eq!(u.device_size, 999_128_301_568);
        assert_eq!(u.used, 593_070_829_568);
        assert_eq!(u.free_estimated, 392_616_300_544);
        assert_eq!(u.free_min, 275_853_692_928);
        assert_eq!(u.data_ratio, 1.0);
        assert_eq!(u.metadata_ratio, 2.0);
        assert_eq!(u.profiles.len(), 3);
        assert_eq!(u.profiles[0].kind, "data");
        assert_eq!(u.profiles[0].profile, "single");
        assert_eq!(u.profiles[0].size, 735_521_538_048);
        assert_eq!(u.profiles[0].used, 576_430_452_736);
        assert_eq!(u.profiles[1].profile, "dup");
        assert_eq!(
            u.per_device["/dev/nvme0n1p2"],
            735_521_538_048 + 30_064_771_072 + 16_777_216
        );
    }

    #[test]
    fn usage_vol() {
        let u = parse_usage(include_str!("../tests/fixtures/btrfs-usage-vol1.txt"));
        assert_eq!(u.device_size, 16_002_569_404_416);
        assert_eq!(
            u.profiles
                .iter()
                .map(|p| p.profile.as_str())
                .collect::<Vec<_>>(),
            ["single", "dup", "dup"]
        );
    }

    #[test]
    fn device_stats() {
        let m = parse_device_stats(include_str!(
            "../tests/fixtures/btrfs-device-stats-vol1.txt"
        ));
        let e = &m["/dev/mapper/rynas-vol1"];
        assert_eq!(e.corruption, 2);
        assert_eq!(e.read_io, 0);
        assert_eq!(total_errors(e), 2);
    }

    #[test]
    fn subvolumes() {
        let v = parse_subvolume_list(include_str!(
            "../tests/fixtures/btrfs-subvolume-list-vol1.txt"
        ));
        assert_eq!(v.len(), 7);
        assert_eq!(v[0].id, 256);
        assert_eq!(v[0].path, "photos");
        assert_eq!(v[0].parent_id, 5);
        assert_eq!(v[0].generation, 81_234);
        assert_eq!(v[0].creation_generation, 12);
        assert_eq!(v[0].parent_uuid, None);
        let snap = v
            .iter()
            .find(|s| s.path == ".snapshots/photos/photos.20261105T0300")
            .unwrap();
        assert_eq!(
            snap.parent_uuid.as_deref(),
            Some("9d3e8a4c-2f1b-4e6d-8a7c-0b1d2e3f4a5b")
        );
        let space = v.iter().find(|s| s.path == "docs/Project files").unwrap();
        assert_eq!(space.id, 263);
        let ro = readonly_ids(include_str!(
            "../tests/fixtures/btrfs-subvolume-list-ro-vol1.txt"
        ));
        assert_eq!(ro, HashSet::from([260, 261]));
    }

    #[test]
    fn scrub_finished_real() {
        let s = parse_scrub(include_str!("../tests/fixtures/btrfs-scrub-finished.txt"));
        assert_eq!(s.status, ScrubState::Finished);
        assert_eq!(s.duration_s, Some(231));
        assert_eq!(s.total_bytes, Some(592_981_266_432));
        assert_eq!(s.rate_bps, Some(2_559_618_362));
        assert_eq!(s.errors, 0);
        assert_eq!(s.error_summary, "no errors found");
        assert_eq!(s.started.as_deref(), Some("Thu Oct 22 15:35:01 2026"));
    }

    #[test]
    fn scrub_running_with_errors() {
        let s = parse_scrub(include_str!("../tests/fixtures/btrfs-scrub-running.txt"));
        assert_eq!(s.status, ScrubState::Running);
        assert_eq!(s.pct, Some(11.18));
        assert_eq!(s.time_left_s, Some(5112));
        assert_eq!(s.scrubbed_bytes, Some(1_094_916_268_032));
        assert_eq!(s.errors, 4);
        assert_eq!(s.corrected, Some(3));
        assert_eq!(s.uncorrectable, Some(1));
    }

    #[test]
    fn scrub_never() {
        let s = parse_scrub(
            "UUID:             3e6c1f0a-9b2d-4c8e-a7f1-5d0b2c4e6a81\n\tno stats available\nTotal to scrub:   9812337541120\nRate:             0/s\nError summary:    no errors found\n",
        );
        assert_eq!(s.status, ScrubState::Never);
        assert_eq!(s.total_bytes, Some(9_812_337_541_120));
    }

    #[test]
    fn compress_option() {
        let o: Vec<String> = ["rw", "noatime", "compress=zstd:1", "space_cache=v2"]
            .map(String::from)
            .into();
        assert_eq!(compression(&o).as_deref(), Some("zstd:1"));
        assert_eq!(
            compression(&["compress-force=zstd:3".to_string()]).as_deref(),
            Some("zstd:3")
        );
        assert_eq!(compression(&["rw".to_string()]), None);
    }

    #[test]
    fn quota() {
        let mut h = crate::host::FakeHost::default();
        h.file("/sys/fs/btrfs/u1/label", "vol1\n");
        h.file("/sys/fs/btrfs/u2/qgroups/mode", "squota\n");
        h.file("/sys/fs/btrfs/u3/qgroups/0_5/referenced", "1\n");
        assert_eq!(quota_mode(&h, "u1"), QuotaMode::None);
        assert_eq!(quota_mode(&h, "u2"), QuotaMode::Squota);
        assert_eq!(quota_mode(&h, "u3"), QuotaMode::Qgroup);
        assert_eq!(quota_mode(&h, "nope"), QuotaMode::Unknown);
    }
}
