//! Starting the arrays of a rynas volume that stayed inactive after boot because a drive is
//! missing.
//!
//! Why this exists: udev assembles md arrays incrementally (`mdadm -I` per member) and starts
//! one only when all its members are there. A degraded array is started later by
//! `mdadm-last-resort@mdX.timer` (30 s after the array appeared) or, in the initramfs, only
//! when the root filesystem needs it. For a data array that last resort is not guaranteed:
//! the timer is armed by udev and may never fire (members busy, the array appearing before
//! the timer unit is loadable, a distribution without it). The volume's mount (`nofail`) then
//! simply times out, and a flexible RAID volume additionally needs its volume group activated
//! once every tier runs, which nothing does after the boot-time `lvm2-pvscan` ran on an
//! incomplete group. The data is intact and the array has enough members; the volume is just
//! not there.
//!
//! What is done, and only for arrays rynas made (their UUID is in the `storage.volumes`
//! records; any other array is never touched):
//! - an array is a candidate when it is `inactive`, its level and member count are known and
//!   the members present are enough for its level to run degraded (raid1: one; raid4/5: all
//!   but one; raid6: all but two; raid10: all but one, the worst case). Arrays lacking
//!   members are never forced (`--force` or `--assume-clean` are never used): `mdadm --run`
//!   asks the kernel, which refuses an array that cannot run anyway;
//! - nothing happens until the machine is up for [`MIN_UPTIME_S`] and the same array was
//!   seen inactive with the same members for at least [`STABLE_MS`]: a drive that is merely
//!   slow to appear gets its chance to join, and the boot's own last resort goes first;
//! - a volume is handled as a whole: all its arrays must be active already or candidates,
//!   otherwise none is started (a flexible RAID with one tier missing could not be used);
//! - then `mdadm --run` on each candidate, `vgchange -ay` on the volume group of a flexible
//!   RAID, and, for an unencrypted volume that is not mounted, `systemctl start` of its
//!   mount unit (the `/etc/fstab` line rynas wrote; an encrypted volume waits for its unlock).

use std::collections::HashMap;
use std::time::Duration;

use rynas_api_types::storage::{ArrayHealth, MdArray, MemberRole, VolumeRecord};

use crate::Inventory;
use crate::host::Host;

/// The machine must be up this long before an array is started degraded.
pub const MIN_UPTIME_S: u64 = 180;
/// An array must stay inactive with the same members this long.
pub const STABLE_MS: u64 = 120_000;
const CMD_TIMEOUT: Duration = Duration::from_secs(60);

/// An inactive array of a volume that can run with the members it has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runnable {
    /// Kernel name (`md127`).
    pub array: String,
    pub path: String,
    pub uuid: String,
    pub level: String,
    /// Members present / members the array has.
    pub present: u32,
    pub raid_disks: u32,
    /// The member devices present, sorted (the stability check compares them).
    pub members: Vec<String>,
}

/// A volume whose inactive arrays can all be started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeStart {
    pub label: String,
    pub arrays: Vec<Runnable>,
    /// Flexible RAID: the volume group to activate afterwards.
    pub vg: Option<String>,
    /// Mount unit to start afterwards (unencrypted volumes not mounted yet).
    pub mount_unit: Option<String>,
}

/// Members the level may lose and still run (worst case), `None` for levels that cannot run
/// degraded or are unknown.
fn may_lose(level: &str, raid_disks: u32) -> Option<u32> {
    match level {
        "raid1" => Some(raid_disks.saturating_sub(1)),
        "raid4" | "raid5" | "raid10" => Some(1),
        "raid6" => Some(2),
        _ => None,
    }
}

/// The array as a candidate, `None` when it is not inactive or cannot run.
fn runnable(a: &MdArray) -> Option<Runnable> {
    if a.health != ArrayHealth::Inactive || a.raid_disks == 0 || a.reshape_position.is_some() {
        return None;
    }
    let lose = may_lose(&a.level, a.raid_disks)?;
    let mut members: Vec<String> = a
        .members
        .iter()
        .filter(|m| !matches!(m.role, MemberRole::Faulty | MemberRole::Journal))
        .map(|m| m.device.clone())
        .collect();
    members.sort();
    let present = members.len() as u32;
    if present == 0 || present + lose < a.raid_disks {
        return None;
    }
    Some(Runnable {
        array: a.name.clone(),
        path: a.path.clone(),
        uuid: a.uuid.clone().unwrap_or_default(),
        level: a.level.clone(),
        present,
        raid_disks: a.raid_disks,
        members,
    })
}

fn same_uuid(a: &str, b: &str) -> bool {
    !a.is_empty() && a.eq_ignore_ascii_case(b)
}

/// The volumes of `records` whose inactive arrays can all run now (see the module docs).
pub fn startable(inv: &Inventory, records: &[VolumeRecord]) -> Vec<VolumeStart> {
    let mut out = Vec::new();
    for r in records {
        let uuids = r.md_uuids();
        if uuids.is_empty() {
            continue;
        }
        let mut arrays = Vec::new();
        let mut whole = true;
        for u in &uuids {
            let found: Vec<&MdArray> = inv
                .arrays
                .iter()
                .filter(|a| a.uuid.as_deref().is_some_and(|x| same_uuid(u, x)))
                .collect();
            // not assembled at all, or twice (a split brain): leave it to the administrator
            let [a] = found.as_slice() else {
                whole = false;
                break;
            };
            match a.health {
                ArrayHealth::Healthy | ArrayHealth::Degraded => {}
                ArrayHealth::Inactive => match runnable(a) {
                    Some(c) => arrays.push(c),
                    None => {
                        whole = false;
                        break;
                    }
                },
                ArrayHealth::Failed | ArrayHealth::Unknown => {
                    whole = false;
                    break;
                }
            }
        }
        if !whole || arrays.is_empty() {
            continue;
        }
        let mounted = inv.mounts.iter().any(|m| m.mountpoint == r.mountpoint);
        out.push(VolumeStart {
            label: r.label.clone(),
            arrays,
            vg: r.vg.clone().filter(|_| !r.tiers.is_empty()),
            mount_unit: (r.luks_uuid.is_none() && !mounted && !r.mountpoint.is_empty())
                .then(|| crate::plan::mount_unit(&r.mountpoint)),
        });
    }
    out
}

/// Remembers since when each candidate array has been inactive with the same members.
#[derive(Debug, Default)]
pub struct Recovery {
    seen: HashMap<String, (u64, Vec<String>)>,
}

impl Recovery {
    /// The volumes of `found` (from [`startable`]) that are due at `now_ms` with the machine
    /// up for `uptime_s`: every array seen with the same members for [`STABLE_MS`]. Arrays
    /// no longer candidates are forgotten.
    pub fn due(&mut self, found: Vec<VolumeStart>, now_ms: u64, uptime_s: u64) -> Vec<VolumeStart> {
        let current: Vec<&Runnable> = found.iter().flat_map(|v| &v.arrays).collect();
        self.seen.retain(|k, _| current.iter().any(|a| &a.array == k));
        for a in &current {
            match self.seen.get(&a.array) {
                Some((_, m)) if *m == a.members => {}
                _ => {
                    self.seen.insert(a.array.clone(), (now_ms, a.members.clone()));
                }
            }
        }
        if uptime_s < MIN_UPTIME_S {
            return Vec::new();
        }
        found
            .into_iter()
            .filter(|v| {
                v.arrays.iter().all(|a| {
                    self.seen
                        .get(&a.array)
                        .is_some_and(|(t, _)| now_ms.saturating_sub(*t) >= STABLE_MS)
                })
            })
            .collect()
    }

    /// Forgets the arrays of a volume once it was handled (started or failed).
    pub fn forget(&mut self, v: &VolumeStart) {
        for a in &v.arrays {
            self.seen.remove(&a.array);
        }
    }
}

/// Seconds the machine is up (`/proc/uptime`), 0 when unreadable.
pub fn uptime_s(host: &dyn Host) -> u64 {
    host.read("/proc/uptime")
        .and_then(|t| t.split_whitespace().next()?.split('.').next()?.parse().ok())
        .unwrap_or(0)
}

fn run(host: &dyn Host, argv: &[&str]) -> Result<(), String> {
    let o = host
        .run(argv[0], &argv[1..], CMD_TIMEOUT)
        .map_err(|e| format!("{}: {e}", argv.join(" ")))?;
    if o.status == Some(0) {
        Ok(())
    } else {
        let why = o.stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
        Err(format!("{}: exit code {:?}: {why}", argv.join(" "), o.status))
    }
}

/// Starts the volume: `mdadm --run` on each array, `vgchange -ay` for a flexible RAID, then
/// its mount unit. Stops at the first failure; what ran stays (a started array is harmless).
/// `Ok(true)` when the volume was also mounted.
pub fn start(host: &dyn Host, v: &VolumeStart) -> Result<bool, String> {
    for a in &v.arrays {
        run(host, &["mdadm", "--run", &a.path])?;
    }
    if let Some(vg) = &v.vg {
        run(host, &["vgchange", "-ay", vg])?;
    }
    if let Some(unit) = &v.mount_unit {
        run(host, &["systemctl", "start", unit])?;
        return Ok(true);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeHost;
    use rynas_api_types::storage::{MdMember, RaidLevel, TierRecord};

    fn member(dev: &str, role: MemberRole) -> MdMember {
        MdMember { device: dev.into(), role, ..Default::default() }
    }

    fn array(name: &str, uuid: &str, level: &str, raid_disks: u32, health: ArrayHealth, members: &[&str]) -> MdArray {
        MdArray {
            name: name.into(),
            path: format!("/dev/{name}"),
            uuid: Some(uuid.into()),
            level: level.into(),
            raid_disks,
            health,
            members: members.iter().map(|d| member(d, MemberRole::Spare)).collect(),
            ..Default::default()
        }
    }

    fn classic(label: &str, uuid: &str, luks: bool) -> VolumeRecord {
        VolumeRecord {
            label: label.into(),
            md_uuid: uuid.into(),
            level: RaidLevel::Raid5,
            luks_uuid: luks.then(|| "luks-uuid".into()),
            fs_uuid: "fs".into(),
            mountpoint: format!("/{label}"),
            ..Default::default()
        }
    }

    fn flex(label: &str, tiers: &[&str]) -> VolumeRecord {
        VolumeRecord {
            label: label.into(),
            level: RaidLevel::Flex1,
            tiers: tiers
                .iter()
                .enumerate()
                .map(|(i, u)| TierRecord { index: i as u32 + 1, md_uuid: (*u).into() })
                .collect(),
            vg: Some(format!("rynas_{label}")),
            fs_uuid: "fs".into(),
            mountpoint: format!("/{label}"),
            ..Default::default()
        }
    }

    fn inv(arrays: Vec<MdArray>) -> Inventory {
        Inventory { arrays, ..Default::default() }
    }

    #[test]
    fn only_inactive_arrays_with_enough_members_of_rynas_volumes() {
        let records = vec![classic("vol1", "AA:01", false), classic("vol2", "BB:02", false)];
        let i = inv(vec![
            // a raid5 of 4 with 3 members: runs degraded
            array("md127", "aa:01", "raid5", 4, ArrayHealth::Inactive, &["sda1", "sdb1", "sdc1"]),
            // a raid5 of 4 with 2 members: never
            array("md126", "BB:02", "raid5", 4, ArrayHealth::Inactive, &["sdd1", "sde1"]),
            // not rynas's
            array("md125", "CC:03", "raid1", 2, ArrayHealth::Inactive, &["sdf1"]),
        ]);
        let s = startable(&i, &records);
        assert_eq!(s.len(), 1, "{s:?}");
        assert_eq!(s[0].label, "vol1");
        assert_eq!(s[0].arrays[0].array, "md127");
        assert_eq!((s[0].arrays[0].present, s[0].arrays[0].raid_disks), (3, 4));
        assert_eq!(s[0].mount_unit.as_deref(), Some("vol1.mount"));
        assert_eq!(s[0].vg, None);
    }

    #[test]
    fn levels_and_members_that_count() {
        let a = |level, n, m: &[&str]| runnable(&array("md1", "u", level, n, ArrayHealth::Inactive, m));
        assert!(a("raid1", 2, &["a"]).is_some());
        assert!(a("raid6", 5, &["a", "b", "c"]).is_some());
        assert!(a("raid6", 5, &["a", "b"]).is_none());
        assert!(a("raid10", 4, &["a", "b", "c"]).is_some());
        assert!(a("raid10", 4, &["a", "b"]).is_none(), "worst case: the two of one mirror");
        assert!(a("raid0", 2, &["a"]).is_none());
        assert!(a("", 0, &["a"]).is_none(), "level unknown");
        assert!(a("raid1", 2, &[]).is_none());
        // faulty members do not count
        let mut x = array("md1", "u", "raid5", 3, ArrayHealth::Inactive, &["a", "b"]);
        x.members[1].role = MemberRole::Faulty;
        assert!(runnable(&x).is_none());
        // running arrays are no candidates; nor is an interrupted reshape
        assert!(runnable(&array("md1", "u", "raid5", 3, ArrayHealth::Degraded, &["a", "b"])).is_none());
        let mut r = array("md1", "u", "raid5", 3, ArrayHealth::Inactive, &["a", "b"]);
        r.reshape_position = Some(1024);
        assert!(runnable(&r).is_none());
    }

    #[test]
    fn a_flexible_volume_is_started_whole_or_not_at_all() {
        let records = vec![flex("flex", &["t1", "t2"])];
        // tier 1 running, tier 2 inactive with enough members: start t2, then the group
        let i = inv(vec![
            array("md10", "t1", "raid5", 3, ArrayHealth::Degraded, &["sda1", "sdb1"]),
            array("md11", "t2", "raid1", 2, ArrayHealth::Inactive, &["sdb2"]),
        ]);
        let s = startable(&i, &records);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].arrays.iter().map(|a| a.array.as_str()).collect::<Vec<_>>(), ["md11"]);
        assert_eq!(s[0].vg.as_deref(), Some("rynas_flex"));
        // a tier that cannot run, or is not assembled at all: nothing
        let i = inv(vec![
            array("md10", "t1", "raid5", 3, ArrayHealth::Inactive, &["sda1"]),
            array("md11", "t2", "raid1", 2, ArrayHealth::Inactive, &["sdb2"]),
        ]);
        assert!(startable(&i, &records).is_empty());
        let i = inv(vec![array("md11", "t2", "raid1", 2, ArrayHealth::Inactive, &["sdb2"])]);
        assert!(startable(&i, &records).is_empty());
        // all running: nothing to do
        let i = inv(vec![
            array("md10", "t1", "raid5", 3, ArrayHealth::Healthy, &["sda1", "sdb1", "sdc1"]),
            array("md11", "t2", "raid1", 2, ArrayHealth::Healthy, &["sdb2", "sdc2"]),
        ]);
        assert!(startable(&i, &records).is_empty());
    }

    #[test]
    fn encrypted_or_mounted_volumes_are_not_mounted() {
        let i = inv(vec![array("md127", "AA:01", "raid1", 2, ArrayHealth::Inactive, &["sda1"])]);
        let s = startable(&i, &[classic("vol1", "AA:01", true)]);
        assert_eq!(s[0].mount_unit, None, "waits for its unlock");
        let mut i = i;
        i.mounts.push(crate::mounts::Mount { mountpoint: "/vol1".into(), ..Default::default() });
        assert_eq!(startable(&i, &[classic("vol1", "AA:01", false)])[0].mount_unit, None);
    }

    #[test]
    fn waits_for_uptime_and_a_stable_array() {
        let records = vec![classic("vol1", "AA:01", false)];
        let found = |m: &[&str]| {
            startable(&inv(vec![array("md127", "AA:01", "raid5", 4, ArrayHealth::Inactive, m)]), &records)
        };
        let mut r = Recovery::default();
        let t0 = 1_000_000;
        assert!(r.due(found(&["a", "b", "c"]), t0, 60).is_empty(), "first sighting");
        assert!(r.due(found(&["a", "b", "c"]), t0 + STABLE_MS, 60).is_empty(), "boot too recent");
        // a member joined meanwhile: the wait starts again
        assert!(r.due(found(&["a", "b", "c", "d"]), t0 + STABLE_MS, 300).is_empty());
        assert!(r.due(found(&["a", "b", "c", "d"]), t0 + STABLE_MS + 1000, 300).is_empty());
        let due = r.due(found(&["a", "b", "c", "d"]), t0 + 2 * STABLE_MS, 300);
        assert_eq!(due.len(), 1);
        r.forget(&due[0]);
        assert!(r.due(found(&["a", "b", "c", "d"]), t0 + 2 * STABLE_MS, 300).is_empty(), "forgotten: waits again");
        // gone (it started by itself): forgotten
        r.due(Vec::new(), t0 + 3 * STABLE_MS, 300);
        assert!(r.seen.is_empty());
    }

    #[test]
    fn start_runs_mdadm_lvm_and_the_mount() {
        let v = VolumeStart {
            label: "flex".into(),
            arrays: vec![Runnable {
                array: "md11".into(),
                path: "/dev/md11".into(),
                uuid: "t2".into(),
                level: "raid1".into(),
                present: 1,
                raid_disks: 2,
                members: vec!["sdb2".into()],
            }],
            vg: Some("rynas_flex".into()),
            mount_unit: Some("flex.mount".into()),
        };
        let mut h = FakeHost::default();
        h.cmd("mdadm --run /dev/md11", 0, "")
            .cmd("vgchange -ay rynas_flex", 0, "")
            .cmd("systemctl start flex.mount", 0, "");
        assert_eq!(start(&h, &v), Ok(true));
        assert_eq!(h.ran(), ["mdadm --run /dev/md11", "vgchange -ay rynas_flex", "systemctl start flex.mount"]);
        let mut h = FakeHost::default();
        h.commands.get_mut().unwrap().insert(
            "mdadm --run /dev/md11".into(),
            crate::host::CmdOutput {
                status: Some(1),
                stdout: String::new(),
                stderr: "mdadm: failed to start array /dev/md11: Input/output error\n".into(),
            },
        );
        let e = start(&h, &v).unwrap_err();
        assert!(e.contains("Input/output error"), "{e}");
        assert_eq!(h.ran(), ["mdadm --run /dev/md11"], "nothing after a failure");
        let mut h = FakeHost::default();
        h.file("/proc/uptime", "354.12 1200.50\n");
        assert_eq!(uptime_s(&h), 354);
    }
}
