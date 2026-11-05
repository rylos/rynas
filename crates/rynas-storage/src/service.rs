//! The storage service: runs the tools through a [`Host`], caches what is expensive or would
//! wake a disk, and assembles the API views. Blocking: call it from a blocking thread.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rynas_api_types::storage::{
    BtrfsFilesystem, DiskDetail, DiskKind, DiskSummary, GrowPlanRequest, MdArray, NodeKind,
    PowerState, ReplacePlanRequest, ScrubPlanRequest, StoragePlan, StorageTree, Subvolume,
    TreeNode, VolumePlanRequest,
};

use crate::Inventory;
use crate::btrfs::{self, Usage};
use crate::host::{CmdOutput, Host};
use crate::inventory::{self, FsParts, SmartEntry, disk_kind};
use crate::lsblk::{BlockDev, BlockGraph, LSBLK_ARGS};
use crate::md::{self, Detail, Export};
use crate::mounts::{Mount, parse_mountinfo};
use crate::smart::{self, SmartRead};
use crate::tuning::{self, TuningInput};
use crate::{luks, plan};

const T_LSBLK: Duration = Duration::from_secs(10);
const T_SMART: Duration = Duration::from_secs(30);
const T_MDADM: Duration = Duration::from_secs(15);
const T_BTRFS: Duration = Duration::from_secs(60);
const T_CRYPT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq)]
pub enum StorageError {
    /// A required tool failed (lsblk): the view cannot be built.
    Tool(String),
    NotFound(String),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageError::Tool(m) => write!(f, "{m}"),
            StorageError::NotFound(m) => write!(f, "{m}: not found"),
        }
    }
}

#[derive(Debug, Clone)]
struct MdCache {
    read_ms: u64,
    export: Option<Export>,
    detail: Option<Detail>,
}

#[derive(Debug, Clone)]
struct FsCache {
    read_ms: u64,
    usage: Option<Usage>,
    subvolumes: Vec<Subvolume>,
    readonly: HashSet<u64>,
}

#[derive(Default)]
struct Cache {
    smart: HashMap<String, SmartEntry>,
    md: HashMap<String, MdCache>,
    fs: HashMap<String, FsCache>,
    /// The last full inventory and when it was taken (ms): shared by the list views, the
    /// tree and the planners for [`Storage::inventory_max_age`].
    inventory: Option<(u64, Arc<Inventory>)>,
}

pub struct Storage {
    host: Arc<dyn Host>,
    /// SMART is read at most this often per disk (and never from a disk in standby).
    pub smart_max_age: Duration,
    /// `mdadm --detail` and the btrfs usage/subvolume walk are refreshed at most this often.
    pub detail_max_age: Duration,
    /// A full inventory (lsblk, power checks, mdstat, btrfs) is reused this long.
    pub inventory_max_age: Duration,
    /// Attached loop devices count as disks: tests of the write side on loop files only.
    pub loop_disks: bool,
    cache: Mutex<Cache>,
}

fn asleep(p: PowerState) -> bool {
    matches!(p, PowerState::Standby | PowerState::Sleep)
}

impl Storage {
    pub fn new(host: Arc<dyn Host>) -> Self {
        Self {
            host,
            smart_max_age: Duration::from_secs(600),
            detail_max_age: Duration::from_secs(60),
            inventory_max_age: Duration::from_secs(5),
            loop_disks: false,
            cache: Mutex::new(Cache::default()),
        }
    }

    fn run(&self, prog: &str, args: &[&str], t: Duration) -> Option<CmdOutput> {
        match self.host.run(prog, args, t) {
            Ok(o) => Some(o),
            Err(e) => {
                tracing::debug!("{prog} {args:?}: {e}");
                None
            }
        }
    }

    /// stdout of a command that must succeed.
    fn run_ok(&self, prog: &str, args: &[&str], t: Duration) -> Option<String> {
        let o = self.run(prog, args, t)?;
        if o.success() {
            Some(o.stdout)
        } else {
            tracing::debug!("{prog} {args:?}: exit {:?}: {}", o.status, o.stderr.trim());
            None
        }
    }

    pub fn graph(&self) -> Result<BlockGraph, StorageError> {
        let out = self
            .run_ok("lsblk", LSBLK_ARGS, T_LSBLK)
            .ok_or_else(|| StorageError::Tool("lsblk failed".into()))?;
        let mut g = BlockGraph::parse(&out).map_err(StorageError::Tool)?;
        if self.loop_disks {
            g.promote_loops();
        }
        Ok(g)
    }

    /// Forgets the cached inventory and the md/btrfs details after a change made by rynas
    /// (the next view reads the system again). SMART stays cached.
    pub fn invalidate(&self) {
        let mut c = self.cache.lock().unwrap();
        c.inventory = None;
        c.md.clear();
        c.fs.clear();
    }

    /// Re-reads APM and write cache of the ATA disks after the disk settings were applied
    /// (`smartctl -n standby -g apm -g wcache`, no SMART data), so the views show the new
    /// values at once instead of after the next SMART read. Only disks with cached SMART are
    /// read (the others get a full read on their next view), and never a disk that sleeps:
    /// its power mode is checked first and smartctl gives up by itself if it went to sleep
    /// meanwhile. Returns the disks that were read.
    pub fn refresh_disk_settings(&self) -> Vec<String> {
        let Ok(g) = self.graph() else {
            return Vec::new();
        };
        let cached: HashSet<String> = self.cache.lock().unwrap().smart.keys().cloned().collect();
        let mut read = Vec::new();
        for d in g.disks() {
            if !cached.contains(&d.kname)
                || disk_kind(d) == DiskKind::Nvme
                || asleep(self.power_of(d))
            {
                continue;
            }
            let mut args = smart::SETTINGS_ARGS.to_vec();
            args.push(&d.path);
            let Some(out) = self.run("smartctl", &args, T_SMART) else {
                continue;
            };
            if let smart::SettingsRead::Data { apm, write_cache } =
                smart::parse_settings(&out.stdout)
            {
                let mut c = self.cache.lock().unwrap();
                if let Some(e) = c.smart.get_mut(&d.kname) {
                    e.data.apm = apm;
                    e.data.write_cache = write_cache;
                    read.push(d.kname.clone());
                }
            }
        }
        // the list views come from the inventory: build it again with the new values
        self.cache.lock().unwrap().inventory = None;
        read
    }

    /// The machine the storage runs on.
    pub fn host(&self) -> &Arc<dyn Host> {
        &self.host
    }

    fn mounts(&self) -> Vec<Mount> {
        parse_mountinfo(&self.host.read("/proc/self/mountinfo").unwrap_or_default())
    }

    fn power_of(&self, d: &BlockDev) -> PowerState {
        if disk_kind(d) == DiskKind::Nvme {
            return PowerState::Active;
        }
        self.host.ata_power(&d.path).unwrap_or(PowerState::Unknown)
    }

    fn zoned(&self, kname: &str) -> bool {
        self.host
            .read(&format!("/sys/block/{kname}/queue/zoned"))
            .is_some_and(|z| {
                let z = z.trim();
                !z.is_empty() && z != "none"
            })
    }

    /// SMART for a disk: from the cache when fresh or when the disk sleeps, else from
    /// smartctl (which still checks the power mode itself). Updates `power` with what
    /// smartctl saw.
    fn smart_for(&self, d: &BlockDev, power: &mut PowerState) -> Option<SmartEntry> {
        let now = self.host.now_ms();
        let serial = d.serial.clone().unwrap_or_default();
        let cached = {
            let c = self.cache.lock().unwrap();
            c.smart
                .get(&d.kname)
                .filter(|e| e.serial == serial)
                .cloned()
        };
        if let Some(c) = &cached
            && now.saturating_sub(c.data.info.read_ms) < self.smart_max_age.as_millis() as u64
        {
            return cached;
        }
        if asleep(*power) {
            return cached;
        }
        let nvme = disk_kind(d) == DiskKind::Nvme;
        let mut args: Vec<&str> = smart::smartctl_args(nvme).to_vec();
        args.push(&d.path);
        let out = self.run("smartctl", &args, T_SMART)?;
        match smart::parse(&out.stdout, now) {
            SmartRead::Data(data) => {
                if *power == PowerState::Unknown {
                    *power = PowerState::Active;
                }
                let e = SmartEntry {
                    data: *data,
                    serial,
                };
                self.cache
                    .lock()
                    .unwrap()
                    .smart
                    .insert(d.kname.clone(), e.clone());
                Some(e)
            }
            SmartRead::Asleep(p) => {
                *power = p;
                cached
            }
            SmartRead::Failed(m) => {
                tracing::debug!("smartctl {}: {m}", d.path);
                cached
            }
        }
    }

    fn summaries(&self, g: &BlockGraph) -> Vec<(DiskSummary, Option<SmartEntry>)> {
        let disks = g.disks();
        let now = self.host.now_ms();
        std::thread::scope(|s| {
            let handles: Vec<_> = disks
                .iter()
                .map(|d| {
                    s.spawn(move || {
                        let mut power = self.power_of(d);
                        let smart = self.smart_for(d, &mut power);
                        let zoned = self.zoned(&d.kname);
                        (
                            inventory::summary(d, g, smart.as_ref(), power, now, zoned),
                            smart,
                        )
                    })
                })
                .collect();
            handles.into_iter().filter_map(|h| h.join().ok()).collect()
        })
    }

    /// `GET /api/v1/storage/disks`
    pub fn disks(&self) -> Result<Vec<DiskSummary>, StorageError> {
        let g = self.graph()?;
        Ok(self.summaries(&g).into_iter().map(|(s, _)| s).collect())
    }

    /// `GET /api/v1/storage/disks/{name}`
    pub fn disk(&self, name: &str) -> Result<DiskDetail, StorageError> {
        let g = self.graph()?;
        let d = g
            .get(name)
            .filter(|d| d.is_disk() && !crate::lsblk::is_virtual_disk(&d.kname))
            .ok_or_else(|| StorageError::NotFound(name.to_string()))?;
        let now = self.host.now_ms();
        let mut power = self.power_of(d);
        let smart = self.smart_for(d, &mut power);
        let summary = inventory::summary(d, &g, smart.as_ref(), power, now, self.zoned(name));

        let mut ti = TuningInput {
            kind: summary.kind,
            transport: summary.transport.clone(),
            temperature_c: summary.temperature_c,
            ..Default::default()
        };
        tuning::read_sysfs(self.host.as_ref(), name, &mut ti);
        if let Some(e) = &smart {
            ti.write_cache = e.data.write_cache;
            ti.apm = e.data.apm;
            ti.smart_read = true;
            let i = &e.data.info;
            ti.power_on_hours = i.power_on_hours;
            ti.load_cycles = i.attributes.iter().find(|a| a.id == 193).map(|a| a.raw);
        }
        let above = g.above(name);
        // md members and btrfs (on the disk or a partition) read ahead on their own device
        ti.stacked = above.iter().any(|x| x.kind.starts_with("raid") || x.kind == "linear")
            || g.get(name)
                .into_iter()
                .chain(above.iter().copied())
                .any(|x| x.fstype.as_deref() == Some("btrfs"));
        // an encrypted mapping that drops discards blocks TRIM for the whole stack
        ti.crypt_blocks_discard = above.iter().any(|x| {
            x.kind == "crypt"
                && self
                    .host
                    .read(&format!("/sys/block/{}/queue/discard_max_bytes", x.kname))
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    == Some(0)
        });
        let mut on_disk: HashSet<String> = g.above(name).iter().map(|x| x.kname.clone()).collect();
        on_disk.insert(name.to_string());
        ti.online_discard = self.mounts().iter().any(|m| {
            g.find(&m.source)
                .is_some_and(|b| on_disk.contains(&b.kname))
                && m.options
                    .iter()
                    .any(|o| o == "discard" || o.starts_with("discard="))
        });
        ti.under_parity_raid = g
            .above(name)
            .iter()
            .any(|x| matches!(x.kind.as_str(), "raid4" | "raid5" | "raid6"));
        Ok(DiskDetail {
            settings: tuning::checks(&ti),
            partitions: inventory::partitions(&g, name),
            smart: smart.map(|e| e.data.info),
            summary,
        })
    }

    fn arrays_with(&self, g: &BlockGraph) -> Vec<MdArray> {
        let stat = md::parse_mdstat(&self.host.read("/proc/mdstat").unwrap_or_default());
        let now = self.host.now_ms();
        let max_age = self.detail_max_age.as_millis() as u64;
        stat.iter()
            .map(|st| {
                let sys = md::read_sysfs(self.host.as_ref(), &st.name);
                let member_disks: HashSet<String> = st
                    .members
                    .iter()
                    .flat_map(|m| g.disks_under(&m.device))
                    .collect();
                let sleeping = member_disks
                    .iter()
                    .filter_map(|d| g.get(d))
                    .any(|d| asleep(self.power_of(d)));
                let cached = self.cache.lock().unwrap().md.get(&st.name).cloned();
                let fresh = cached
                    .as_ref()
                    .is_some_and(|c| now.saturating_sub(c.read_ms) < max_age);
                let mut stale = false;
                let entry = if fresh || !st.active {
                    cached
                } else if sleeping {
                    stale = true;
                    cached
                } else {
                    let dev = format!("/dev/{}", st.name);
                    let export = self
                        .run_ok("mdadm", &["--detail", "--export", &dev], T_MDADM)
                        .map(|s| md::parse_export(&s));
                    let detail = self
                        .run_ok("mdadm", &["--detail", &dev], T_MDADM)
                        .map(|s| md::parse_detail(&s));
                    let c = MdCache {
                        read_ms: now,
                        export,
                        detail,
                    };
                    self.cache
                        .lock()
                        .unwrap()
                        .md
                        .insert(st.name.clone(), c.clone());
                    Some(c)
                };
                let disk_of = |dev: &str| g.disks_under(dev).into_iter().next();
                md::build(
                    st,
                    &sys,
                    entry.as_ref().and_then(|c| c.export.as_ref()),
                    entry.as_ref().and_then(|c| c.detail.as_ref()),
                    stale,
                    &disk_of,
                )
            })
            .collect()
    }

    /// `GET /api/v1/storage/arrays`
    pub fn arrays(&self) -> Result<Vec<MdArray>, StorageError> {
        let g = self.graph()?;
        Ok(self.arrays_with(&g))
    }

    fn filesystems_with(&self, g: &BlockGraph, mounts: &[Mount]) -> Vec<BtrfsFilesystem> {
        // --mounted: no scan of unmounted devices (that would read every disk)
        let Some(show) = self.run_ok(
            "btrfs",
            &["filesystem", "show", "--mounted", "--raw"],
            T_BTRFS,
        ) else {
            return Vec::new();
        };
        let now = self.host.now_ms();
        let max_age = self.detail_max_age.as_millis() as u64;
        btrfs::parse_show(&show)
            .iter()
            .map(|sh| {
                // all the names a device may have in mountinfo
                let mut sources: Vec<String> = Vec::new();
                let mut disks: HashSet<String> = HashSet::new();
                for d in &sh.devices {
                    sources.push(d.path.clone());
                    if let Some(b) = g.find(&d.path) {
                        sources.push(b.path.clone());
                        sources.push(format!("/dev/{}", b.kname));
                        if b.kind == "crypt" {
                            sources.push(format!("/dev/mapper/{}", b.name));
                        }
                        disks.extend(g.disks_under(&b.kname));
                    }
                }
                let main = inventory::fs_mounts(mounts, &sources)
                    .first()
                    .map(|m| m.mountpoint.clone());
                let sleeping = disks
                    .iter()
                    .filter_map(|d| g.get(d))
                    .any(|d| asleep(self.power_of(d)));
                let mut stats = None;
                let mut scrub = None;
                let mut stale = false;
                let mut cache_entry = self.cache.lock().unwrap().fs.get(&sh.uuid).cloned();
                if let Some(mnt) = &main {
                    // in-memory counters and the scrub status file: safe while disks sleep
                    stats = self
                        .run_ok("btrfs", &["device", "stats", mnt], T_BTRFS)
                        .map(|s| btrfs::parse_device_stats(&s));
                    scrub = self
                        .run_ok("btrfs", &["scrub", "status", "--raw", mnt], T_BTRFS)
                        .map(|s| btrfs::parse_scrub(&s));
                    let fresh = cache_entry
                        .as_ref()
                        .is_some_and(|c| now.saturating_sub(c.read_ms) < max_age);
                    if !fresh {
                        if sleeping {
                            stale = true;
                        } else {
                            let usage = self
                                .run_ok("btrfs", &["filesystem", "usage", "-b", mnt], T_BTRFS)
                                .map(|s| btrfs::parse_usage(&s));
                            let mut args = btrfs::SUBVOL_LIST_ARGS.to_vec();
                            args.push(mnt);
                            let subvolumes = self
                                .run_ok("btrfs", &args, T_BTRFS)
                                .map(|s| btrfs::parse_subvolume_list(&s))
                                .unwrap_or_default();
                            let readonly = self
                                .run_ok("btrfs", &["subvolume", "list", "-r", mnt], T_BTRFS)
                                .map(|s| btrfs::readonly_ids(&s))
                                .unwrap_or_default();
                            let c = FsCache {
                                read_ms: now,
                                usage,
                                subvolumes,
                                readonly,
                            };
                            self.cache
                                .lock()
                                .unwrap()
                                .fs
                                .insert(sh.uuid.clone(), c.clone());
                            cache_entry = Some(c);
                        }
                    }
                }
                let parts = FsParts {
                    show: Some(sh),
                    usage: cache_entry.as_ref().and_then(|c| c.usage.as_ref()),
                    stats: stats.as_ref(),
                    subvolumes: cache_entry
                        .as_ref()
                        .map(|c| c.subvolumes.clone())
                        .unwrap_or_default(),
                    readonly: cache_entry
                        .as_ref()
                        .map(|c| c.readonly.clone())
                        .unwrap_or_default(),
                    scrub,
                    quota: btrfs::quota_mode(self.host.as_ref(), &sh.uuid),
                };
                let mut f = inventory::build_filesystem(
                    &sh.uuid,
                    sh.label.clone(),
                    &sources,
                    mounts,
                    parts,
                    cache_entry.as_ref().map(|c| c.read_ms).unwrap_or(now),
                );
                f.stale = stale;
                f
            })
            .collect()
    }

    /// `GET /api/v1/storage/filesystems`
    pub fn filesystems(&self) -> Result<Vec<BtrfsFilesystem>, StorageError> {
        let g = self.graph()?;
        Ok(self.filesystems_with(&g, &self.mounts()))
    }

    /// Everything at once, from a single lsblk run.
    pub fn inventory(&self) -> Result<Inventory, StorageError> {
        let graph = self.graph()?;
        let mounts = self.mounts();
        let disks = self.summaries(&graph).into_iter().map(|(s, _)| s).collect();
        let arrays = self.arrays_with(&graph);
        let filesystems = self.filesystems_with(&graph, &mounts);
        Ok(Inventory {
            graph,
            disks,
            arrays,
            filesystems,
            mounts,
        })
    }

    /// The cached inventory if it is younger than [`Storage::inventory_max_age`].
    pub fn cached_inventory(&self) -> Option<Arc<Inventory>> {
        let now = self.host.now_ms();
        let max = self.inventory_max_age.as_millis() as u64;
        let c = self.cache.lock().unwrap();
        c.inventory
            .as_ref()
            .filter(|(at, _)| now.saturating_sub(*at) < max)
            .map(|(_, inv)| inv.clone())
    }

    /// Takes a new inventory and caches it. The caller makes sure only one refresh runs at a
    /// time (single flight: rynasd serialises them behind an async lock).
    pub fn refresh_inventory(&self) -> Result<Arc<Inventory>, StorageError> {
        let inv = Arc::new(self.inventory()?);
        let now = self.host.now_ms();
        self.cache.lock().unwrap().inventory = Some((now, inv.clone()));
        Ok(inv)
    }

    /// The cached inventory, or a new one.
    pub fn current_inventory(&self) -> Result<Arc<Inventory>, StorageError> {
        match self.cached_inventory() {
            Some(i) => Ok(i),
            None => self.refresh_inventory(),
        }
    }

    /// `GET /api/v1/storage/tree`
    pub fn tree(&self) -> Result<StorageTree, StorageError> {
        let inv = self.current_inventory()?;
        Ok(self.tree_of(&inv))
    }

    /// The tree of an inventory, with the details of the open LUKS mappings.
    pub fn tree_of(&self, inv: &Inventory) -> StorageTree {
        let mut t = inventory::tree(inv);
        // details of the open LUKS mappings (device-mapper table only)
        let mut status: HashMap<String, luks::CryptStatus> = HashMap::new();
        for c in luks::containers(&inv.graph) {
            if let Some((_, name)) = c.mapping
                && let Some(out) = self.run_ok("cryptsetup", &["status", &name], T_CRYPT)
            {
                status.insert(name, luks::parse_status(&out));
            }
        }
        fn walk(n: &mut TreeNode, st: &HashMap<String, luks::CryptStatus>) {
            if n.kind == NodeKind::Crypt
                && let Some(s) = st.get(&n.name)
            {
                let mut parts = vec![s.kind.clone().unwrap_or_else(|| "LUKS".into())];
                parts.extend(s.cipher.clone());
                if let Some(ss) = s.sector_size {
                    parts.push(format!("{ss}-byte sectors"));
                }
                if s.flags.iter().any(|f| f == "discards") {
                    parts.push("discard".into());
                }
                n.detail = Some(parts.join(", "));
            }
            for c in &mut n.children {
                walk(c, st);
            }
        }
        for v in &mut t.volumes {
            for l in &mut v.layers {
                walk(l, &status);
            }
        }
        t
    }

    pub fn plan_volume(&self, req: &VolumePlanRequest) -> Result<StoragePlan, StorageError> {
        Ok(plan::plan_volume(&*self.current_inventory()?, req))
    }

    pub fn plan_grow(&self, req: &GrowPlanRequest) -> Result<StoragePlan, StorageError> {
        Ok(plan::plan_grow(&*self.current_inventory()?, req))
    }

    pub fn plan_replace(&self, req: &ReplacePlanRequest) -> Result<StoragePlan, StorageError> {
        Ok(plan::plan_replace(&*self.current_inventory()?, req))
    }

    pub fn plan_scrub(&self, req: &ScrubPlanRequest) -> Result<StoragePlan, StorageError> {
        Ok(plan::plan_scrub(&*self.current_inventory()?, req))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeHost;
    use rynas_api_types::storage::{ArrayHealth, HealthVerdict};

    fn fx(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    /// The fixture NAS: sdd is in standby, the others awake.
    fn host() -> FakeHost {
        let mut h = FakeHost {
            now_ms: 1_793_836_800_000,
            ..Default::default()
        };
        let lsblk = format!("lsblk {}", LSBLK_ARGS.join(" "));
        h.cmd(&lsblk, 0, &fx("lsblk-nas.json"));
        for d in ["sda", "sdb", "sdc", "sde"] {
            h.cmd(
                &format!("smartctl --json=c -n standby -a -g apm -g wcache /dev/{d}"),
                0,
                &fx("smartctl-sata-hdd.json"),
            );
            h.power.insert(format!("/dev/{d}"), PowerState::Active);
        }
        h.power.insert("/dev/sdd".into(), PowerState::Standby);
        for d in ["nvme0n1", "nvme1n1"] {
            h.cmd(
                &format!("smartctl --json=c -a /dev/{d}"),
                0,
                &fx("smartctl-nvme.json"),
            );
        }
        h.file("/proc/mdstat", &fx("proc-mdstat.txt"));
        h.file("/proc/self/mountinfo", &fx("proc-self-mountinfo.txt"));
        h.cmd(
            "mdadm --detail --export /dev/md2",
            0,
            &fx("mdadm-detail-export-md2.txt"),
        );
        h.cmd("mdadm --detail /dev/md2", 0, &fx("mdadm-detail-md2.txt"));
        h.cmd(
            "btrfs filesystem show --mounted --raw",
            0,
            &fx("btrfs-filesystem-show.txt")
                .split("\n\n")
                .take(2)
                .collect::<Vec<_>>()
                .join("\n\n"),
        );
        h.cmd(
            "btrfs device stats /vol1",
            0,
            &fx("btrfs-device-stats-vol1.txt"),
        );
        h.cmd(
            "btrfs scrub status --raw /vol1",
            0,
            &fx("btrfs-scrub-running.txt"),
        );
        h.cmd(
            "btrfs filesystem usage -b /vol1",
            0,
            &fx("btrfs-usage-vol1.txt"),
        );
        h.cmd(
            "btrfs subvolume list -p -c -g -u -q -R /vol1",
            0,
            &fx("btrfs-subvolume-list-vol1.txt"),
        );
        h.cmd(
            "btrfs subvolume list -r /vol1",
            0,
            &fx("btrfs-subvolume-list-ro-vol1.txt"),
        );
        h.cmd(
            "cryptsetup status rynas-vol1",
            0,
            &fx("cryptsetup-status.txt"),
        );
        h.file(
            "/sys/fs/btrfs/3e6c1f0a-9b2d-4c8e-a7f1-5d0b2c4e6a81/label",
            "vol1\n",
        );
        h.file("/sys/block/md2/md/array_state", "clean\n");
        h.file("/sys/block/md2/md/degraded", "1\n");
        h
    }

    #[test]
    fn never_wakes_a_sleeping_disk() {
        let h = Arc::new(host());
        let s = Storage::new(h.clone());
        let disks = s.disks().unwrap();
        assert_eq!(disks.len(), 7);
        let sdd = disks.iter().find(|d| d.name == "sdd").unwrap();
        assert_eq!(sdd.power_state, PowerState::Standby);
        assert_eq!(sdd.health, HealthVerdict::Unknown);
        assert!(
            !h.ran().iter().any(|c| c.contains("/dev/sdd")),
            "{:?}",
            h.ran()
        );
        let sda = disks.iter().find(|d| d.name == "sda").unwrap();
        assert_eq!(sda.health, HealthVerdict::Healthy);
    }

    #[test]
    fn smart_is_cached() {
        let h = Arc::new(host());
        let s = Storage::new(h.clone());
        s.disks().unwrap();
        s.disks().unwrap();
        let n = h
            .ran()
            .iter()
            .filter(|c| c.ends_with("/dev/sda") && c.starts_with("smartctl"))
            .count();
        assert_eq!(n, 1);
    }

    #[test]
    fn disk_settings_reread_after_apply() {
        let mut h = host();
        h.cmd(
            "smartctl --json=c -n standby -g apm -g wcache /dev/sda",
            0,
            r#"{"smartctl":{"exit_status":0},"ata_apm":{"enabled":true,"level":127},"write_cache":{"enabled":false}}"#,
        );
        let h = Arc::new(h);
        let s = Storage::new(h.clone());
        s.disks().unwrap();
        let apm = |s: &Storage| {
            s.disk("sda")
                .unwrap()
                .settings
                .into_iter()
                .find(|c| c.key == "apm")
                .and_then(|c| c.current)
        };
        assert_ne!(apm(&s).as_deref(), Some("127"));
        let read = s.refresh_disk_settings();
        assert!(read.contains(&"sda".to_string()), "{read:?}");
        assert_eq!(apm(&s).as_deref(), Some("127"));
        // the sleeping disk and the NVMe were not asked
        assert!(!h.ran().iter().any(|c| c.contains("-g apm") && c.ends_with("/dev/sdd")));
        assert!(!h.ran().iter().any(|c| c.contains("-g apm") && c.contains("nvme")));
    }

    #[test]
    fn standby_reported_by_smartctl() {
        let mut h = host();
        // power check not available (e.g. USB bridge): smartctl's -n standby still protects
        h.power.remove("/dev/sdb");
        h.cmd(
            "smartctl --json=c -n standby -a -g apm -g wcache /dev/sdb",
            2,
            &fx("smartctl-standby.json"),
        );
        let s = Storage::new(Arc::new(h));
        let d = s.disk("sdb").unwrap();
        assert_eq!(d.summary.power_state, PowerState::Standby);
        assert!(d.smart.is_none());
    }

    #[test]
    fn disk_detail_and_errors() {
        let mut h = host();
        h.file("/sys/block/sda/queue/scheduler", "[mq-deadline] none\n")
            .file("/sys/block/sda/queue/read_ahead_kb", "128\n");
        let s = Storage::new(Arc::new(h));
        let d = s.disk("sda").unwrap();
        assert!(d.smart.is_some());
        assert_eq!(d.partitions[0].holders, ["md2"]);
        let ra = d
            .settings
            .iter()
            .find(|c| c.key == "read_ahead_kb")
            .unwrap();
        assert_eq!(ra.current.as_deref(), Some("128"));
        assert_eq!(s.disk("md2"), Err(StorageError::NotFound("md2".into())));
        assert_eq!(s.disk("zram0"), Err(StorageError::NotFound("zram0".into())));
    }

    #[test]
    fn arrays_and_filesystems() {
        let h = Arc::new(host());
        let s = Storage::new(h.clone());
        let a = s.arrays().unwrap();
        let md2 = a.iter().find(|a| a.name == "md2").unwrap();
        assert_eq!(md2.health, ArrayHealth::Degraded);
        assert_eq!(md2.events, Some(48_213));
        assert!(!md2.stale);
        let f = s.filesystems().unwrap();
        assert_eq!(f.len(), 2);
        let vol1 = f
            .iter()
            .find(|f| f.label.as_deref() == Some("vol1"))
            .unwrap();
        assert_eq!(
            vol1.scrub.as_ref().unwrap().status,
            rynas_api_types::storage::ScrubState::Running
        );
        assert_eq!(vol1.subvolumes.len(), 7);
        assert_eq!(vol1.quota, rynas_api_types::storage::QuotaMode::None);
        let t = s.tree().unwrap();
        let v = t
            .volumes
            .iter()
            .find(|v| v.label.as_deref() == Some("vol1"))
            .unwrap();
        assert_eq!(
            v.layers[0].detail.as_deref(),
            Some("LUKS2, aes-xts-plain64, 4096-byte sectors, discard")
        );
    }

    #[test]
    fn md_detail_skipped_when_member_sleeps() {
        let mut h = host();
        h.power.insert("/dev/sdb".into(), PowerState::Standby);
        let h = Arc::new(h);
        let s = Storage::new(h.clone());
        let md2 = s
            .arrays()
            .unwrap()
            .into_iter()
            .find(|a| a.name == "md2")
            .unwrap();
        assert!(md2.stale);
        assert_eq!(md2.events, None);
        assert!(
            !h.ran()
                .iter()
                .any(|c| c.starts_with("mdadm") && c.contains("/dev/md2"))
        );
        // btrfs walk on top of the sleeping array is skipped too
        let f = s.filesystems().unwrap();
        let vol1 = f
            .iter()
            .find(|f| f.label.as_deref() == Some("vol1"))
            .unwrap();
        assert!(vol1.stale);
        assert!(
            !h.ran()
                .iter()
                .any(|c| c == "btrfs filesystem usage -b /vol1")
        );
    }

    #[test]
    fn plan_through_service() {
        let s = Storage::new(Arc::new(host()));
        let p = s
            .plan_volume(&VolumePlanRequest {
                disks: vec!["sdd".into()],
                level: rynas_api_types::storage::RaidLevel::Single,
                encrypt: false,
                compress: true,
                label: "vol2".into(),
            })
            .unwrap();
        assert!(p.valid, "{:?}", p.errors);
        // sdd sleeps: no SMART yet
        assert!(p.warnings.iter().any(|w| w.contains("no SMART data")));
    }

    #[test]
    fn inventory_is_shared_while_fresh() {
        let h = Arc::new(host());
        let s = Storage::new(h.clone());
        let a = s.current_inventory().unwrap();
        s.tree().unwrap();
        let b = s.current_inventory().unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        let lsblk = h.ran().iter().filter(|c| c.starts_with("lsblk")).count();
        assert_eq!(lsblk, 1, "{:?}", h.ran());
        // expired: a new one
        let mut s2 = Storage::new(h.clone());
        s2.inventory_max_age = Duration::ZERO;
        let c = s2.current_inventory().unwrap();
        assert!(!Arc::ptr_eq(&c, &s2.current_inventory().unwrap()));
    }
}
