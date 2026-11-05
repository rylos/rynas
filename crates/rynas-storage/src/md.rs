//! md arrays: `/proc/mdstat` and sysfs (in memory, never wake a disk), refined with
//! `mdadm --detail --export` and `mdadm --detail` (mdadm 4.4 on Debian 13) for the superblock
//! facts sysfs does not have (UUID, name, events, bitmap).
//!
//! `mdadm --detail` loads a member superblock from disk, so the caller only runs it when no
//! member disk is in standby; otherwise the previous answer is kept and the array is `stale`.

use std::collections::HashMap;

use rynas_api_types::storage::{ArrayHealth, MdArray, MdMember, MemberRole, SyncProgress};

use crate::host::Host;

/// One member as listed in `/proc/mdstat`: `sdb1[1](F)`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StatMember {
    pub device: String,
    /// Descriptor number in brackets (not necessarily the slot).
    pub number: u32,
    pub faulty: bool,
    pub spare: bool,
    pub write_mostly: bool,
    pub journal: bool,
    pub replacement: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct StatArray {
    pub name: String,
    pub active: bool,
    pub read_only: bool,
    pub level: String,
    pub members: Vec<StatMember>,
    /// 1 KiB blocks.
    pub blocks: u64,
    pub metadata: Option<String>,
    pub chunk_bytes: Option<u64>,
    pub raid_disks: Option<u32>,
    pub working: Option<u32>,
    /// `[UU_]`
    pub map: Option<String>,
    pub sync: Option<SyncProgress>,
    pub bitmap: Option<String>,
}

fn parse_size_suffix(s: &str) -> Option<u64> {
    let s = s.trim();
    let (n, mult) = match s.chars().last()? {
        'k' | 'K' => (&s[..s.len() - 1], 1024),
        'M' => (&s[..s.len() - 1], 1024 * 1024),
        'G' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    n.parse::<u64>().ok().map(|v| v * mult)
}

fn parse_member(tok: &str) -> Option<StatMember> {
    let (dev, rest) = tok.split_once('[')?;
    let (num, flags) = rest.split_once(']')?;
    Some(StatMember {
        device: dev.to_string(),
        number: num.parse().ok()?,
        faulty: flags.contains("(F)"),
        spare: flags.contains("(S)"),
        write_mostly: flags.contains("(W)"),
        journal: flags.contains("(J)"),
        replacement: flags.contains("(R)"),
    })
}

/// `[==>....]  recovery = 41.3% (866432/2097088) finish=0.1min speed=144405K/sec`
/// or `resync=DELAYED`.
fn parse_sync(line: &str) -> Option<SyncProgress> {
    for action in ["resync", "recovery", "check", "repair", "reshape"] {
        let Some(i) = line.find(action) else { continue };
        let after = line[i + action.len()..].trim_start();
        let Some(v) = after.strip_prefix('=') else {
            continue;
        };
        let v = v.trim_start();
        let mut p = SyncProgress {
            action: action.to_string(),
            ..Default::default()
        };
        if let Some(pct) = v
            .split('%')
            .next()
            .and_then(|x| x.trim().parse::<f32>().ok())
        {
            p.pct = Some(pct);
            for tok in v.split_whitespace() {
                if let Some(f) = tok
                    .strip_prefix("finish=")
                    .and_then(|f| f.strip_suffix("min"))
                {
                    p.finish_s = f.parse::<f64>().ok().map(|m| (m * 60.0) as u64);
                }
                if let Some(s) = tok
                    .strip_prefix("speed=")
                    .and_then(|s| s.strip_suffix("K/sec"))
                {
                    p.speed_kib_s = s.parse().ok();
                }
            }
        } else {
            let w = v
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            p.waiting = Some(w);
        }
        return Some(p);
    }
    None
}

pub fn parse_mdstat(s: &str) -> Vec<StatArray> {
    let mut out: Vec<StatArray> = Vec::new();
    for line in s.lines() {
        if !line.starts_with(' ')
            && let Some((name, rest)) = line.split_once(" : ")
            && name.trim().starts_with("md")
        {
            let mut a = StatArray {
                name: name.trim().to_string(),
                ..Default::default()
            };
            let mut toks = rest.split_ascii_whitespace().peekable();
            a.active = toks.next() == Some("active");
            while let Some(t) = toks.peek().filter(|t| t.starts_with('(')) {
                if t.contains("read-only") {
                    a.read_only = true;
                }
                toks.next();
            }
            if let Some(t) = toks.peek()
                && !t.contains('[')
            {
                a.level = t.to_string();
                toks.next();
            }
            a.members = toks.filter_map(parse_member).collect();
            // mdstat lists members newest first
            a.members.sort_by_key(|m| m.number);
            out.push(a);
            continue;
        }
        let Some(a) = out.last_mut() else { continue };
        let t = line.trim();
        if t.is_empty() || t.starts_with("unused devices") {
            continue;
        }
        if let Some((n, rest)) = t.split_once(" blocks") {
            a.blocks = n.trim().parse().unwrap_or(0);
            // "super 1.2 level 5, 512k chunk, algorithm 2" / "512k chunks" / "512K chunks 2 near-copies"
            let words: Vec<&str> = rest.split_whitespace().collect();
            for (i, w) in words.iter().enumerate() {
                if *w == "super" {
                    a.metadata = words.get(i + 1).map(|s| s.to_string());
                } else if w.starts_with("chunk")
                    && i > 0
                    && let Some(b) = parse_size_suffix(words[i - 1])
                {
                    a.chunk_bytes = Some(b);
                }
            }
            // [3/2] [UU_]
            for part in rest.split_whitespace() {
                let Some(inner) = part.strip_prefix('[').and_then(|p| p.strip_suffix(']')) else {
                    continue;
                };
                if let Some((n, m)) = inner.split_once('/') {
                    a.raid_disks = n.parse().ok();
                    a.working = m.parse().ok();
                } else if !inner.is_empty() && inner.bytes().all(|b| b == b'U' || b == b'_') {
                    a.map = Some(inner.to_string());
                }
            }
            continue;
        }
        if let Some(b) = t.strip_prefix("bitmap:") {
            a.bitmap = Some(
                if b.contains("file:") {
                    "file"
                } else {
                    "internal"
                }
                .to_string(),
            );
            continue;
        }
        if let Some(p) = parse_sync(t) {
            a.sync = Some(p);
        }
    }
    out
}

/// `mdadm --detail --export /dev/mdX`
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Export {
    pub level: Option<String>,
    pub devices: Option<u32>,
    pub metadata: Option<String>,
    pub uuid: Option<String>,
    pub name: Option<String>,
    /// (device, role) with role "0".."n", "spare", "faulty", "journal".
    pub members: Vec<(String, String)>,
}

pub fn parse_export(s: &str) -> Export {
    let mut e = Export::default();
    let mut roles: HashMap<String, String> = HashMap::new();
    let mut devs: HashMap<String, String> = HashMap::new();
    for line in s.lines() {
        let Some((k, v)) = line.trim().split_once('=') else {
            continue;
        };
        let v = v.trim().to_string();
        match k {
            "MD_LEVEL" => e.level = Some(v),
            "MD_DEVICES" => e.devices = v.parse().ok(),
            "MD_METADATA" => e.metadata = Some(v),
            "MD_UUID" => e.uuid = Some(v),
            "MD_NAME" => e.name = Some(v),
            _ => {
                if let Some(mid) = k.strip_prefix("MD_DEVICE_") {
                    if let Some(id) = mid.strip_suffix("_ROLE") {
                        roles.insert(id.to_string(), v);
                    } else if let Some(id) = mid.strip_suffix("_DEV") {
                        devs.insert(id.to_string(), v);
                    }
                }
            }
        }
    }
    let mut members: Vec<(String, String)> = devs
        .into_iter()
        .map(|(id, dev)| {
            let role = roles.remove(&id).unwrap_or_default();
            (dev.trim_start_matches("/dev/").to_string(), role)
        })
        .collect();
    members.sort();
    e.members = members;
    e
}

/// The useful lines of plain `mdadm --detail /dev/mdX`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Detail {
    pub state: Option<String>,
    pub events: Option<u64>,
    /// "Internal", "none" or a path; None when the line is missing (no bitmap).
    pub bitmap: Option<String>,
    pub consistency_policy: Option<String>,
    pub chunk_bytes: Option<u64>,
    pub name: Option<String>,
    pub uuid: Option<String>,
    /// (device, state words), e.g. ("sdc1", "spare rebuilding").
    pub devices: Vec<(String, String)>,
}

pub fn parse_detail(s: &str) -> Detail {
    let mut d = Detail::default();
    let mut in_table = false;
    for line in s.lines() {
        let t = line.trim();
        if t.starts_with("Number") && t.contains("RaidDevice") {
            in_table = true;
            continue;
        }
        if in_table {
            // "0  8  1  0  active sync   /dev/sda1" or "-  0  0  1  removed"
            let words: Vec<&str> = t.split_whitespace().collect();
            if words.len() >= 5 {
                let last = *words.last().unwrap();
                if let Some(dev) = last.strip_prefix("/dev/") {
                    let state = words[4..words.len() - 1].join(" ");
                    d.devices.push((dev.to_string(), state));
                }
            }
            continue;
        }
        let Some((k, v)) = t.split_once(" : ") else {
            continue;
        };
        let v = v.trim();
        match k.trim() {
            "State" => d.state = Some(v.to_string()),
            "Events" => d.events = v.parse().ok(),
            "Intent Bitmap" => d.bitmap = Some(v.to_ascii_lowercase()),
            "Consistency Policy" => d.consistency_policy = Some(v.to_string()),
            "Chunk Size" => d.chunk_bytes = parse_size_suffix(v),
            // "nas:vol1  (local to host nas)"
            "Name" => d.name = v.split_whitespace().next().map(str::to_string),
            "UUID" => d.uuid = Some(v.to_string()),
            _ => {}
        }
    }
    d
}

/// Live facts from `/sys/block/<md>/md/`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SysMd {
    pub array_state: Option<String>,
    pub degraded: Option<u32>,
    pub mismatch_cnt: Option<u64>,
    pub sync_action: Option<String>,
    pub bitmap_location: Option<String>,
    /// member → (state, slot, errors)
    pub members: HashMap<String, (String, Option<u32>, Option<u64>)>,
}

pub fn read_sysfs(host: &dyn Host, md: &str) -> SysMd {
    let base = format!("/sys/block/{md}/md");
    let r = |f: &str| {
        host.read(&format!("{base}/{f}"))
            .map(|s| s.trim().to_string())
    };
    let mut s = SysMd {
        array_state: r("array_state"),
        degraded: r("degraded").and_then(|v| v.parse().ok()),
        mismatch_cnt: r("mismatch_cnt").and_then(|v| v.parse().ok()),
        sync_action: r("sync_action"),
        bitmap_location: r("bitmap/location"),
        members: HashMap::new(),
    };
    for e in host.list(&base) {
        let Some(dev) = e.strip_prefix("dev-") else {
            continue;
        };
        let state = r(&format!("{e}/state")).unwrap_or_default();
        let slot = r(&format!("{e}/slot")).and_then(|v| v.parse().ok());
        let errors = r(&format!("{e}/errors")).and_then(|v| v.parse().ok());
        s.members.insert(dev.to_string(), (state, slot, errors));
    }
    s
}

/// Members a level may lose before the data is gone (guaranteed, worst case).
pub fn tolerance(level: &str, raid_disks: u32) -> u32 {
    match level {
        "raid1" => raid_disks.saturating_sub(1),
        "raid4" | "raid5" => 1,
        "raid6" => 2,
        "raid10" => 1,
        _ => 0,
    }
}

/// Combine everything known about one array. `detail` and `export` are the last answers of
/// mdadm (possibly old: then `stale`).
pub fn build(
    st: &StatArray,
    sys: &SysMd,
    export: Option<&Export>,
    detail: Option<&Detail>,
    stale: bool,
    disk_of: &dyn Fn(&str) -> Option<String>,
) -> MdArray {
    let raid_disks = st
        .raid_disks
        .or(export.and_then(|e| e.devices))
        .unwrap_or(st.members.iter().filter(|m| !m.spare && !m.faulty).count() as u32);
    let degraded = sys.degraded.unwrap_or_else(|| {
        st.raid_disks
            .zip(st.working)
            .map(|(n, w)| n.saturating_sub(w))
            .unwrap_or(0)
    });
    let level = if st.level.is_empty() {
        export.and_then(|e| e.level.clone()).unwrap_or_default()
    } else {
        st.level.clone()
    };
    let recovering = st
        .sync
        .as_ref()
        .is_some_and(|s| s.action == "recovery" && s.pct.is_some());
    let members = st
        .members
        .iter()
        .map(|m| {
            let (sys_state, slot, errors) = sys.members.get(&m.device).cloned().unwrap_or_default();
            let detail_state = detail
                .and_then(|d| d.devices.iter().find(|(dev, _)| dev == &m.device))
                .map(|(_, s)| s.as_str());
            let export_role = export
                .and_then(|e| e.members.iter().find(|(dev, _)| dev == &m.device))
                .map(|(_, r)| r.as_str());
            let role = if m.faulty || sys_state.contains("faulty") {
                MemberRole::Faulty
            } else if m.journal || sys_state.contains("journal") {
                MemberRole::Journal
            } else if detail_state.is_some_and(|s| s.contains("rebuilding"))
                || (recovering && sys_state.contains("spare") && slot.is_some())
            {
                MemberRole::Rebuilding
            } else if m.spare || sys_state == "spare" || export_role == Some("spare") {
                MemberRole::Spare
            } else if sys_state.contains("in_sync")
                || detail_state.is_some_and(|s| s.contains("active"))
                || export_role.is_some_and(|r| r.parse::<u32>().is_ok())
                || (sys_state.is_empty() && st.active)
            {
                MemberRole::Active
            } else {
                MemberRole::Unknown
            };
            let slot = slot.or_else(|| export_role.and_then(|r| r.parse().ok()));
            MdMember {
                device: m.device.clone(),
                disk: disk_of(&m.device),
                role,
                slot: if matches!(role, MemberRole::Spare | MemberRole::Faulty) {
                    None
                } else {
                    slot
                },
                state: if sys_state.is_empty() {
                    detail_state.unwrap_or("").to_string()
                } else {
                    sys_state
                },
                errors,
            }
        })
        .collect::<Vec<_>>();
    let health = if !st.active {
        ArrayHealth::Inactive
    } else if degraded == 0 {
        ArrayHealth::Healthy
    } else if degraded > tolerance(&level, raid_disks) {
        ArrayHealth::Failed
    } else {
        ArrayHealth::Degraded
    };
    let state = detail
        .and_then(|d| d.state.clone())
        .filter(|_| !stale)
        .or_else(|| sys.array_state.clone())
        .unwrap_or_else(|| if st.active { "active" } else { "inactive" }.to_string());
    let bitmap = match (
        &st.bitmap,
        &sys.bitmap_location,
        detail.and_then(|d| d.bitmap.clone()),
    ) {
        (Some(b), _, _) => Some(b.clone()),
        (None, Some(loc), _) if loc != "none" => Some("internal".to_string()),
        (None, Some(_), _) => Some("none".to_string()),
        (None, None, d) => d,
    };
    MdArray {
        path: format!("/dev/{}", st.name),
        name: st.name.clone(),
        md_name: export
            .and_then(|e| e.name.clone())
            .or_else(|| detail.and_then(|d| d.name.clone())),
        uuid: export
            .and_then(|e| e.uuid.clone())
            .or_else(|| detail.and_then(|d| d.uuid.clone())),
        level,
        metadata: st
            .metadata
            .clone()
            .or_else(|| export.and_then(|e| e.metadata.clone())),
        raid_disks,
        size: st.blocks * 1024,
        chunk_bytes: st.chunk_bytes.or(detail.and_then(|d| d.chunk_bytes)),
        state,
        health,
        degraded,
        sync: st.sync.clone(),
        bitmap,
        events: detail.and_then(|d| d.events),
        mismatch_cnt: sys.mismatch_cnt,
        members,
        stale,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MDSTAT: &str = include_str!("../tests/fixtures/proc-mdstat.txt");

    #[test]
    fn mdstat_shapes() {
        let v = parse_mdstat(MDSTAT);
        let names: Vec<_> = v.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["md2", "md0", "md3", "md4", "md5", "md6"]);

        let md2 = &v[0];
        assert!(md2.active);
        assert_eq!(md2.level, "raid5");
        assert_eq!(md2.metadata.as_deref(), Some("1.2"));
        assert_eq!(md2.chunk_bytes, Some(512 * 1024));
        assert_eq!((md2.raid_disks, md2.working), (Some(3), Some(2)));
        assert_eq!(md2.map.as_deref(), Some("UU_"));
        assert_eq!(md2.bitmap.as_deref(), Some("internal"));
        let s = md2.sync.as_ref().unwrap();
        assert_eq!(s.action, "recovery");
        assert_eq!(s.pct, Some(37.4));
        assert_eq!(s.speed_kib_s, Some(187_342));
        assert_eq!(s.finish_s, Some(28_620)); // 477.0 min
        assert_eq!(md2.members.len(), 3);
        assert_eq!(md2.members[2].device, "sdc1");

        let md0 = &v[1];
        assert_eq!(md0.level, "raid1");
        assert!(md0.sync.is_none());
        assert_eq!(md0.map.as_deref(), Some("UU"));

        let md3 = &v[2];
        assert!(!md3.active);
        assert!(md3.members[0].spare);

        let md4 = &v[3];
        assert!(md4.members[1].faulty);
        assert_eq!(md4.working, Some(1));

        let md5 = &v[4];
        assert_eq!(
            md5.sync.as_ref().unwrap().waiting.as_deref(),
            Some("delayed")
        );

        let md6 = &v[5];
        assert_eq!(md6.level, "raid6");
        let s = md6.sync.as_ref().unwrap();
        assert_eq!(s.action, "check");
        assert_eq!(s.pct, Some(2.1));
        assert!(md6.members.iter().any(|m| m.spare));
    }

    #[test]
    fn export() {
        let e = parse_export(include_str!(
            "../tests/fixtures/mdadm-detail-export-md2.txt"
        ));
        assert_eq!(e.level.as_deref(), Some("raid5"));
        assert_eq!(e.devices, Some(3));
        assert_eq!(
            e.uuid.as_deref(),
            Some("6b3f0c2e:1d4a8e7b:2c9f0a1b:2c3d4e5f")
        );
        assert_eq!(e.name.as_deref(), Some("nas:vol1"));
        assert_eq!(
            e.members,
            [
                ("sda1".to_string(), "0".to_string()),
                ("sdb1".to_string(), "1".to_string()),
                ("sdc1".to_string(), "spare".to_string()),
            ]
        );
    }

    #[test]
    fn detail() {
        let d = parse_detail(include_str!("../tests/fixtures/mdadm-detail-md2.txt"));
        assert_eq!(d.state.as_deref(), Some("clean, degraded, recovering"));
        assert_eq!(d.events, Some(48_213));
        assert_eq!(d.bitmap.as_deref(), Some("internal"));
        assert_eq!(d.chunk_bytes, Some(512 * 1024));
        assert_eq!(d.name.as_deref(), Some("nas:vol1"));
        assert_eq!(d.devices.len(), 3);
        assert_eq!(
            d.devices[2],
            ("sdc1".to_string(), "spare rebuilding".to_string())
        );
    }

    #[test]
    fn combined() {
        let st = &parse_mdstat(MDSTAT)[0];
        let e = parse_export(include_str!(
            "../tests/fixtures/mdadm-detail-export-md2.txt"
        ));
        let d = parse_detail(include_str!("../tests/fixtures/mdadm-detail-md2.txt"));
        let mut sys = SysMd {
            array_state: Some("clean".into()),
            degraded: Some(1),
            mismatch_cnt: Some(0),
            ..Default::default()
        };
        sys.members
            .insert("sda1".into(), ("in_sync".into(), Some(0), Some(0)));
        sys.members
            .insert("sdb1".into(), ("in_sync".into(), Some(1), Some(0)));
        sys.members
            .insert("sdc1".into(), ("spare".into(), Some(2), Some(0)));
        let disk = |p: &str| Some(p.trim_end_matches(char::is_numeric).to_string());
        let a = build(st, &sys, Some(&e), Some(&d), false, &disk);
        assert_eq!(a.health, ArrayHealth::Degraded);
        assert_eq!(a.degraded, 1);
        assert_eq!(a.state, "clean, degraded, recovering");
        assert_eq!(a.events, Some(48_213));
        assert_eq!(a.size, 15_627_585_536 * 1024);
        assert_eq!(a.members[0].role, MemberRole::Active);
        assert_eq!(a.members[0].disk.as_deref(), Some("sda"));
        assert_eq!(a.members[2].role, MemberRole::Rebuilding);
        assert_eq!(a.members[2].slot, Some(2));

        // without mdadm (member asleep): sysfs and mdstat still tell the story
        let a = build(st, &sys, None, None, true, &disk);
        assert_eq!(a.state, "clean");
        assert!(a.stale);
        assert_eq!(a.members[2].role, MemberRole::Rebuilding);
        assert_eq!(a.uuid, None);
    }

    #[test]
    fn failed_and_inactive() {
        let v = parse_mdstat(MDSTAT);
        let none = |_: &str| None;
        let md3 = build(&v[2], &SysMd::default(), None, None, false, &none);
        assert_eq!(md3.health, ArrayHealth::Inactive);
        assert_eq!(md3.members[0].role, MemberRole::Spare);
        let md4 = build(&v[3], &SysMd::default(), None, None, false, &none);
        assert_eq!(md4.health, ArrayHealth::Degraded);
        assert_eq!(md4.members[1].role, MemberRole::Faulty);
        let raid0 = parse_mdstat(
            "md9 : active raid0 sda1[0] sdb1[1](F)\n      100 blocks super 1.2 512k chunks [2/1] [U_]\n",
        );
        let a = build(&raid0[0], &SysMd::default(), None, None, false, &none);
        assert_eq!(a.health, ArrayHealth::Failed);
    }
}
