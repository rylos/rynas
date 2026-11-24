//! The lock table of the scheduler (S4 of `docs/research/scheduling.md`): what holds which
//! resource now, and the questions other features ask it.
//!
//! The holders come from:
//! - the leases of the scheduling gate ([`crate::gate`]): every scheduled run it let through,
//!   with the claims of its action ([`claims_of`]);
//! - the filesystem lock ([`crate::fslock`]): heavy work started by hand (a backup or a
//!   replication run from its page, storage operations) and deduplication;
//! - rynas tasks: package work, maintenance (applying the configuration, rollbacks), any
//!   other work (only a shutdown waits for it);
//! - `/proc/mdstat`: an md resync, recovery, reshape or check holds the array's disks;
//! - the task units that run without a lease.
//!
//! The gate's queue ([`rynas_scheduler::queue`]) decides with it; Docker's automatic updates
//! ([`docker_busy`]) and the scheduled shutdown's checks ([`power_busy`]) ask it what keeps
//! them from going.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rynas_api_types::TaskStatus as UpidStatus;
use rynas_api_types::scheduler::{Blocker, Resource, ScheduledTask, SmartTestKind, TaskAction};
use rynas_scheduler::queue::{Claim, EVERY, Gives, Holder, priority};
use rynas_storage::mounts::{Mount, parse_mountinfo};

use crate::gate::Gate;
use crate::protection::Protection;

/// The volumes holding `paths` (their filesystem keys).
fn volumes(mounts: &[Mount], paths: &[String]) -> Vec<Claim> {
    let mut out: Vec<Claim> = paths
        .iter()
        .filter_map(|p| crate::fslock::mount_of(mounts, p).map(|m| Claim::Volume(m.majmin.clone())))
        .collect();
    out.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    out.dedup();
    out
}

/// What the work of task `t` claims, from its action (the user never sets it). Empty: the
/// task goes through the queue untouched (snapshots, user scripts, checks).
pub fn claims_of(p: &Protection, t: &ScheduledTask) -> Vec<Claim> {
    let mounts = || parse_mountinfo(&crate::places::mountinfo());
    match &t.action {
        TaskAction::Scrub { filesystem } => volumes(&mounts(), std::slice::from_ref(filesystem)),
        TaskAction::Backup { job } => volumes(&mounts(), &crate::restic_backup::job_source_paths(job)),
        TaskAction::Replicate { job, target } => volumes(&mounts(), &replication_paths(p, job, target)),
        TaskAction::PcMaintenance => volumes(&mounts(), &pc_repositories(p)),
        TaskAction::SmartTest { disk, test: SmartTestKind::Long } => vec![Claim::Disk(disk.clone())],
        TaskAction::MdCheck { .. } => {
            let text = std::fs::read_to_string("/proc/mdstat").unwrap_or_default();
            let mut d: Vec<String> = md_arrays(&text).into_iter().flat_map(|a| a.disks).collect();
            d.sort();
            d.dedup();
            d.into_iter().map(Claim::Disk).collect()
        }
        TaskAction::SystemUpgrade | TaskAction::DockerAutoUpdate | TaskAction::KernelUpdate => vec![Claim::Packages],
        TaskAction::PowerShutdown { .. } => vec![Claim::Power],
        _ => Vec::new(),
    }
}

/// How a task's run behaves when more urgent work wants what it holds.
pub fn gives_of(a: &TaskAction) -> Gives {
    match a {
        TaskAction::Scrub { .. } => Gives::Yields,
        TaskAction::SmartTest { .. } | TaskAction::MdCheck { .. } => Gives::Throttles,
        TaskAction::Dedup { .. } => Gives::Dedup,
        _ => Gives::No,
    }
}

/// The rynas task type of a task's work in rynasd (to tell its filesystem lock from work
/// started by hand).
pub fn work_kind(a: &TaskAction) -> Option<&'static str> {
    match a {
        TaskAction::Backup { .. } => Some("backup.run"),
        TaskAction::Replicate { .. } => Some("replication.run"),
        TaskAction::Scrub { .. } => Some("btrfs.scrub"),
        _ => None,
    }
}

/// The source volume of a replica (and a local target's).
fn replication_paths(p: &Protection, job: &str, target: &str) -> Vec<String> {
    let Ok(doc) = p
        .ops
        .config
        .applied(rynas_btrfs::SNAPSHOTS_KEY)
        .map_err(|e| e.to_string())
        .and_then(|v| crate::services::from_value::<rynas_api_types::snapshots::SnapshotsConfig>(v).map_err(|e| e.2))
    else {
        return Vec::new();
    };
    let Some(j) = doc.jobs.iter().find(|j| j.name == job) else {
        return Vec::new();
    };
    let mut out = vec![j.subvolume.clone()];
    if let Some(t) = j.replicas.iter().find(|r| r.id == target)
        && let rynas_api_types::replication::ReplicaDestination::Local { path } = &t.destination
    {
        out.push(path.clone());
    }
    out
}

/// The PC backup repositories (the nightly maintenance works on every group's).
fn pc_repositories(p: &Protection) -> Vec<String> {
    p.ops
        .pc_agent
        .get()
        .and_then(std::sync::Weak::upgrade)
        .map(|s| s.snapshot().groups.values().map(|g| g.repo.clone()).collect())
        .unwrap_or_default()
}

/// Rynas task types of package work (one at a time with the system updates).
/// Every rynas task that runs apt or dpkg (their lock), and the Docker and app updates.
const PACKAGE_KINDS: &[&str] = &[
    "updates.install",
    "updates.autoremove",
    "updates.backports",
    "docker.install",
    "docker.auto-update",
    "apps.auto-update",
    "apps.update",
    "syncthing.adopt",
    "tailscale.install",
    "tailscale.uninstall",
    "ups.install",
    "ups.update-nut",
    "crypt.install",
    "dedup.install",
    "time.apply",
];

/// What a drive says about its self-test (`smartctl -c -l selftest`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SelfTest {
    /// A self-test runs now (`None`: the output does not say).
    pub in_progress: Option<bool>,
    /// The drive's estimate of its extended self-test (minutes).
    pub extended_min: Option<u64>,
}

/// Reads `smartctl -c -l selftest`: ATA ("Self-test execution status: ( 249) Self-test routine
/// in progress", the status byte's high nibble 15 while one runs; "Extended self-test routine
/// recommended polling time: ( 255) minutes") and NVMe ("Self-test status: Extended
/// self-test in progress (12% completed)", "No self-test in progress").
pub fn parse_self_test(out: &str) -> SelfTest {
    let paren = |l: &str| -> Option<u64> {
        let a = l.find('(')?;
        let b = l[a..].find(')')? + a;
        l[a + 1..b].trim().parse().ok()
    };
    let mut st = SelfTest::default();
    let mut lines = out.lines().peekable();
    while let Some(l) = lines.next() {
        let t = l.trim();
        if t.starts_with("Self-test execution status:") {
            st.in_progress = match paren(t) {
                Some(v) => Some(v >> 4 == 0xF),
                None => Some(t.contains("in progress")),
            };
        } else if t.starts_with("Self-test status:") || t.starts_with("No self-test in progress") {
            st.in_progress = Some(t.contains("in progress") && !t.starts_with("No self-test"));
        } else if t.starts_with("Extended self-test routine") {
            let next = if t.contains("polling time") { Some(t) } else { lines.peek().copied() };
            st.extended_min = next.and_then(paren);
        }
    }
    st
}

/// Maintenance work automatic updates wait for.
const MAINTENANCE_KINDS: &[&str] = &[
    "snapshot.rollback",
    "snapshot.restore",
    "ids.remap",
    "homes.move",
    "config.apply",
    "crypt.",
    "docker.migrate",
    "power.",
];

/// Heavy storage work (its filesystem lock, when it has one, is in the table too).
const HEAVY_KINDS: &[&str] = &[
    "storage.",
    "btrfs.scrub",
    "replication.",
    "backup.",
    "pcbackup.maintenance",
    "pcbackup.prune",
];

fn starts(kind: &str, list: &[&str]) -> bool {
    list.iter().any(|k| kind.starts_with(k))
}

/// The priority of heavy work started by hand (its rynas task type).
fn kind_priority(kind: &str) -> u8 {
    if kind.starts_with("backup.") || kind.starts_with("replication.") {
        priority::TRANSFER
    } else if kind.starts_with("pcbackup.") {
        priority::PC_MAINTENANCE
    } else if kind.contains("scrub") {
        priority::SCRUB
    } else if kind == "storage.dedup" {
        priority::DEDUP
    } else {
        // storage operations: a grow or a replace goes before everything scheduled
        priority::SNAPSHOT
    }
}

/// An md array of `/proc/mdstat`: its member disks and the operation going on, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdArray {
    pub name: String,
    pub disks: Vec<String>,
    /// `resync`, `recovery`, `reshape`, `check` (with its progress) when one goes on.
    pub busy: Option<String>,
}

fn parent_disk(sys: &Path, name: &str) -> String {
    let dev = sys.join("class/block").join(name);
    if dev.exists() {
        if dev.join("partition").exists()
            && let Ok(real) = std::fs::canonicalize(&dev)
            && let Some(parent) = real.parent().and_then(|p| p.file_name())
        {
            return parent.to_string_lossy().into_owned();
        }
        return name.to_string();
    }
    // no sysfs entry (tests, a disk gone): by the naming rules of the kernel
    let base = name.trim_end_matches(|c: char| c.is_ascii_digit());
    if base.len() == name.len() {
        return name.to_string();
    }
    if name.starts_with("nvme") || name.starts_with("mmcblk") {
        return base.strip_suffix('p').map_or(name, |b| b).to_string();
    }
    if ["sd", "vd", "hd", "xvd"].iter().any(|p| name.starts_with(p)) {
        return base.to_string();
    }
    name.to_string()
}

/// The md arrays of a `/proc/mdstat` text.
pub fn md_arrays(text: &str) -> Vec<MdArray> {
    md_arrays_in(Path::new("/sys"), text)
}

fn md_arrays_in(sys: &Path, text: &str) -> Vec<MdArray> {
    let busy: HashMap<String, String> = rynas_power::policy::mdstat_ops(text)
        .into_iter()
        .map(|m| {
            let sep = if m.progress.is_empty() { "" } else { " " };
            (m.array.clone(), format!("{}{sep}{}", m.op, m.progress))
        })
        .collect();
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(" : ") else { continue };
        let name = name.trim();
        if line.starts_with(' ') || !name.starts_with("md") {
            continue;
        }
        let mut disks: Vec<String> = rest
            .split_whitespace()
            .filter_map(|w| w.split_once('['))
            .map(|(dev, _)| parent_disk(sys, dev))
            .collect();
        disks.sort();
        disks.dedup();
        out.push(MdArray { name: name.to_string(), disks, busy: busy.get(name).cloned() });
    }
    out
}

/// The disks under a block device: its slaves (md, LVM, LUKS) down to the disks.
fn leaf_disks(sys: &Path, name: &str, depth: u8, out: &mut Vec<String>) {
    let slaves = sys.join("class/block").join(name).join("slaves");
    let mut found = false;
    if depth < 8
        && let Ok(rd) = std::fs::read_dir(&slaves)
    {
        for e in rd.flatten() {
            found = true;
            leaf_disks(sys, &e.file_name().to_string_lossy(), depth + 1, out);
        }
    }
    if !found {
        out.push(parent_disk(sys, name));
    }
}

/// The disks of the filesystem mounted from `source` (every device of a btrfs filesystem).
fn disks_of_mount(sys: &Path, m: &Mount) -> Vec<String> {
    let mut names = Vec::new();
    let dev = std::fs::canonicalize(&m.source).unwrap_or_else(|_| PathBuf::from(&m.source));
    if let Some(n) = dev.file_name() {
        names.push(n.to_string_lossy().into_owned());
    }
    if m.fstype == "btrfs"
        && let Ok(rd) = std::fs::read_dir(sys.join("fs/btrfs"))
    {
        for fs in rd.flatten() {
            let devs: Vec<String> = std::fs::read_dir(fs.path().join("devices"))
                .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
                .unwrap_or_default();
            if names.iter().any(|n| devs.contains(n)) {
                names.extend(devs);
            }
        }
    }
    let mut out = Vec::new();
    for n in names {
        leaf_disks(sys, &n, 0, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

/// Where a holder comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A scheduled run the gate let through (its unit).
    Lease { unit: String },
    /// The filesystem lock (work started by hand, deduplication).
    FsLock,
    /// A rynas task.
    Task { upid: String, running: bool, heavy: bool },
    /// An md operation in `/proc/mdstat` (its text).
    Raid { text: String },
    /// A task unit running without a lease.
    Unit { unit: String },
    /// A SMART long test running in its drive after its unit ended.
    DiskTest,
}

/// The lock table at one moment.
pub struct Table {
    pub holders: Vec<Holder>,
    pub sources: Vec<Source>,
    mounts: Vec<Mount>,
    sys: PathBuf,
    disks: Mutex<HashMap<String, Vec<String>>>,
}

impl Table {
    /// The disks under a volume key.
    pub fn disks(&self, key: &str) -> Vec<String> {
        if let Some(d) = self.disks.lock().unwrap().get(key) {
            return d.clone();
        }
        let d = self
            .mounts
            .iter()
            .find(|m| m.majmin == key)
            .map(|m| disks_of_mount(&self.sys, m))
            .unwrap_or_default();
        self.disks.lock().unwrap().insert(key.to_string(), d.clone());
        d
    }

    /// The resource a claim names, for people (a volume by its mount point).
    pub fn resource(&self, c: &Claim) -> Resource {
        match c {
            Claim::Volume(k) if k == EVERY => Resource::Volume { path: EVERY.into() },
            Claim::Volume(k) => Resource::Volume {
                path: self
                    .mounts
                    .iter()
                    .filter(|m| m.majmin == *k)
                    .map(|m| m.mountpoint.clone())
                    .min_by_key(|p| p.len())
                    .unwrap_or_else(|| k.clone()),
            },
            Claim::Disk(d) => Resource::Disk { disk: d.clone() },
            Claim::Packages => Resource::Packages,
            Claim::Power => Resource::Power,
            Claim::Maintenance | Claim::Busy => Resource::Maintenance,
        }
    }

    /// A holder as people see it, holding `claim`.
    pub fn blocker(&self, h: &Holder, claim: &Claim) -> Blocker {
        Blocker { task: h.task.clone(), name: h.name.clone(), kind: h.kind.clone(), resource: self.resource(claim) }
    }

    /// The holders (other than the task `me`) whose claims meet one of `claims`: what a start
    /// with them would wait for or slow down.
    pub fn meeting(&self, me: &str, claims: &[Claim]) -> Vec<Blocker> {
        let disks = |k: &str| self.disks(k);
        let mut out: Vec<Blocker> = Vec::new();
        for h in &self.holders {
            if h.task.as_deref() == Some(me) || h.gives == Gives::Dedup {
                continue;
            }
            if let Some(c) = claims.iter().find(|c| h.claims.iter().any(|x| rynas_scheduler::queue::conflict(c, x, &disks))) {
                let b = self.blocker(h, c);
                if !out.contains(&b) {
                    out.push(b);
                }
            }
        }
        out
    }
}

/// Builds the lock table: the gate's leases, the filesystem lock, rynas tasks, `mdstat` (its
/// text) and the running task `units` (systemd's list).
pub fn table(gate: Option<&Gate>, tasks: &rynas_tasks::TaskManager, mdstat: &str, units: &[String]) -> Table {
    table_in(gate, tasks, mdstat, units, &crate::places::mountinfo(), Path::new("/sys"))
}

pub(crate) fn table_in(
    gate: Option<&Gate>,
    tasks: &rynas_tasks::TaskManager,
    mdstat: &str,
    units: &[String],
    mountinfo: &str,
    sys: &Path,
) -> Table {
    let mut holders = Vec::new();
    let mut sources = Vec::new();
    let leases = gate.map(|g| g.lease_views()).unwrap_or_default();
    let guard_ids: Vec<u64> = leases.iter().filter_map(|l| l.guard).collect();
    // the filesystem locks taken by the leases' own work in rynasd: (kind, key)
    let mut own: Vec<(String, String)> = leases
        .iter()
        .filter_map(|l| l.kind.map(|k| (k, &l.holder.claims)))
        .flat_map(|(k, claims)| {
            claims.iter().filter_map(move |c| match c {
                Claim::Volume(key) => Some((k.to_string(), key.clone())),
                _ => None,
            })
        })
        .collect();
    let mut leased_units = Vec::new();
    for l in &leases {
        let mut h = l.holder.clone();
        h.claims.push(Claim::Busy);
        holders.push(h);
        leased_units.push(l.unit.clone());
        sources.push(Source::Lease { unit: l.unit.clone() });
    }
    for d in gate.map(|g| g.disk_tests()).unwrap_or_default() {
        holders.push(Holder {
            task: Some(d.task.clone()),
            name: d.name.clone(),
            kind: None,
            claims: vec![Claim::Disk(d.disk.clone())],
            priority: priority::DISK_TEST,
            gives: Gives::Throttles,
        });
        sources.push(Source::DiskTest);
    }
    for (key, id, what, dedup) in crate::fslock::FsLocks::global().holders() {
        if guard_ids.contains(&id) {
            continue;
        }
        if let Some(i) = own.iter().position(|(k, v)| *k == what && *v == key) {
            own.remove(i);
            continue;
        }
        holders.push(Holder {
            task: None,
            name: String::new(),
            kind: Some(what.clone()),
            claims: vec![Claim::Volume(key)],
            priority: kind_priority(&what),
            gives: if dedup { Gives::Dedup } else { Gives::No },
        });
        sources.push(Source::FsLock);
    }
    for t in tasks.list(500) {
        let running = t.status == UpidStatus::Running;
        if !(running || t.status == UpidStatus::Queued) || crate::dedup::gives_way(&t.kind) {
            continue;
        }
        let mut claims = Vec::new();
        if starts(&t.kind, PACKAGE_KINDS) {
            claims.push(Claim::Packages);
        } else if starts(&t.kind, MAINTENANCE_KINDS) {
            claims.push(Claim::Maintenance);
        }
        claims.push(Claim::Busy);
        holders.push(Holder {
            task: None,
            name: String::new(),
            kind: Some(t.kind.clone()),
            claims,
            priority: kind_priority(&t.kind),
            gives: Gives::No,
        });
        sources.push(Source::Task { upid: t.id.clone(), running, heavy: starts(&t.kind, HEAVY_KINDS) });
    }
    for a in md_arrays_in(sys, mdstat) {
        let Some(op) = a.busy else { continue };
        holders.push(Holder {
            task: None,
            name: a.name.clone(),
            kind: Some(format!("raid.{}", op.split_whitespace().next().unwrap_or("check"))),
            claims: a.disks.iter().cloned().map(Claim::Disk).collect(),
            priority: priority::DISK_TEST,
            gives: Gives::Throttles,
        });
        sources.push(Source::Raid { text: format!("{}: RAID {op}", a.name) });
    }
    // a start that waits in its gate (the queue, the maintenance window) runs nothing and
    // holds nothing: never "busy" (a scheduled shutdown does not wait for it)
    let waiting: Vec<String> = gate
        .map(|g| g.queued().into_iter().map(|(id, _)| rynas_scheduler::units::service_of(&id)).collect())
        .unwrap_or_default();
    for u in units.iter().filter(|u| !leased_units.contains(u) && !waiting.contains(u)) {
        let id = u
            .strip_prefix(rynas_scheduler::units::UNIT_PREFIX)
            .and_then(|s| s.strip_suffix(".service"))
            .map(str::to_string);
        holders.push(Holder {
            task: id,
            name: String::new(),
            kind: None,
            claims: vec![Claim::Busy],
            priority: priority::OTHER,
            gives: if crate::dedup::gives_way(u) { Gives::Dedup } else { Gives::No },
        });
        sources.push(Source::Unit { unit: u.clone() });
    }
    Table {
        holders,
        sources,
        mounts: parse_mountinfo(mountinfo),
        sys: sys.to_path_buf(),
        disks: Mutex::new(HashMap::new()),
    }
}

pub fn docker_busy(t: &Table) -> Option<String> {
    busy_except(
        t,
        rynas_api_types::scheduler::system::DOCKER_AUTO_UPDATE,
        &["docker.auto-update", "apps.auto-update", "docker.auto-window"],
    )
}

/// What keeps an automatic job (Docker updates, the kernel installation) from starting: the
/// same as [`docker_busy`], leaving out the job's own scheduler task and task kinds.
pub fn busy_except(t: &Table, own_task: &str, own_kinds: &[&str]) -> Option<String> {
    for (h, s) in t.holders.iter().zip(&t.sources) {
        if h.gives == Gives::Dedup
            || h.task.as_deref() == Some(own_task)
            || h.kind.as_deref().is_some_and(|k| own_kinds.contains(&k))
        {
            continue;
        }
        let claims = h.claims.iter().any(|c| !matches!(c, Claim::Busy));
        match s {
            Source::Raid { text } => return Some(format!("RAID: {text}")),
            Source::Lease { .. } if claims => {
                return Some(format!("scheduled task {} is running", h.task.as_deref().unwrap_or_default()));
            }
            Source::FsLock => return Some(format!("task {} is running", h.kind.as_deref().unwrap_or_default())),
            Source::Task { heavy, .. } if *heavy || claims => {
                return Some(format!("task {} is running", h.kind.as_deref().unwrap_or_default()));
            }
            _ => {}
        }
    }
    None
}

/// What keeps a scheduled shutdown waiting: `storage` (md operations, scrubs) and `tasks`
/// (any other work of rynas and running task units, deduplication left out: it stops and
/// saves its work). `me`: the shutdown's own rynas task, `unit`: its own unit.
pub fn power_busy(t: &Table, storage: bool, tasks: bool, me: &str, unit: &str) -> Vec<String> {
    let mut busy = Vec::new();
    for (h, s) in t.holders.iter().zip(&t.sources) {
        match s {
            Source::Raid { text } if storage => busy.push(text.clone()),
            Source::Task { upid, running: true, .. } => {
                let kind = h.kind.as_deref().unwrap_or_default();
                if storage && kind.contains("scrub") {
                    busy.push(format!("{kind} running ({upid})"));
                } else if tasks && upid != me && !kind.contains("scrub") {
                    busy.push(format!("rynas task {kind} running"));
                }
            }
            Source::Lease { unit: u } | Source::Unit { unit: u }
                if tasks && u != unit && !crate::dedup::gives_way(u) && h.gives != Gives::Dedup =>
            {
                busy.push(format!("scheduler task {u} running"));
            }
            _ => {}
        }
    }
    busy
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md_arrays_and_their_disks() {
        let text = "Personalities : [raid1] [raid6]\n\
md2 : active raid1 sdb3[1] sda3[0]\n      1000 blocks super 1.2 [2/2] [UU]\n      [==>....]  check = 12.5% (1/8) finish=10min speed=100K/sec\n\n\
md127 : active raid5 nvme0n1p2[0] nvme1n1p2[1] sdc[2](F)\n      2000 blocks [3/2] [UU_]\n\nunused devices: <none>\n";
        let none = Path::new("/nonexistent-sys");
        let a = md_arrays_in(none, text);
        assert_eq!(a[0], MdArray { name: "md2".into(), disks: vec!["sda".into(), "sdb".into()], busy: Some("check 12.5%".into()) });
        assert_eq!(a[1].disks, ["nvme0n1", "nvme1n1", "sdc"]);
        assert_eq!(a[1].busy, None);
    }

    #[test]
    fn disks_from_sysfs() {
        let d = tempfile::tempdir().unwrap();
        let sys = d.path();
        // md2 over sda3 and sdb3; sda3 is a partition of sda
        for (dev, parent) in [("sda", None), ("sda3", Some("sda")), ("sdb", None), ("sdb3", Some("sdb")), ("md2", None)] {
            let path = match parent {
                Some(p) => sys.join("devices/block").join(p).join(dev),
                None => sys.join("devices/block").join(dev),
            };
            std::fs::create_dir_all(&path).unwrap();
            if parent.is_some() {
                std::fs::write(path.join("partition"), "3").unwrap();
            }
            std::fs::create_dir_all(sys.join("class/block")).unwrap();
            std::os::unix::fs::symlink(&path, sys.join("class/block").join(dev)).unwrap();
        }
        std::fs::create_dir_all(sys.join("devices/block/md2/slaves")).unwrap();
        for s in ["sda3", "sdb3"] {
            std::fs::write(sys.join("devices/block/md2/slaves").join(s), "").unwrap();
        }
        let m = Mount {
            majmin: "0:40".into(),
            root: "/".into(),
            mountpoint: "/vol1".into(),
            fstype: "btrfs".into(),
            source: "/dev/md2".into(),
            options: vec![],
        };
        assert_eq!(disks_of_mount(sys, &m), ["sda", "sdb"]);
        assert_eq!(parent_disk(sys, "sda3"), "sda");
    }

    #[test]
    fn the_table_answers_docker_and_the_shutdown() {
        let d = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _g = rt.enter();
        let tasks = rynas_tasks::TaskManager::new(d.path().join("t"), "nas", Default::default()).unwrap();
        let mdstat = "md2 : active raid1 sdb3[1] sda3[0]\n      [==>..]  resync = 3.0% (1/8)\n";
        let t = table_in(None, &tasks, mdstat, &["rynas-task-x.service".into(), "rynas-task-dedup-vol1.service".into()], "", Path::new("/nonexistent"));
        assert_eq!(docker_busy(&t).as_deref(), Some("RAID: md2: RAID resync 3.0%"));
        assert_eq!(power_busy(&t, true, false, "", ""), ["md2: RAID resync 3.0%"]);
        assert_eq!(power_busy(&t, false, true, "", "rynas-task-p.service"), ["scheduler task rynas-task-x.service running"]);
        let quiet = table_in(None, &tasks, "", &[], "", Path::new("/nonexistent"));
        assert_eq!(docker_busy(&quiet), None);
        assert!(power_busy(&quiet, true, true, "", "").is_empty());
    }

    #[test]
    fn drives_tell_whether_their_long_test_runs() {
        let ata = "General SMART Values:\nOffline data collection status:  (0x00)\tOffline data collection activity\n\
Self-test execution status:      ( 249)\tSelf-test routine in progress...\n\t\t\t\t\t90% of test remaining.\n\
Short self-test routine \nrecommended polling time: \t (   2) minutes.\nExtended self-test routine\nrecommended polling time: \t ( 255) minutes.\n";
        assert_eq!(parse_self_test(ata), SelfTest { in_progress: Some(true), extended_min: Some(255) });
        let done = ata.replace("( 249)\tSelf-test routine in progress...", "(   0)\tThe previous self-test routine completed");
        assert_eq!(parse_self_test(&done).in_progress, Some(false));
        let nvme = "Self-test status: Extended self-test in progress (12% completed)\n";
        assert_eq!(parse_self_test(nvme).in_progress, Some(true));
        assert_eq!(parse_self_test("No self-test in progress\n").in_progress, Some(false));
        assert_eq!(parse_self_test(""), SelfTest::default());
    }

    #[test]
    fn every_apt_and_dpkg_user_holds_the_package_system() {
        for k in ["updates.install", "updates.autoremove", "updates.backports", "tailscale.install", "ups.update-nut", "dedup.install", "crypt.install"] {
            assert!(starts(k, PACKAGE_KINDS), "{k}");
        }
        assert!(!starts("updates.check", PACKAGE_KINDS));
    }
}
