//! Space (`/v1/space/...`, public under `/api/v1/space/...`): what takes room on each volume
//! and how to get it back, plus the snapshot guardrails (`docs/phase2/space.md`, D-SPACE).
//!
//! - [`discover`]: where snapshots and leftovers are (configuration and a short walk);
//! - [`model`]: history and growth, budgets, the low-space hysteresis, the thinning order;
//! - [`actions`]: safe actions and review items (dry runs and parsers);
//! - [`domain`]: the `space` configuration key (and its journald drop-in);
//! - [`pins`]: pinned snapshots.
//!
//! A scan runs in a thread of its own at idle I/O priority and nice 19 (daily, skipping
//! volumes whose disks sleep, and on demand); its answer is kept in
//! `<state>/space/<volume>.json` and `<state>/space/_global.json`, so `GET /v1/space` answers
//! at once. Every deletion is a task that discovers again first and refuses protected
//! snapshots; it ends with `btrfs subvolume sync`, so the space shows as freed when the task
//! ends.

pub mod actions;
pub mod discover;
pub mod domain;
pub mod model;
pub mod pins;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::notify::{EventAction, NotifyCategory, NotifySeverity};
use rynas_api_types::snapshots::SnapshotsConfig;
use rynas_api_types::space::{
    CategoryUsage, MeasureMethod, PinRequest, PinnedSnapshots, RefusedPath, ReviewDeleteRequest, ReviewItem, ReviewKind,
    SPACE_KEY, SafeAction, SafeActionKind, SnapshotSource, SourceCaps, SourceKind, SpaceCategory, SpaceDeleteRequest,
    SpaceOverview, SpacePreview, SpacePreviewRequest, SpaceScanPlan, SpaceSettings, SpaceSnapshot, VolumeSpace,
};
use rynas_api_types::{SharesConfig, TaskStarted, error_codes};
use rynas_btrfs::SNAPSHOTS_KEY;
use rynas_btrfs::extents::WalkError;
use rynas_btrfs::freed::Line;
use rynas_btrfs::pcspace::{Known, LineSpace, Snap as PcSnap, Step};
use rynas_notify::Notifier;
use rynas_notify::event::{code, field, firing, oneshot, quietly, resolved};
use rynas_services::SHARES_KEY;
use rynas_tasks::{Cmd, TaskCtx};
use serde::{Deserialize, Serialize};

use self::discover::{Facts, JobFact, Probe, Source, SystemFact};
use self::model::History;
use crate::ops::{Error, Ops, Res, user};

/// The least time one volume's measurements may take in a scan a person started before the
/// rest is left unmeasured (the task then ends partial); three times the expected time when
/// that is longer.
pub const VOLUME_BUDGET: Duration = Duration::from_secs(45 * 60);
/// The time one volume's update after a change may take (pairs of neighbours only: seconds
/// each); beyond it the task ends partial and the rest waits for the next change or Measure.
pub const CHANGES_BUDGET: Duration = Duration::from_secs(10 * 60);
/// The daily housekeeping (recycle bins, thumbnails, pins) is due after this long; a volume's
/// part of it waits for its disks to be awake.
pub const HOUSEKEEPING_EVERY: Duration = Duration::from_secs(24 * 3600);
/// Changes of snapshots arriving within this time make one update.
const CHANGES_SETTLE: Duration = Duration::from_secs(30);
/// Speeds of the reference NAS (a J4125 with disks, the PC backups' first measure of
/// 2026-11-26) when this NAS has not timed a measure yet: tree bytes a full tree search reads
/// per second, milliseconds per pair of neighbours.
const REF_WALK_BPS: u64 = 1_500_000;
const REF_PAIR_MS: u64 = 10_000;

/// What a scan measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanMode {
    /// Asked by a person ("Measure"): every source, with the full tree searches and file
    /// walks it needs, Docker's usage, the leftovers and the recycle bins.
    Full,
    /// After snapshots changed (rynas made or removed some), with the volume's disks awake:
    /// only the pairs of neighbours not compared before. Nothing is searched or walked in
    /// full; a source that would need it keeps its last numbers, and so do Docker, the
    /// leftovers and the recycle bins.
    Changes,
}

/// How a scan ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanEnd {
    /// Sources left unmeasured by the time budget (`None`: none).
    pub partial: Option<String>,
}

/// Sends "the snapshots under this path changed" to Space (`None`: every volume).
static CHANGES: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<Option<PathBuf>>> = std::sync::OnceLock::new();

/// Snapshots under `path` were made or removed by rynas (a snapshot job, a retention pass, a
/// receive, a deletion): Space brings that volume's numbers up to date when its disks are
/// next awake. `None`: any volume. Does nothing before Space runs.
pub fn snapshots_changed(path: Option<&Path>) {
    if let Some(tx) = CHANGES.get() {
        let _ = tx.send(path.map(Path::to_path_buf));
    }
}
/// Most ranges a measurement keeps per range set (16 bytes each). With the set's and the
/// kept members' sets, vector growth and the merge's copy, the peak stays under 128 MiB
/// (2 × 3 × 16 MiB); more shared extents than that are "not measured".
pub const MAX_RANGES: usize = 1 << 20;
/// Most distinct data extents the tree search keeps for a set (8 bytes each; the vector
/// holds at most about twice that before a deduplication: under 128 MiB); more are "not
/// measured".
pub const MAX_EXTENTS: usize = 1 << 23;
/// How long the preview of a deletion may measure before it answers "not measured".
pub const PREVIEW_BUDGET: Duration = Duration::from_secs(5 * 60);
/// Most snapshots the automatic thinning removes in one pass.
pub const THIN_MAX: usize = 50;
/// Most time one pass of the automatic thinning takes (it goes on at the next check).
pub const THIN_TIME_CAP: Duration = Duration::from_secs(15 * 60);

/// What holds a volume for one deletion of the automatic thinning (dropped right after it).
pub type Claim = Box<dyn Send>;

/// The key of the "automatic cleanup cannot free more" problem of a volume.
fn stuck_key(label: &str) -> String {
    format!("space:{label}:auto_free_stuck")
}

/// What a measurement gives for a set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Measured {
    pub freed: Option<u64>,
    pub at_least: Option<u64>,
    pub method: MeasureMethod,
}

/// The machine as Space sees it (a canned tree in tests).
pub trait Env: Send + Sync {
    /// `(label, mount point, folder the walk starts from)` of every volume.
    fn volumes(&self) -> Vec<(String, PathBuf, PathBuf)>;
    /// `(total, free)` bytes of the filesystem at `mount`.
    fn space(&self, mount: &Path) -> Option<(u64, u64)>;
    fn probe(&self) -> &dyn Probe;
    /// What removing `set` frees while `keep` stays; `progress` gets a line per subvolume.
    fn measure(&self, mount: &Path, set: &[PathBuf], keep: &[PathBuf], stop: &dyn Fn() -> bool, progress: &dyn Fn(&str)) -> Measured;
    /// What removing the selected snapshots of `lines` frees (each line oldest first, `true`:
    /// selected; all on `mount`), by diffs and an exact check; `keep`: what a full walk keeps
    /// when the lines cannot be compared.
    fn measure_lines(&self, mount: &Path, lines: &[Line], keep: &[PathBuf], stop: &dyn Fn() -> bool, progress: &dyn Fn(&str)) -> Measured;
    /// Allocated bytes of a plain tree.
    fn tree_bytes(&self, path: &Path, stop: &dyn Fn() -> bool) -> Option<u64>;
    /// Deletes one subvolume (not recursive: nested subvolumes make it fail).
    fn delete_subvolume(&self, path: &Path) -> Result<(), String>;
    /// Removes a plain file or folder (never following links).
    fn remove_tree(&self, path: &Path) -> Result<(), String>;
    /// Waits until the filesystem freed what deleted subvolumes held.
    fn sync(&self, mount: &Path) -> Result<(), String>;
    /// Where the system's own files are (`/`; the dry root under `--dry-system`).
    fn root(&self) -> PathBuf;
    /// The volume's disks sleep: an automatic scan leaves it alone.
    fn asleep(&self, _mount: &Path) -> bool {
        false
    }
    /// Runs a command that changes the system (`None`: refused, e.g. `--dry-system`).
    fn run(&self, c: Cmd) -> Result<String, String>;
    /// A `btrfs receive` writes into `dir` now.
    fn receiving(&self, dir: &Path) -> bool;
    /// The filesystem holding `path` (its UUID), when it can be read.
    fn fs_id(&self, path: &Path) -> Option<String>;
    /// Measures one line of read-only snapshots (oldest first) by comparing neighbours
    /// ([`rynas_btrfs::pcspace`]); `anchor` false: the pairs only, never a full tree search.
    /// Not available by default (canned trees: the source is then walked as before).
    fn measure_line(&self, _line: &[PcSnap], _anchor: bool, _stop: &dyn Fn() -> bool, _progress: &dyn Fn(&str), _step: &mut dyn FnMut(Step)) -> Result<LineSpace, WalkError> {
        Err(WalkError::Unsupported("no neighbour comparison here".into()))
    }
    /// Bytes of a subvolume's tree: what a full tree search of it reads.
    fn subvolume_tree_bytes(&self, _path: &Path) -> Option<u64> {
        None
    }
}

/// The machine. `dry`: nothing is deleted or run (logged instead).
pub struct HostEnv {
    pub dry: bool,
    pub root: PathBuf,
    pub shares: Option<Arc<rynas_services::SharesDomain>>,
}

fn statvfs(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs(3) fills the struct; a valid C string
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let f = s.f_frsize as u64;
    Some((s.f_blocks as u64 * f, s.f_bavail as u64 * f))
}

/// The quota groups of a filesystem, when quotas are on, full (not simple) and consistent:
/// level-0 exclusive bytes by subvolume id.
fn qgroups(mount: &Path) -> Option<HashMap<u64, u64>> {
    let dir = rynas_btrfs::ioctl::open_dir(mount).ok()?;
    let base = Path::new("/sys/fs/btrfs").join(rynas_btrfs::ioctl::fs_uuid(&dir).ok()?).join("qgroups");
    let read = |p: PathBuf| std::fs::read_to_string(p).ok().map(|s| s.trim().to_string());
    if !base.is_dir() || read(base.join("mode")).as_deref() == Some("squota") || read(base.join("inconsistent")).as_deref() == Some("1") {
        return None;
    }
    let mut out = HashMap::new();
    for e in std::fs::read_dir(&base).ok()?.flatten() {
        let n = e.file_name().into_string().unwrap_or_default();
        let Some(id) = n.strip_prefix("0_").and_then(|x| x.parse::<u64>().ok()) else { continue };
        if let Some(x) = read(e.path().join("exclusive")).and_then(|s| s.parse::<u64>().ok()) {
            out.insert(id, x);
        }
    }
    Some(out)
}

/// A level-1 quota group whose members are exactly `ids`: its exclusive bytes are what
/// removing the set frees (`btrfs qgroup show -c --raw`).
fn qgroup_level1(mount: &Path, ids: &BTreeSet<u64>) -> Option<u64> {
    let out = actions::run(Cmd::new("btrfs").args(["qgroup", "show", "-c", "--raw"]).arg(mount).timeout(Duration::from_secs(60))).ok()?;
    parse_level1(&out, ids)
}

/// The exclusive bytes of the level-1 group of `qgroup show -c --raw` whose children are
/// exactly `ids`.
pub fn parse_level1(out: &str, ids: &BTreeSet<u64>) -> Option<u64> {
    for l in out.lines() {
        let w: Vec<&str> = l.split_whitespace().collect();
        let Some(g) = w.first().and_then(|g| g.strip_prefix("1/")) else { continue };
        if g.parse::<u64>().is_err() {
            continue;
        }
        let excl: u64 = w.get(2)?.parse().ok()?;
        let children: BTreeSet<u64> = w
            .get(3)
            .map(|c| c.split(',').filter_map(|x| x.strip_prefix("0/").and_then(|n| n.parse().ok())).collect())
            .unwrap_or_default();
        if !children.is_empty() && children == *ids {
            return Some(excl);
        }
    }
    None
}

impl Env for HostEnv {
    fn volumes(&self) -> Vec<(String, PathBuf, PathBuf)> {
        crate::places::host_places()
            .into_iter()
            .map(|(label, folder)| {
                if label == rynas_api_types::storage::SYSTEM_VOLUME {
                    (label, PathBuf::from("/"), PathBuf::from(folder))
                } else {
                    (label, PathBuf::from(&folder), PathBuf::from(folder))
                }
            })
            .collect()
    }

    fn space(&self, mount: &Path) -> Option<(u64, u64)> {
        statvfs(mount)
    }

    fn probe(&self) -> &dyn Probe {
        &discover::HostProbe
    }

    fn measure(&self, mount: &Path, set: &[PathBuf], keep: &[PathBuf], stop: &dyn Fn() -> bool, progress: &dyn Fn(&str)) -> Measured {
        qgroup_measure(mount, set).unwrap_or_else(|| walk_measure(set, keep, stop, progress))
    }

    fn measure_lines(&self, mount: &Path, lines: &[Line], keep: &[PathBuf], stop: &dyn Fn() -> bool, progress: &dyn Fn(&str)) -> Measured {
        let set: Vec<PathBuf> = lines.iter().flatten().filter(|x| x.1).map(|x| x.0.clone()).collect();
        if let Some(m) = qgroup_measure(mount, &set) {
            return m;
        }
        let lim = rynas_btrfs::extents::Limits { max_ranges: MAX_RANGES, max_extents: MAX_EXTENTS, stop, progress };
        match rynas_btrfs::freed::measure_lines(lines, &lim) {
            Ok(m) => Measured { freed: Some(m.freed), at_least: None, method: MeasureMethod::ExtentWalk },
            Err(rynas_btrfs::extents::WalkError::Unsupported(e)) => {
                progress(&format!("{e}; measuring by full walks instead"));
                walk_measure(&set, keep, stop, progress)
            }
            Err(e) => {
                tracing::info!("space: {}: {e}", set.first().map(|p| p.display().to_string()).unwrap_or_default());
                progress(&format!("not measured: {e}"));
                Measured::default()
            }
        }
    }

    fn tree_bytes(&self, path: &Path, stop: &dyn Fn() -> bool) -> Option<u64> {
        rynas_btrfs::extents::tree_bytes(path, stop).ok().map(|x| x.0)
    }

    fn delete_subvolume(&self, path: &Path) -> Result<(), String> {
        if self.dry {
            tracing::info!("space (dry system): would delete subvolume {}", path.display());
            return Ok(());
        }
        let (parent, name) = (path.parent().ok_or("no parent")?, path.file_name().and_then(|n| n.to_str()).ok_or("bad name")?);
        let dir = rynas_btrfs::ioctl::open_dir(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        rynas_btrfs::ioctl::destroy_subvolume(&dir, name).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn remove_tree(&self, path: &Path) -> Result<(), String> {
        if self.dry {
            tracing::info!("space (dry system): would remove {}", path.display());
            return Ok(());
        }
        // never across a subvolume or a mount point
        actions::remove_plain_tree(path)
    }

    fn receiving(&self, dir: &Path) -> bool {
        actions::receive_running(dir, Path::new("/proc"))
    }

    fn fs_id(&self, path: &Path) -> Option<String> {
        rynas_btrfs::ioctl::open_dir(path).ok().and_then(|f| rynas_btrfs::ioctl::fs_uuid(&f).ok())
    }

    fn sync(&self, mount: &Path) -> Result<(), String> {
        if self.dry {
            return Ok(());
        }
        actions::subvolume_sync(mount)
    }

    fn root(&self) -> PathBuf {
        self.root.clone()
    }

    fn asleep(&self, mount: &Path) -> bool {
        self.shares.as_ref().is_some_and(|s| crate::services::path_asleep(s, &mount.display().to_string()))
    }

    fn measure_line(&self, line: &[PcSnap], anchor: bool, stop: &dyn Fn() -> bool, progress: &dyn Fn(&str), step: &mut dyn FnMut(Step)) -> Result<LineSpace, WalkError> {
        let lim = rynas_btrfs::extents::Limits { max_ranges: MAX_RANGES, max_extents: MAX_EXTENTS, stop, progress };
        rynas_btrfs::pcspace::measure_line_opts(line, &lim, false, anchor, step)
    }

    fn subvolume_tree_bytes(&self, path: &Path) -> Option<u64> {
        let f = rynas_btrfs::ioctl::open_dir(path).ok()?;
        let id = rynas_btrfs::ioctl::subvol_info(&f).ok()?.root_id;
        rynas_btrfs::extents::subvolume_tree_bytes(&f, id).ok().flatten()
    }

    fn run(&self, c: Cmd) -> Result<String, String> {
        if self.dry {
            tracing::info!("space (dry system): would run {c:?}");
            return Ok(String::new());
        }
        actions::run(c)
    }
}

/// The exact number from quota groups, when they are on and a level-1 group is exactly `set`.
fn qgroup_measure(mount: &Path, set: &[PathBuf]) -> Option<Measured> {
    let q = qgroups(mount)?;
    let ids: BTreeSet<u64> = set
        .iter()
        .filter_map(|p| rynas_btrfs::ioctl::open_dir(p).ok().and_then(|f| rynas_btrfs::ioctl::subvol_info(&f).ok()).map(|i| i.root_id))
        .collect();
    let at_least: u64 = ids.iter().filter_map(|i| q.get(i)).sum();
    qgroup_level1(mount, &ids).map(|x| Measured { freed: Some(x), at_least: Some(at_least), method: MeasureMethod::Qgroup })
}

/// The full walk of `set` and `keep` (`rynas_btrfs::extents::measure`).
fn walk_measure(set: &[PathBuf], keep: &[PathBuf], stop: &dyn Fn() -> bool, progress: &dyn Fn(&str)) -> Measured {
    let lim = rynas_btrfs::extents::Limits { max_ranges: MAX_RANGES, max_extents: MAX_EXTENTS, stop, progress };
    match rynas_btrfs::extents::measure(set, keep, &lim) {
        // a file that could not be read may hold anything: no number rather than a wrong one
        Ok(m) if m.unreadable > 0 => {
            tracing::info!("space: {} files unreadable, not measured", m.unreadable);
            progress(&format!("not measured: {} files unreadable", m.unreadable));
            Measured::default()
        }
        Ok(m) => Measured { freed: Some(m.freed), at_least: m.exclusive, method: MeasureMethod::ExtentWalk },
        Err(e) => {
            tracing::info!("space: {}: {e}", set.first().map(|p| p.display().to_string()).unwrap_or_default());
            progress(&format!("not measured: {e}"));
            Measured::default()
        }
    }
}

/// Work a person waits for (the preview): the lowest best-effort disk priority, not idle (an
/// idle reader gets nothing while a backup or a scrub keeps the disks busy).
fn waited_for_priority() {
    // SAFETY: plain syscalls on the calling thread, no memory passed
    unsafe {
        libc::syscall(libc::SYS_ioprio_set, 1, 0, (2 << 13) | 7);
        libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t, 10);
    }
}

/// The calling thread reads the disk only when nobody else does and gets the CPU last.
fn background_priority() {
    // SAFETY: plain syscalls on the calling thread, no memory passed
    unsafe {
        libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13);
        libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t, 19);
    }
}

/// Docker facts gathered before a scan (the engine is async).
#[derive(Debug, Clone, Default)]
pub struct DockerFacts {
    /// The volume holding Docker's data.
    pub volume: Option<String>,
    pub usage: rynas_api_types::docker::DockerDiskUsage,
    /// Untagged images nothing needs: `(id, bytes)`.
    pub dangling: Vec<(String, u64)>,
    /// Tagged images nothing needs: `(id, name, bytes)`.
    pub unused_images: Vec<(String, String, u64)>,
    /// Named volumes no container uses: `(name, bytes)`.
    pub unused_volumes: Vec<(String, u64)>,
}

/// The cached answer of one volume and the global part.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Global {
    safe_actions: Vec<SafeAction>,
    review: Vec<ReviewItem>,
    scanned_at_ms: Option<u64>,
}

pub struct Space {
    pub ops: Arc<Ops>,
    pub notifier: Arc<dyn Notifier>,
    pub env: Arc<dyn Env>,
    /// rynasd's state directory (replication status, task logs).
    pub state_dir: PathBuf,
    /// `<state>/space`.
    dir: PathBuf,
    /// The file manager's thumbnail cache.
    pub thumbs: PathBuf,
    /// Downloaded app catalogues (`<state>/catalog`).
    pub catalog: PathBuf,
    pub docker: crate::docker::DkState,
    pub pc: crate::pc_backup::PbState,
    pub pins: Arc<pins::Pins>,
    /// The scheduling gate (its leases are in the lock table the deletions ask).
    pub gate: Option<Arc<crate::gate::Gate>>,
    scanning: AtomicBool,
    /// Automatic cleanup passes a person stopped: volume → the settings then (JSON). The next
    /// pass waits for the next day ([`model::ThinDay::cooldown_until_ms`]) unless the settings
    /// change.
    thin_stopped: Mutex<HashMap<String, String>>,
    /// Previews by task id, with when they were asked (the oldest finished ones go first).
    previews: Mutex<HashMap<String, (u64, SpacePreview)>>,
    history: Mutex<History>,
    /// The images each app stack runs now, read before a scan and before the identical
    /// snapshots action ([`Space::refresh_stack_images`]).
    stack_images: Mutex<Option<BTreeMap<String, discover::ImageSet>>>,
    /// What the neighbour comparisons found out per snapshot UUID (`<state>/space/known.json`):
    /// a later scan compares only the pairs that changed.
    /// By volume, then snapshot UUID; a volume's map holds only the snapshots it had at its
    /// last update.
    known: Mutex<BTreeMap<String, BTreeMap<String, Known>>>,
    /// Volumes whose snapshots changed since their last numbers.
    pending: Mutex<BTreeSet<String>>,
    /// Volumes with an update waiting for their disks.
    waiting: Mutex<BTreeSet<String>>,
}

/// Previews kept in memory.
const PREVIEWS_KEPT: usize = 32;

pub type SpaceState = Option<Arc<Space>>;

fn now_ms() -> u64 {
    rynas_tasks::now_ms()
}

fn sp(st: &SpaceState) -> Res<Arc<Space>> {
    st.clone().ok_or_else(|| Error::new(StatusCode::SERVICE_UNAVAILABLE, error_codes::BACKEND_UNAVAILABLE, "space not available".into()))
}

fn json_body<T>(b: Result<Json<T>, JsonRejection>) -> Res<T> {
    b.map(|Json(v)| v).map_err(Error::from)
}

fn started(id: String) -> Response {
    (StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response()
}

/// A request's path: absolute, normalized, at most 4 KiB.
fn check_path(p: &str) -> Res<()> {
    let ok = p.starts_with('/')
        && p.len() <= 4096
        && !p.contains('\0')
        && !p.ends_with('/')
        && p[1..].split('/').all(|c| !c.is_empty() && c != "." && c != "..");
    if ok { Ok(()) } else { Err(Error::invalid("paths", format!("invalid snapshot path {p:?}"))) }
}

/// What discovery found on every volume (no measurement).
pub struct Discovered {
    pub volumes: Vec<(String, PathBuf, PathBuf)>,
    pub sources: BTreeMap<String, Vec<Source>>,
    pub leftovers: BTreeMap<String, Vec<discover::Leftover>>,
    pub facts: Facts,
}

impl Discovered {
    /// The source and snapshot at `path`.
    pub fn find(&self, path: &str) -> Option<(&str, &Source, &discover::Snap)> {
        self.sources.iter().find_map(|(v, ss)| {
            ss.iter().find_map(|s| s.snaps.iter().find(|x| x.path.display().to_string() == path).map(|x| (v.as_str(), s, x)))
        })
    }

    fn mount_of(&self, volume: &str) -> Option<&Path> {
        self.volumes.iter().find(|v| v.0 == volume).map(|v| v.1.as_path())
    }
}

impl Space {
    #[allow(clippy::too_many_arguments)]
    pub fn new(ops: Arc<Ops>, notifier: Arc<dyn Notifier>, env: Arc<dyn Env>, state_dir: PathBuf, thumbs: PathBuf, catalog: PathBuf, pins: Arc<pins::Pins>) -> Arc<Self> {
        let dir = state_dir.join("space");
        let history = std::fs::read(dir.join("history.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let known = std::fs::read(dir.join("known.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Arc::new(Self {
            ops,
            notifier,
            env,
            dir,
            state_dir,
            thumbs,
            catalog,
            docker: None,
            pc: None,
            pins,
            gate: None,
            scanning: AtomicBool::new(false),
            thin_stopped: Mutex::new(HashMap::new()),
            previews: Mutex::new(HashMap::new()),
            history: Mutex::new(history),
            stack_images: Mutex::new(None),
            known: Mutex::new(known),
            pending: Mutex::new(BTreeSet::new()),
            waiting: Mutex::new(BTreeSet::new()),
        })
    }

    /// Docker and PC backups, once they exist.
    pub fn with(self: Arc<Self>, docker: crate::docker::DkState, pc: crate::pc_backup::PbState, gate: Option<Arc<crate::gate::Gate>>) -> Arc<Self> {
        let mut s = Arc::try_unwrap(self).unwrap_or_else(|_| panic!("Space::with before sharing"));
        s.docker = docker;
        s.pc = pc;
        s.gate = gate;
        Arc::new(s)
    }

    /// Keeps a preview's answer; above [`PREVIEWS_KEPT`] the oldest finished ones go (never
    /// one still measuring).
    fn keep_preview(&self, id: String, p: SpacePreview) {
        let mut m = self.previews.lock().unwrap();
        let asked = m.get(&id).map(|x| x.0).unwrap_or_else(now_ms);
        m.insert(id, (asked, p));
        while m.len() > PREVIEWS_KEPT {
            let Some(old) = m.iter().filter(|(_, v)| v.1.done).min_by_key(|(_, v)| v.0).map(|(k, _)| k.clone()) else { break };
            m.remove(&old);
        }
    }

    /// The applied settings (defaults when unset or unreadable).
    pub fn settings(&self) -> SpaceSettings {
        self.ops.config.applied(SPACE_KEY).ok().flatten().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
    }

    /// The defaults in force on a system where nobody ever saved the Space settings: without
    /// an applied value the journald drop-in (the journal cap) is never written. Applied once,
    /// as rynas, with its own changeset; an administrator's journald cap still wins (render).
    pub fn ensure_applied(&self, log: &dyn Fn(&str)) {
        let cfg = &self.ops.config;
        if !matches!(cfg.applied(SPACE_KEY), Ok(None)) {
            return;
        }
        let Ok(v) = serde_json::to_value(SpaceSettings::default()) else { return };
        let owner = "rynas";
        let r = cfg
            .stage(owner, SPACE_KEY, Some(v))
            .map_err(|e| e.to_string())
            .and_then(|_| cfg.apply_keys(owner, owner, "Space defaults", &[SPACE_KEY.to_string()], log, &|| false).map_err(|e| e.to_string()));
        match r {
            Ok(rev) => log(&format!("default settings applied (revision {rev})")),
            Err(e) => log(&format!("default settings not applied: {e}")),
        }
    }

    fn write_json<T: Serialize>(&self, name: &str, v: &T) {
        let _ = std::fs::create_dir_all(&self.dir);
        let tmp = self.dir.join(format!("{name}.new"));
        if std::fs::write(&tmp, serde_json::to_vec(v).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, self.dir.join(name));
        }
    }

    fn save_history(&self) {
        let h = self.history.lock().unwrap().clone();
        self.write_json("history.json", &h);
    }

    /// The configuration's facts (applied values). An unreadable `snapshots` or `shares` key
    /// is an error, never "nothing configured": every live job folder would look deleted.
    fn facts(&self) -> Result<Facts, String> {
        let c = &self.ops.config;
        let read = |key: &str| -> Result<Option<serde_json::Value>, String> { c.applied(key).map_err(|e| format!("configuration key {key}: {e}")) };
        let snaps: SnapshotsConfig = match read(SNAPSHOTS_KEY)? {
            Some(v) => serde_json::from_value(v).map_err(|e| format!("configuration key {SNAPSHOTS_KEY}: {e}"))?,
            None => SnapshotsConfig::default(),
        };
        let shares: SharesConfig = match read(SHARES_KEY)? {
            Some(v) => serde_json::from_value(v).map_err(|e| format!("configuration key {SHARES_KEY}: {e}"))?,
            None => SharesConfig::default(),
        };
        let mut replica_dirs = Vec::new();
        let jobs = snaps
            .jobs
            .iter()
            .map(|j| {
                let mut latest = Vec::new();
                for t in &j.replicas {
                    if let rynas_api_types::replication::ReplicaDestination::Local { path } = &t.destination {
                        replica_dirs.push(path.clone());
                    }
                    // a missing or unreadable state file reads as "no replica yet": all kept
                    if t.enabled {
                        latest.push(rynas_btrfs::replication::read_status(&self.state_dir, &j.name, &t.id).latest);
                    }
                }
                let r = &j.retention;
                let keep = r.hourly + r.daily + r.weekly + r.monthly + r.yearly;
                JobFact {
                    name: j.name.clone(),
                    subvolume: j.subvolume.clone(),
                    snapshot_dir: j.snapshot_dir.clone(),
                    keep_count: (keep > 0).then_some(keep),
                    replica_latest: latest,
                }
            })
            .collect();
        let sys = &snaps.system;
        let running_parent_uuid = self.env.probe().subvol(&self.env.root()).and_then(|f| f.parent_uuid);
        let docker = self.docker.as_ref().and_then(|d| {
            let l = d.layout()?;
            Some((l.stacks(), l.snapshots(), d.settings().keep_snapshots))
        });
        Ok(Facts {
            jobs,
            system: Some(SystemFact {
                dir: sys.snapshot_dir.clone(),
                prefix: sys.prefix.clone(),
                subvolume: sys.subvolume.clone(),
                keep: sys.keep,
                running_parent_uuid,
            }),
            docker,
            pc_folders: self.pc.as_ref().map(|p| p.snapshot_folders()).unwrap_or_default(),
            replica_dirs,
            pins: self.pins.all(),
            stack_images: self.stack_images.lock().unwrap().clone(),
            live: Vec::new(),
        }
        .with_live(shares.shares.into_iter().map(|s| s.path)))
    }

    /// Finds every source and leftover now, with protections (no measurement). Fails when
    /// the configuration cannot be read.
    pub fn discover(&self) -> Result<Discovered, String> {
        self.discover_in(None)
    }

    /// [`Space::discover`] of one volume only (`only`, a label): the configured sources whose
    /// snapshot folder is on it and the walk of that volume alone (the automatic cleanup).
    pub fn discover_in(&self, only: Option<&str>) -> Result<Discovered, String> {
        let mut facts = self.facts()?;
        let probe = self.env.probe();
        let mut volumes = self.env.volumes();
        let mounts: Vec<(String, PathBuf)> = volumes.iter().map(|(l, m, _)| (l.clone(), m.clone())).collect();
        if let Some(o) = only {
            let on = |p: &str| discover::volume_of(Path::new(p), &mounts) == Some(o);
            facts.jobs.retain(|j| on(&j.snapshot_dir));
            facts.system = facts.system.filter(|s| on(&s.dir));
            facts.docker = facts.docker.filter(|d| on(&d.1));
            facts.pc_folders.retain(|(_, f)| on(f));
            volumes.retain(|v| v.0 == o);
        }
        let mut all = discover::configured(&facts, probe);
        let known: BTreeSet<PathBuf> = all.iter().map(|s| s.dir.clone()).collect();
        let mut leftovers: BTreeMap<String, Vec<discover::Leftover>> = BTreeMap::new();
        for (label, _, base) in &volumes {
            let f = discover::walk(base, &known, &facts, probe);
            all.extend(f.sources);
            leftovers.entry(label.clone()).or_default().extend(f.leftovers);
        }
        for s in &mut all {
            let pc_parents = if s.kind == SourceKind::PcBackup { pc_parents(&s.dir) } else { BTreeSet::new() };
            let live = s.live.as_ref().and_then(|l| probe.subvol(l));
            discover::mark_identical(s, live.as_ref(), facts.stack_images.as_ref());
            discover::protect(s, &facts, &pc_parents);
        }
        Ok(Discovered { sources: discover::by_volume(all, &mounts), volumes, leftovers, facts })
    }

    /// The cached overview.
    pub fn overview(&self) -> SpaceOverview {
        let read = |name: &str| std::fs::read(self.dir.join(name)).ok();
        let g: Global = read("_global.json").and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let settings = self.settings();
        let low = self.history.lock().unwrap().low.clone();
        let mut volumes = Vec::new();
        for (label, mount, _) in self.env.volumes() {
            let mut v: VolumeSpace = read(&format!("{label}.json")).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| VolumeSpace {
                volume: label.clone(),
                mount: mount.display().to_string(),
                total_bytes: 0,
                used_bytes: 0,
                free_bytes: 0,
                categories: Vec::new(),
                sources: Vec::new(),
                reclaimable_bytes: 0,
                scanned_at_ms: None,
                scan_s: 0,
                auto_free: false,
                low_free: false,
                btrfs: false,
            });
            // the totals and settings are read now; the rest is the last scan's
            if let Some((t, f)) = self.env.space(&mount) {
                v.total_bytes = t;
                v.free_bytes = f;
                v.used_bytes = t.saturating_sub(f);
            }
            v.auto_free = settings.auto_free.contains(&label);
            v.low_free = low.contains(&label);
            let pins = self.pins.all();
            for s in &mut v.sources {
                for x in &mut s.snapshots {
                    x.pinned = pins.contains(&x.path);
                    if x.pinned {
                        x.protected = Some(rynas_api_types::space::Protection::Pinned);
                    } else if x.protected == Some(rynas_api_types::space::Protection::Pinned) {
                        x.protected = None;
                    }
                }
            }
            volumes.push(v);
        }
        let (journal_cap_bytes, journal_cap_by_admin) = self.journal_cap();
        SpaceOverview {
            volumes,
            safe_actions: g.safe_actions,
            review: g.review,
            scanning: self.scanning.load(Ordering::Acquire),
            scanned_at_ms: g.scanned_at_ms,
            journal_cap_bytes,
            journal_cap_by_admin,
            pending: self.pending.lock().unwrap().iter().cloned().collect(),
        }
    }

    /// The speeds measures run at on this NAS (the PC backups time them): tree bytes a full
    /// tree search reads per second, milliseconds per pair, measured here.
    fn speeds(&self) -> (u64, u64, bool) {
        self.pc.as_ref().map(|p| p.measure_speed()).unwrap_or((REF_WALK_BPS, REF_PAIR_MS, false))
    }

    /// What the PC backups' measures know (the same snapshots, never measured twice).
    fn pc_known(&self) -> BTreeMap<String, Known> {
        self.pc.as_ref().map(|p| p.space_known()).unwrap_or_default()
    }

    /// What a full scan of `d` (one volume with `only`) would compare and search, and about
    /// how long that takes.
    fn plan_of(&self, d: &Discovered, only: Option<&str>) -> SpaceScanPlan {
        let (all, pc) = (self.known.lock().unwrap().clone(), self.pc_known());
        let (bps, pair_ms, here) = self.speeds();
        let mut p = SpaceScanPlan { measured_here: here, ..SpaceScanPlan::default() };
        for (label, sources) in &d.sources {
            if only.is_some_and(|o| o != label) {
                continue;
            }
            let known = all.get(label).cloned().unwrap_or_default();
            for s in sources.iter().filter(|s| !s.snaps.is_empty()) {
                let live = has_live(s);
                let lines = source_lines(s, live, &known, &pc);
                if lines.iter().any(|l| !l.cheap) {
                    p.full_walks += 1;
                    continue;
                }
                for l in &lines {
                    p.pairs += rynas_btrfs::pcspace::pairs_needed(&l.snaps).len() as u32;
                    if !live && rynas_btrfs::pcspace::needs_walk(&l.snaps) {
                        p.walks += 1;
                        p.walk_tree_bytes += l.snaps.last().and_then(|x| self.env.subvolume_tree_bytes(&x.path)).unwrap_or(0);
                    }
                }
            }
        }
        let ms = (u128::from(p.walk_tree_bytes) * 1000 / u128::from(bps.max(1))) as u64 + pair_ms.saturating_mul(u64::from(p.pairs));
        p.estimate_s = ms.div_ceil(1000);
        p
    }

    /// `GET /v1/space/plan`: what "Measure" would do now (blocking: it reads the snapshot
    /// folders, as the person asked).
    pub fn plan(&self) -> Result<SpaceScanPlan, String> {
        let d = self.discover()?;
        Ok(self.plan_of(&d, None))
    }

    /// What removing every snapshot of `s` frees, by comparing neighbours (cached per
    /// snapshot, so only the pairs that changed are read). `None`: this mode cannot tell
    /// without walking (a line that cannot be compared pair by pair, or in
    /// [`ScanMode::Changes`] a line without a live folder never measured): [`ScanMode::Full`]
    /// then walks the files as before, [`ScanMode::Changes`] keeps the last numbers. `part`
    /// gets the expected milliseconds of each part as it begins (the progress).
    #[allow(clippy::too_many_arguments)]
    fn measure_by_lines(&self, label: &str, s: &Source, mode: ScanMode, pc: &BTreeMap<String, Known>, stop: &dyn Fn() -> bool, log: &dyn Fn(&str), part: &dyn Fn(u64)) -> Result<Option<Measured>, WalkError> {
        let live = has_live(s);
        let known = self.known.lock().unwrap().get(label).cloned().unwrap_or_default();
        let lines = source_lines(s, live, &known, pc);
        if lines.is_empty() || lines.iter().any(|l| !l.cheap) {
            return Ok(None);
        }
        if mode == ScanMode::Changes && !live && lines.iter().any(|l| rynas_btrfs::pcspace::needs_walk(&l.snaps)) {
            return Ok(None);
        }
        let (bps, pair_ms, _) = self.speeds();
        let (mut freed, mut own) = (0u64, 0u64);
        for l in &lines {
            let mut step = |st: Step| match st {
                Step::Pair { .. } => part(pair_ms),
                Step::Walk { index } => {
                    let path = l.snaps.get(index).map(|x| x.path.clone()).unwrap_or_default();
                    let tree = self.env.subvolume_tree_bytes(&path).unwrap_or(0);
                    log(&format!("full tree search of {}: {tree} bytes of tree", path.display()));
                    part((u128::from(tree) * 1000 / u128::from(bps.max(1))) as u64)
                }
            };
            let r = match self.env.measure_line(&l.snaps, !live, stop, log, &mut step) {
                Ok(r) => r,
                Err(WalkError::Unsupported(e)) => {
                    log(&format!("{}: {e}", s.dir.display()));
                    return Ok(None);
                }
                Err(e) => return Err(e),
            };
            {
                let mut all = self.known.lock().unwrap();
                let k = all.entry(label.to_string()).or_default();
                for (x, kn) in l.snaps.iter().zip(&r.known) {
                    k.insert(x.uuid.clone(), kn.clone());
                }
            }
            freed = freed.saturating_add(line_freed(&r, live));
            own = own.saturating_add(r.known.iter().map(|k| k.exclusive.as_ref().map_or(0, |e| e.bytes)).sum::<u64>());
        }
        Ok(Some(Measured { freed: Some(freed), at_least: Some(own), method: MeasureMethod::ExtentWalk }))
    }

    /// Measures and records (blocking; run in a thread of its own). `only`: one volume;
    /// `mode`: what is measured ([`ScanMode`]); `pct`: the progress by expected time.
    #[allow(clippy::too_many_arguments)]
    pub fn scan(&self, only: Option<&str>, mode: ScanMode, docker: &DockerFacts, log: &dyn Fn(&str), pct: &dyn Fn(u8), cancelled: &dyn Fn() -> bool) -> Result<ScanEnd, String> {
        let full = mode == ScanMode::Full;
        // after a change only that volume is read (the others may sleep)
        let d = if full { self.discover()? } else { self.discover_in(only)? };
        let settings = self.settings();
        let now = now_ms();
        let plan = self.plan_of(&d, only);
        let pc = self.pc_known();
        if full {
            let speed = if plan.measured_here { "speeds measured on this NAS" } else { "speeds of a reference NAS" };
            log(&format!(
                "{} pairs of neighbours to compare, {} full tree searches ({} bytes of trees), {} sources walked file by file; expected: about {} s for the comparisons and searches ({speed})",
                plan.pairs, plan.walks, plan.walk_tree_bytes, plan.full_walks, plan.estimate_s
            ));
        } else {
            log(&format!("snapshots changed: {} pairs of neighbours to compare", plan.pairs));
        }
        // the progress: the expected time of the parts begun so far
        let total_ms = plan.estimate_s.saturating_mul(1000).max(1);
        let done_ms = std::sync::atomic::AtomicU64::new(0);
        let shown = std::sync::atomic::AtomicU8::new(0);
        let part = |ms: u64| {
            let before = done_ms.fetch_add(ms, Ordering::AcqRel);
            let p = (before.saturating_mul(100) / total_ms).min(99) as u8;
            if p > shown.load(Ordering::Acquire) {
                shown.store(p, Ordering::Release);
                pct(p);
            }
        };
        let mut hist = self.history.lock().unwrap().clone();
        let mut review: Vec<ReviewItem> = Vec::new();
        let mut recycle_old = (0u64, 0u32);
        let mut stopped: Vec<String> = Vec::new();
        let mut updated: Vec<String> = Vec::new();
        let identical: Vec<&discover::Snap> =
            d.sources.values().flatten().filter(|s| s.kind == SourceKind::AppStack).flat_map(|s| s.snaps.iter()).filter(|x| x.identical && x.protected.is_none()).collect();
        let shares: SharesConfig = self.ops.config.applied(SHARES_KEY).ok().flatten().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
        let homes: Vec<String> = std::fs::read_to_string(self.env.root().join("etc/passwd")).map(|t| actions::homes_of(&t)).unwrap_or_default();
        let now_s = (now / 1000) as i64;
        let mut live_ids = BTreeSet::new();
        let read_prev = |label: &str| -> Option<VolumeSpace> { std::fs::read(self.dir.join(format!("{label}.json"))).ok().and_then(|b| serde_json::from_slice(&b).ok()) };
        let prev_global: Global = std::fs::read(self.dir.join("_global.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        for (label, mount, _base) in &d.volumes {
            if only.is_some_and(|o| o != label) {
                continue;
            }
            if !full && self.env.asleep(mount) {
                log(&format!("{label}: its disks sleep, left for when they are awake"));
                continue;
            }
            // changes from now on make the volume pending again
            self.pending.lock().unwrap().remove(label);
            updated.push(label.clone());
            let prev = read_prev(label);
            let prev_cat = |c: SpaceCategory| prev.as_ref().and_then(|v| v.categories.iter().find(|x| x.category == c)).map(|x| (x.bytes, x.reclaimable_bytes)).unwrap_or_default();
            let started = Instant::now();
            let budget = if full { VOLUME_BUDGET.max(Duration::from_secs(plan.estimate_s.saturating_mul(3))) } else { CHANGES_BUDGET };
            let deadline = started + budget;
            let stop = || cancelled() || Instant::now() > deadline;
            log(&format!("{label}: measuring"));
            let (total, free) = self.env.space(mount).unwrap_or((0, 0));
            let mut sources = Vec::new();
            let mut all_fresh = true;
            for s in d.sources.get(label).into_iter().flatten() {
                let set: Vec<PathBuf> = s.snaps.iter().map(|x| x.path.clone()).collect();
                let keep: Vec<PathBuf> = s.live.iter().filter(|l| l.exists()).cloned().collect();
                let by_lines = if set.is_empty() || stop() { Ok(None) } else { self.measure_by_lines(label, s, mode, &pc, &stop, log, &part) };
                let m = match by_lines {
                    Ok(Some(m)) => Some(m),
                    Ok(None) if set.is_empty() => Some(Measured::default()),
                    Ok(None) if full && !stop() => Some(self.env.measure(mount, &set, &keep, &stop, log)),
                    Ok(None) => None,
                    Err(e) => {
                        log(&format!("{label}: {} {}: not measured: {e}", kind_word(s.kind), s.name));
                        None
                    }
                };
                // the time budget left it without a number: the task ends partial
                if !set.is_empty() && !cancelled() && Instant::now() > deadline && m.is_none_or(|m| m.freed.is_none()) {
                    stopped.push(format!("{label}: {}", s.name));
                }
                let mut api = match m {
                    Some(m) => to_api(label, s, m),
                    // not measured now: the last numbers of the source stay
                    None => {
                        all_fresh = false;
                        let mut a = to_api(label, s, Measured::default());
                        if let Some(l) = prev.as_ref().and_then(|v| v.sources.iter().find(|x| x.id == s.id)) {
                            (a.freed_bytes, a.freed_at_least_bytes, a.method) = (l.freed_bytes, l.freed_at_least_bytes, l.method);
                        }
                        a
                    }
                };
                if let Some(b) = api.freed_bytes {
                    hist.record(&api.id, now, b);
                }
                api.growth_7d_bytes = hist.growth(&api.id, now, 7);
                if let Some(b) = settings.budget(&api.id) {
                    api.caps.max_count = b.max_count;
                    api.caps.max_age_days = b.max_age_days;
                    api.caps.max_bytes = b.max_bytes;
                }
                api.over_budget = model::over_budget(&api, &settings, now);
                live_ids.insert(api.id.clone());
                log(&format!("{label}: {} {}: {} snapshots, {}", kind_word(s.kind), s.name, api.count, bytes_text(api.freed_bytes)));
                sources.push(api);
            }
            // review: sources a person decides on, and leftovers
            for s in sources.iter() {
                let kind = match s.kind {
                    SourceKind::OrphanJob => ReviewKind::OrphanSnapshots,
                    SourceKind::Received => ReviewKind::ReceivedUnknownOwner,
                    _ => continue,
                };
                review.push(ReviewItem { id: s.id.clone(), kind, volume: Some(label.clone()), label: s.dir.clone(), bytes: s.freed_bytes, created_ms: s.newest_ms });
            }
            let mut leftover_bytes = 0u64;
            for l in d.leftovers.get(label).into_iter().flatten() {
                let id = format!("{}:{}", kind_id(l.kind), l.path.display());
                // after a change a leftover keeps its last size (each size is a walk of a tree)
                let bytes = if !full {
                    prev_global.review.iter().find(|r| r.id == id).and_then(|r| r.bytes)
                } else if stop() {
                    None
                } else {
                    self.env.tree_bytes(&l.path, &stop)
                };
                leftover_bytes += bytes.unwrap_or(0);
                review.push(ReviewItem { id, kind: l.kind, volume: Some(label.clone()), label: l.path.display().to_string(), bytes, created_ms: l.created_ms });
            }
            // top-level rollback copies of the system (not mounted: no size)
            if label == rynas_api_types::storage::SYSTEM_VOLUME {
                for (id, name) in top_level_leftovers(&self.env.root()) {
                    review.push(ReviewItem { id: format!("rollback-top:{id}:{name}"), kind: ReviewKind::RollbackCopy, volume: Some(label.clone()), label: name.clone(), bytes: None, created_ms: discover::leftover_of(&name).and_then(|x| x.1) });
                }
            }
            // recycle bins of the shares and homes on this volume: a walk of each bin, only
            // when a person asked (after a change the last size stays)
            let mut bins = prev_cat(SpaceCategory::RecycleBins).0;
            if full {
                bins = 0;
                let mut bin_paths: Vec<(String, u32)> = shares.shares.iter().filter(|sh| sh.smb.recycle.enabled).map(|sh| (sh.path.clone(), sh.smb.recycle.retention_days)).collect();
                bin_paths.extend(homes.iter().map(|h| (h.clone(), shares.smb.homes.recycle.retention_days)));
                let mounts: Vec<(String, PathBuf)> = d.volumes.iter().map(|(l, m, _)| (l.clone(), m.clone())).collect();
                for (path, days) in bin_paths {
                    if discover::volume_of(Path::new(&path), &mounts) != Some(label.as_str()) || stop() {
                        continue;
                    }
                    if let Ok(u) = rynas_services::recycle::usage(&path, days, now_s, &stop) {
                        bins += u.total_bytes;
                        recycle_old.0 += u.old_bytes;
                        recycle_old.1 += u.old_items as u32;
                    }
                }
            }
            // Docker (read before a full scan only: `docker system df` sizes every volume)
            // and the system's own files
            let (docker_bytes, docker_free) = if !full {
                prev_cat(SpaceCategory::Docker)
            } else if docker.volume.as_deref() == Some(label.as_str()) {
                let u = docker.usage;
                (u.images_bytes + u.containers_bytes + u.volumes_bytes + u.build_cache_bytes, docker.usage.build_cache_bytes + docker.dangling.iter().map(|x| x.1).sum::<u64>())
            } else {
                (0, 0)
            };
            let system_bytes = if mount == Path::new("/") {
                let root = self.env.root();
                ["var/log/journal", "var/cache/apt"].iter().filter_map(|p| self.env.tree_bytes(&root.join(p), &stop)).sum::<u64>()
                    + self.env.tree_bytes(&self.thumbs, &stop).unwrap_or(0)
                    + self.env.tree_bytes(&self.state_dir.join("tasks"), &stop).unwrap_or(0)
            } else {
                0
            };
            let snap_bytes: u64 = sources.iter().map(|s| s.freed_bytes.unwrap_or(0)).sum();
            let snap_exact = sources.iter().all(|s| s.freed_bytes.is_some() || s.count == 0);
            let unprotected: u64 = sources
                .iter()
                .filter(|s| s.snapshots.iter().all(|x| x.protected.is_none()))
                .map(|s| s.freed_bytes.unwrap_or(0))
                .sum();
            let used = total.saturating_sub(free);
            let accounted = snap_bytes + bins + docker_bytes + system_bytes + leftover_bytes;
            let categories = vec![
                CategoryUsage { category: SpaceCategory::Snapshots, bytes: snap_bytes, reclaimable_bytes: unprotected, exact: snap_exact },
                CategoryUsage { category: SpaceCategory::RecycleBins, bytes: bins, reclaimable_bytes: 0, exact: true },
                CategoryUsage { category: SpaceCategory::Docker, bytes: docker_bytes, reclaimable_bytes: docker_free, exact: true },
                CategoryUsage { category: SpaceCategory::System, bytes: system_bytes, reclaimable_bytes: 0, exact: true },
                CategoryUsage { category: SpaceCategory::Leftovers, bytes: leftover_bytes, reclaimable_bytes: 0, exact: false },
                CategoryUsage { category: SpaceCategory::Data, bytes: used.saturating_sub(accounted), reclaimable_bytes: 0, exact: snap_exact },
            ];
            hist.record(&format!("volume:{label}"), now, snap_bytes);
            let btrfs = self.env.probe().subvol(mount).is_some() || sources.iter().any(|s| s.count > 0);
            // the age shown is that of the oldest number: an update that kept some last
            // numbers keeps the last time
            let scanned_at_ms = if full || all_fresh { Some(now_ms()) } else { prev.as_ref().and_then(|v| v.scanned_at_ms) };
            let v = VolumeSpace {
                volume: label.clone(),
                mount: mount.display().to_string(),
                total_bytes: total,
                used_bytes: used,
                free_bytes: free,
                reclaimable_bytes: unprotected,
                categories,
                sources,
                scanned_at_ms,
                scan_s: started.elapsed().as_secs(),
                auto_free: settings.auto_free.contains(label),
                low_free: hist.low.contains(label),
                btrfs,
            };
            self.write_json(&format!("{label}.json"), &v);
            log(&format!("{label}: done in {} s", v.scan_s));
        }
        // what the comparisons learnt: each volume updated keeps only the snapshots it has now,
        // and volumes that went are forgotten
        {
            let mut k = self.known.lock().unwrap();
            for label in &updated {
                let seen: BTreeSet<&str> = d.sources.get(label).into_iter().flatten().flat_map(|s| s.snaps.iter()).filter_map(|x| x.facts.uuid.as_deref()).collect();
                if let Some(m) = k.get_mut(label) {
                    m.retain(|u, _| seen.contains(u.as_str()));
                }
            }
            let vols: BTreeSet<String> = self.env.volumes().into_iter().map(|v| v.0).collect();
            k.retain(|v, m| vols.contains(v) && !m.is_empty());
            let k = k.clone();
            self.write_json("known.json", &k);
        }
        if !full {
            // after a change only the volume's review entries and the history move
            *self.history.lock().unwrap() = hist;
            self.save_history();
            if let Some(o) = only {
                let mut g = prev_global;
                g.review.retain(|r| r.volume.as_deref() != Some(o) || matches!(r.kind, ReviewKind::OldKernel | ReviewKind::UnusedImage | ReviewKind::UnusedVolume));
                g.review.extend(review);
                self.write_json("_global.json", &g);
            }
            return Ok(ScanEnd { partial: partial_of(&stopped) });
        }
        // the global part
        let root = self.env.root();
        let journal = self.env.tree_bytes(&root.join("var/log/journal"), &|| false).unwrap_or(0);
        let (cap, _) = self.journal_cap();
        let (apt_bytes, apt_n) = actions::apt_cache_bytes(&root);
        let thumbs = actions::cache_files(&self.thumbs);
        let serials = actions::stale_serials(&self.catalog);
        let serial_bytes: u64 = serials.iter().filter_map(|p| self.env.tree_bytes(p, &|| false)).sum();
        let dangling: u64 = docker.dangling.iter().map(|x| x.1).sum::<u64>() + docker.usage.build_cache_bytes;
        let act = |kind, bytes: u64, items: u32| SafeAction { kind, bytes, items, available: bytes > 0 || items > 0 };
        let safe_actions = vec![
            act(SafeActionKind::JournalVacuum, actions::journal_excess(journal, cap), 0),
            act(SafeActionKind::AptClean, apt_bytes, apt_n),
            act(SafeActionKind::DockerDangling, dangling, docker.dangling.len() as u32),
            act(SafeActionKind::RecycleBins, recycle_old.0, recycle_old.1),
            act(SafeActionKind::Thumbnails, thumbs.iter().map(|x| x.1).sum(), thumbs.len() as u32),
            act(SafeActionKind::CatalogSerials, serial_bytes, serials.len() as u32),
            act(SafeActionKind::IdenticalAppSnapshots, 0, identical.len() as u32),
        ];
        // kernels and Docker leftovers
        for k in self.old_kernels() {
            review.push(ReviewItem { id: format!("kernel:{}", k.package), kind: ReviewKind::OldKernel, volume: Some(rynas_api_types::storage::SYSTEM_VOLUME.into()), label: k.package, bytes: Some(k.bytes), created_ms: None });
        }
        for (id, name, bytes) in &docker.unused_images {
            review.push(ReviewItem { id: format!("image:{id}"), kind: ReviewKind::UnusedImage, volume: docker.volume.clone(), label: name.clone(), bytes: Some(*bytes), created_ms: None });
        }
        for (name, bytes) in &docker.unused_volumes {
            review.push(ReviewItem { id: format!("volume:{name}"), kind: ReviewKind::UnusedVolume, volume: docker.volume.clone(), label: name.clone(), bytes: Some(*bytes), created_ms: None });
        }
        if only.is_none() {
            hist.keep_only(&live_ids);
        }
        *self.history.lock().unwrap() = hist;
        self.save_history();
        // a scan of one volume keeps the other volumes' review entries
        let mut g: Global = std::fs::read(self.dir.join("_global.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        if let Some(o) = only {
            g.review.retain(|r| r.volume.as_deref() != Some(o) && !matches!(r.kind, ReviewKind::OldKernel | ReviewKind::UnusedImage | ReviewKind::UnusedVolume));
            g.review.extend(review);
        } else {
            g.review = review;
            g.scanned_at_ms = Some(now_ms());
        }
        g.safe_actions = safe_actions;
        self.write_json("_global.json", &g);
        Ok(ScanEnd { partial: partial_of(&stopped) })
    }

    /// The journal's cap in force (what journald reads now, never below the administrator's
    /// value) and whether the administrator set one.
    pub fn journal_cap(&self) -> (Option<u64>, bool) {
        let root = self.env.root();
        let admin = domain::admin_journal_cap(&root);
        (domain::effective_journal_cap(admin, domain::journald_cap_now(&root)), admin.is_some())
    }

    fn old_kernels(&self) -> Vec<actions::Kernel> {
        let q = actions::run(Cmd::new("dpkg-query").args(["-W", "-f", "${Package}\t${db:Status-Abbrev}\t${Installed-Size}\n", "linux-image-*"]).timeout(Duration::from_secs(30)));
        let Ok(out) = q else { return Vec::new() };
        let running = actions::run(Cmd::new("uname").arg("-r").timeout(Duration::from_secs(5))).map(|s| s.trim().to_string()).unwrap_or_default();
        if running.is_empty() {
            return Vec::new();
        }
        let held: BTreeSet<String> = actions::run(Cmd::new("apt-mark").arg("showhold").timeout(Duration::from_secs(30)))
            .map(|s| s.lines().map(|l| l.trim().to_string()).collect())
            .unwrap_or_default();
        actions::old_kernels(&actions::parse_kernels(&out), &running, &held)
    }
}

/// The reason of a partial end: the sources the time budget left without a number.
fn partial_of(stopped: &[String]) -> Option<String> {
    (!stopped.is_empty()).then(|| format!("the time limit left {} sources unmeasured ({}); Measure goes on from what is known", stopped.len(), stopped.join(", ")))
}

/// The source's live folder exists (what keeps sharing the snapshots' data).
fn has_live(s: &Source) -> bool {
    s.live.as_ref().is_some_and(|l| l.exists())
}

/// One line of a source as the neighbour comparison sees it.
struct SourceLine {
    /// Oldest first, with what is known of each.
    snaps: Vec<PcSnap>,
    /// Neighbours compare without walks: every snapshot read-only with a UUID, and not a lone
    /// snapshot of a live folder (what it holds alone needs the live folder compared).
    cheap: bool,
}

/// The lines of `s` (`known`, then the PC backups' `pc`: by snapshot UUID).
fn source_lines(s: &Source, live: bool, known: &BTreeMap<String, Known>, pc: &BTreeMap<String, Known>) -> Vec<SourceLine> {
    let mut lines: BTreeMap<&str, Vec<&discover::Snap>> = BTreeMap::new();
    for x in s.snaps.iter().rev() {
        lines.entry(line_of(&x.name)).or_default().push(x);
    }
    lines
        .into_values()
        .map(|l| {
            let readonly = l.windows(2).all(|w| rynas_btrfs::pcspace::pair_cheap(w[0].facts.readonly, w[1].facts.readonly));
            let ids = l.iter().all(|x| x.facts.uuid.is_some());
            let cheap = ids && readonly && (!live || l.len() >= 2);
            let snaps = l
                .iter()
                .map(|x| {
                    let uuid = x.facts.uuid.clone().unwrap_or_default();
                    // Space's own numbers, else what a PC's measure found (never searched twice)
                    let known = known.get(&uuid).or_else(|| pc.get(&uuid)).cloned().unwrap_or_default();
                    PcSnap { path: x.path.clone(), uuid, known }
                })
                .collect();
            SourceLine { snaps, cheap }
        })
        .collect()
}

/// What removing a measured line frees. Without a live folder: the whole line (each extent
/// once). With one: what every snapshot but the newest holds and its newer neighbour dropped,
/// plus what the newest holds alone (≈: data the newest shares with older ones and the live
/// folder changed since is left out; the preview of a deletion measures it exactly).
fn line_freed(r: &LineSpace, live: bool) -> u64 {
    let k = &r.known;
    if !live || k.is_empty() {
        return r.union;
    }
    let died: u64 = k[..k.len() - 1].iter().map(|x| x.died.as_ref().map_or(0, |e| e.bytes)).sum();
    died.saturating_add(k[k.len() - 1].exclusive.as_ref().map_or(0, |e| e.bytes))
}

/// The line of snapshots a name belongs to: the name before its time (`@home.20261110T0300`
/// → `@home`); names without one (app stack snapshots) are one line.
fn line_of(name: &str) -> &str {
    rynas_btrfs::name::split_snapshot_name(name).map(|(p, _)| p).or_else(|| name.rsplit_once('.').map(|(p, _)| p)).unwrap_or("")
}

/// The lines of `snaps` (newest first) that hold a member of `set`, each oldest first with
/// `true` for the members.
pub fn lines_of(snaps: &[discover::Snap], set: &[PathBuf]) -> Vec<Line> {
    let mut lines: BTreeMap<&str, Line> = BTreeMap::new();
    for s in snaps.iter().rev() {
        lines.entry(line_of(&s.name)).or_default().push((s.path.clone(), set.contains(&s.path)));
    }
    lines.into_values().filter(|l| l.iter().any(|x| x.1)).collect()
}

/// The snapshots of a source (`snaps`, newest first) that stay and can share data with `set`:
/// in each line, the kept snapshot right before and right after every run of set members.
/// Snapshots of one line are a linear history, so an extent lives in a contiguous run of
/// them: if neither neighbour of a run references it, no farther snapshot does either,
/// unless data was reflinked or deduplicated across non-adjacent snapshots (then it is
/// counted as freed: an over-estimate, never a low number). Grouping by name can only add
/// such over-estimates: a kept snapshot left out never makes the number smaller.
pub fn neighbour_keep(snaps: &[discover::Snap], set: &[PathBuf]) -> Vec<PathBuf> {
    let mut lines: BTreeMap<&str, Vec<&discover::Snap>> = BTreeMap::new();
    for s in snaps {
        lines.entry(line_of(&s.name)).or_default().push(s);
    }
    let mut out = Vec::new();
    for line in lines.values() {
        let in_set: Vec<bool> = line.iter().map(|s| set.contains(&s.path)).collect();
        for (i, s) in line.iter().enumerate() {
            let next_to_set = (i > 0 && in_set[i - 1]) || in_set.get(i + 1).copied().unwrap_or(false);
            if !in_set[i] && next_to_set {
                out.push(s.path.clone());
            }
        }
    }
    out
}

/// Top-level subvolumes of the root filesystem left by system rollbacks (`btrfs subvolume
/// list`; not mounted anywhere).
fn top_level_leftovers(root: &Path) -> Vec<(u64, String)> {
    if root != Path::new("/") {
        return Vec::new();
    }
    let Ok(out) = actions::run(Cmd::new("btrfs").args(["subvolume", "list", "/"]).timeout(Duration::from_secs(60))) else { return Vec::new() };
    actions::top_level_subvolumes(&out).into_iter().filter(|(_, n)| discover::leftover_of(n).is_some_and(|x| x.0 == ReviewKind::RollbackCopy)).collect()
}

/// A pinned snapshot is gone for sure ([`Space::prune_pins`]).
fn pin_gone(p: &Path, mounts: &[(String, PathBuf)], env: &dyn Env) -> bool {
    if std::fs::symlink_metadata(p).is_ok() {
        return false;
    }
    let Some(parent) = p.parent().filter(|d| d.is_dir()) else { return false };
    let Some(mount) = discover::volume_of(parent, mounts).and_then(|l| mounts.iter().find(|m| m.0 == l)).map(|m| &m.1) else {
        return false;
    };
    matches!((env.fs_id(parent), env.fs_id(mount)), (Some(a), Some(b)) if a == b)
}

/// Names a PC folder's next incremental sends need (the newest complete per subvolume).
fn pc_parents(dir: &Path) -> BTreeSet<String> {
    rynas_btrfs::receive::scan_dir(dir).map(|(entries, _)| rynas_btrfs::receive::parents(&entries, now_ms()).into_iter().collect()).unwrap_or_default()
}

fn kind_word(k: SourceKind) -> &'static str {
    match k {
        SourceKind::ShareJob => "job",
        SourceKind::OrphanJob => "deleted job",
        SourceKind::AppStack => "app stack",
        SourceKind::SystemPoints => "restore points",
        SourceKind::PcBackup => "PC backup",
        SourceKind::Replica => "replicas",
        SourceKind::Received => "received, owner unknown",
        SourceKind::IncomingReplica => "incoming replica",
    }
}

fn kind_id(k: ReviewKind) -> &'static str {
    match k {
        ReviewKind::RollbackCopy => "rollback",
        ReviewKind::RestoreCopy => "restore",
        ReviewKind::AdoptCopy => "adopt",
        ReviewKind::StackCopy => "stack-copy",
        ReviewKind::OrphanSnapshots => "orphan",
        ReviewKind::RemovedTargetReplicas => "replica",
        ReviewKind::RemovedPcFolder => "pc-removed",
        ReviewKind::ReceivedUnknownOwner => "received",
        ReviewKind::OldKernel => "kernel",
        ReviewKind::UnusedImage => "image",
        ReviewKind::UnusedVolume => "volume",
    }
}

fn bytes_text(b: Option<u64>) -> String {
    b.map(rynas_api_types::units::human_bytes).unwrap_or_else(|| "not measured".into())
}

/// The API form of a source after its measurement.
pub fn to_api(volume: &str, s: &Source, m: Measured) -> SnapshotSource {
    SnapshotSource {
        id: s.id.clone(),
        kind: s.kind,
        name: s.name.clone(),
        volume: volume.into(),
        dir: s.dir.display().to_string(),
        count: s.snaps.len() as u32,
        oldest_ms: s.snaps.iter().filter_map(|x| x.created_ms).min(),
        newest_ms: s.snaps.iter().filter_map(|x| x.created_ms).max(),
        freed_bytes: m.freed,
        freed_at_least_bytes: m.at_least,
        method: m.method,
        growth_7d_bytes: None,
        caps: SourceCaps { keep_count: s.keep_count, ..Default::default() },
        over_budget: false,
        snapshots: s
            .snaps
            .iter()
            .map(|x| SpaceSnapshot {
                path: x.path.display().to_string(),
                name: x.name.clone(),
                created_ms: x.created_ms,
                pinned: x.protected == Some(rynas_api_types::space::Protection::Pinned),
                protected: x.protected,
                identical: x.identical,
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------------------------
// deleting
// ---------------------------------------------------------------------------------------------

impl Space {
    /// Deletes one snapshot that discovery found unprotected (`d` freshly discovered).
    fn delete_snapshot(&self, d: &Discovered, path: &str, log: &dyn Fn(&str)) -> Result<(), String> {
        let (_, s, x) = d.find(path).ok_or_else(|| format!("{path} is not a snapshot Space knows (gone?)"))?;
        if let Some(p) = x.protected {
            return Err(format!("{path} is kept ({p:?})"));
        }
        match s.kind {
            SourceKind::ShareJob | SourceKind::SystemPoints => {
                let prefix = if s.kind == SourceKind::SystemPoints { d.facts.system.as_ref().map(|x| x.prefix.clone()).unwrap_or_default() } else { s.name.clone() };
                // btrbk's lock: never while the job runs
                let _lock = rynas_btrfs::scan::JobLock::acquire(&rynas_btrfs::scan::lock_path(&prefix)).map_err(|e| e.to_string())?;
                self.env.delete_subvolume(&x.path)
            }
            SourceKind::AppStack => {
                // the stack's own busy mark: never during an update, a rollback, a snapshot
                let _busy = match &self.docker {
                    Some(dk) => Some(dk.take_busy(&s.name).map_err(|e| e.2)?),
                    None => None,
                };
                self.env.delete_subvolume(&x.path)?;
                let _ = std::fs::remove_file(s.dir.join(format!("{}.json", x.name)));
                Ok(())
            }
            SourceKind::PcBackup if s.id.starts_with("pc:") => {
                let pc = self.pc.as_ref().ok_or("PC backups not available")?;
                pc.delete_for_space(&s.name, &x.name)
            }
            SourceKind::IncomingReplica => Err(format!("{path} is a replica another NAS sends here")),
            SourceKind::PcBackup | SourceKind::OrphanJob | SourceKind::Replica | SourceKind::Received => {
                if self.env.receiving(&s.dir) {
                    return Err(format!("a btrfs receive into {} is running", s.dir.display()));
                }
                self.env.delete_subvolume(&x.path)
            }
        }?;
        // the only time a pin goes with its snapshot (pinned ones never get here anyway)
        let _ = self.pins.set(path, false);
        log(&format!("deleted {path}"));
        // its neighbours meet now: their pair is compared at the volume's next update
        snapshots_changed(Some(&x.path));
        Ok(())
    }

    /// Waits for the cleaner on every volume touched.
    fn sync_volumes(&self, d: &Discovered, volumes: &BTreeSet<String>, log: &dyn Fn(&str)) {
        for v in volumes {
            if let Some(m) = d.mount_of(v) {
                log(&format!("{v}: the filesystem is freeing the space"));
                match self.env.sync(m) {
                    Ok(()) => log(&format!("{v}: space freed")),
                    Err(e) => log(&format!("{v}: waiting for the filesystem: {e}")),
                }
            }
        }
    }

    /// Task `space.delete`: every path must be unprotected now, or nothing is deleted.
    pub fn run_delete(&self, paths: &[String], log: &dyn Fn(&str), cancelled: &dyn Fn() -> bool) -> Result<(), String> {
        let d = self.discover()?;
        let refused: Vec<String> = paths
            .iter()
            .filter_map(|p| match d.find(p) {
                None => Some(format!("{p}: not a snapshot Space knows")),
                Some((_, _, x)) => x.protected.map(|why| format!("{p}: kept ({why:?})")),
            })
            .collect();
        if !refused.is_empty() {
            return Err(format!("nothing deleted: {}", refused.join("; ")));
        }
        let mut touched = BTreeSet::new();
        let mut failed = Vec::new();
        for p in paths {
            if cancelled() {
                break;
            }
            let vol = d.find(p).map(|x| x.0.to_string());
            match self.delete_snapshot(&d, p, log) {
                Ok(()) => {
                    touched.extend(vol);
                }
                Err(e) => {
                    log(&e);
                    failed.push(e);
                }
            }
        }
        self.sync_volumes(&d, &touched, log);
        if failed.is_empty() { Ok(()) } else { Err(format!("{} of {} not deleted", failed.len(), paths.len())) }
    }

    /// Task `space.preview`: the number for the set, within [`PREVIEW_BUDGET`].
    pub fn run_preview(&self, paths: &[String], cancelled: &dyn Fn() -> bool, log: &dyn Fn(&str)) -> SpacePreview {
        self.run_preview_within(paths, cancelled, log, PREVIEW_BUDGET)
    }

    /// [`Space::run_preview`] with its time limit: past it the answer is "not measured"
    /// (`timed_out_minutes`), never a partial number.
    pub fn run_preview_within(&self, paths: &[String], cancelled: &dyn Fn() -> bool, log: &dyn Fn(&str), budget: Duration) -> SpacePreview {
        waited_for_priority();
        let mut out = SpacePreview::default();
        let d = match self.discover() {
            Ok(d) => d,
            Err(e) => {
                out.error = Some(e);
                out.done = true;
                return out;
            }
        };
        // by source: (mount, selected snapshots)
        let mut by_source: BTreeMap<String, (PathBuf, Vec<PathBuf>)> = BTreeMap::new();
        for p in paths {
            match d.find(p) {
                None => out.refused.push(RefusedPath { path: p.clone(), protected: None }),
                Some((_, _, x)) if x.protected.is_some() => {
                    out.refused.push(RefusedPath { path: p.clone(), protected: x.protected });
                }
                Some((v, s, _)) => {
                    out.paths.push(p.clone());
                    let mount = d.mount_of(v).map(Path::to_path_buf).unwrap_or_default();
                    by_source.entry(s.id.clone()).or_insert_with(|| (mount, Vec::new())).1.push(PathBuf::from(p));
                }
            }
        }
        // by filesystem, measured together (an extent two selected sources share is freed):
        // the lines of snapshots the selection is in, and, for a full walk, what stays and
        // keeps sharing (the live subvolume, the nearest kept snapshots around the selection)
        let mut by_mount: BTreeMap<PathBuf, (Vec<Line>, Vec<PathBuf>, usize)> = BTreeMap::new();
        for (id, (mount, set)) in by_source {
            let Some(s) = d.sources.values().flatten().find(|s| s.id == id) else { continue };
            let e = by_mount.entry(mount).or_default();
            e.0.extend(lines_of(&s.snaps, &set));
            e.1.extend(s.live.iter().filter(|l| l.exists()).cloned());
            e.1.extend(neighbour_keep(&s.snaps, &set));
            e.2 += set.len();
        }
        let deadline = Instant::now() + budget;
        let late = || Instant::now() > deadline;
        let stop = || cancelled() || late();
        let mut freed = Some(0u64);
        let mut at_least = Some(0u64);
        let mut method = MeasureMethod::ExtentWalk;
        for (mount, (lines, keep, n)) in by_mount {
            log(&format!("{}: measuring {n} snapshots", mount.display()));
            let m = if stop() { Measured::default() } else { self.env.measure_lines(&mount, &lines, &keep, &stop, log) };
            freed = freed.zip(m.freed).map(|(a, b)| a + b);
            at_least = at_least.zip(m.at_least).map(|(a, b)| a + b);
            if m.method != MeasureMethod::ExtentWalk {
                method = m.method;
            }
            if freed.is_none() {
                // one volume not measured: no total
                break;
            }
        }
        if freed.is_none() && late() {
            let minutes = budget.as_secs().div_ceil(60) as u32;
            log(&format!("not measured: measuring took longer than {minutes} min"));
            out.timed_out_minutes = Some(minutes);
        }
        out.freed_bytes = freed;
        out.freed_at_least_bytes = if freed.is_some() { at_least } else { None };
        out.method = if out.freed_bytes.is_some() { method } else { MeasureMethod::None };
        out.done = true;
        out
    }

    /// Task `space.review-delete`: each id must still be in a fresh review list.
    pub fn run_review_delete(&self, ids: &[String], log: &dyn Fn(&str)) -> Result<(), String> {
        let d = self.discover()?;
        let mut touched = BTreeSet::new();
        let mut failed = 0usize;
        for id in ids {
            let r = self.review_one(&d, id, log);
            match r {
                Ok(Some(v)) => {
                    touched.insert(v);
                }
                Ok(None) => {}
                Err(e) => {
                    failed += 1;
                    log(&format!("{id}: {e}"));
                }
            }
        }
        self.sync_volumes(&d, &touched, log);
        if failed == 0 { Ok(()) } else { Err(format!("{failed} of {} not removed", ids.len())) }
    }

    /// Removes one review item; the volume to sync, if any.
    fn review_one(&self, d: &Discovered, id: &str, log: &dyn Fn(&str)) -> Result<Option<String>, String> {
        let (kind, rest) = id.split_once(':').ok_or("invalid id")?;
        match kind {
            "rollback" | "restore" | "adopt" | "stack-copy" => {
                let (vol, l) = d
                    .leftovers
                    .iter()
                    .find_map(|(v, ls)| ls.iter().find(|l| l.path.display().to_string() == rest).map(|l| (v.clone(), l)))
                    .ok_or("not found any more")?;
                if kind_id(l.kind) != kind {
                    return Err("changed meanwhile".into());
                }
                if self.env.probe().subvol(&l.path).is_some() {
                    self.env.delete_subvolume(&l.path)?;
                } else {
                    self.env.remove_tree(&l.path)?;
                }
                log(&format!("removed {rest}"));
                Ok(Some(vol))
            }
            "rollback-top" => {
                let (num, name) = rest.split_once(':').ok_or("invalid id")?;
                let num: u64 = num.parse().map_err(|_| "invalid id")?;
                let now = top_level_leftovers(&self.env.root());
                if !now.iter().any(|(i, n)| *i == num && n == name) {
                    return Err("not found any more".into());
                }
                let root_id = actions::run(Cmd::new("btrfs").args(["inspect-internal", "rootid", "/"]).timeout(Duration::from_secs(30)))?;
                if root_id.trim() == num.to_string() {
                    return Err("the system runs from it".into());
                }
                self.env.run(Cmd::new("btrfs").args(["subvolume", "delete", "--subvolid"]).arg(num.to_string()).arg("/").timeout(Duration::from_secs(600)))?;
                log(&format!("removed {name} (subvolume {num})"));
                Ok(Some(rynas_api_types::storage::SYSTEM_VOLUME.into()))
            }
            "orphan" | "received" => {
                let (vol, s) = d
                    .sources
                    .iter()
                    .find_map(|(v, ss)| ss.iter().find(|s| s.id == id).map(|s| (v.clone(), s)))
                    .ok_or("not found any more")?;
                if self.env.receiving(&s.dir) {
                    return Err(format!("a btrfs receive into {} is running", s.dir.display()));
                }
                for x in &s.snaps {
                    if x.protected.is_some() {
                        log(&format!("{} kept ({:?})", x.path.display(), x.protected));
                        continue;
                    }
                    self.env.delete_subvolume(&x.path)?;
                    let _ = std::fs::remove_file(s.dir.join(format!("{}.json", x.name)));
                    log(&format!("deleted {}", x.path.display()));
                    snapshots_changed(Some(&x.path));
                }
                // the folder itself only when nothing is left in it and it is a plain folder
                if self.env.probe().subvol(&s.dir).is_none() {
                    let _ = std::fs::remove_dir(&s.dir);
                }
                Ok(Some(vol))
            }
            "kernel" => {
                if !self.old_kernels().iter().any(|k| k.package == rest) {
                    return Err("not an old kernel any more".into());
                }
                log(&format!("removing {rest}"));
                self.env.run(crate::updates::apt(&["purge", rest], Duration::from_secs(1800)))?;
                Ok(None)
            }
            "image" | "volume" => Err("handled by the Docker step".into()),
            _ => Err("invalid id".into()),
        }
    }

    /// The safe action `kind` (blocking part; Docker images are removed by the caller).
    pub fn run_action(&self, kind: SafeActionKind, log: &dyn Fn(&str), cancelled: &dyn Fn() -> bool) -> Result<(), String> {
        match kind {
            SafeActionKind::JournalVacuum => {
                // the cap in force (the administrator's, or the applied one), never a default
                let cap = self.journal_cap().0.ok_or("no journal size limit is in force")?;
                let out = self.env.run(Cmd::new("journalctl").arg(format!("--vacuum-size={}M", cap >> 20)).timeout(Duration::from_secs(600)))?;
                log(out.trim());
            }
            SafeActionKind::AptClean => {
                self.env.run(Cmd::new("apt-get").arg("clean").timeout(Duration::from_secs(600)))?;
                log("downloaded packages removed");
            }
            SafeActionKind::DockerDangling => {
                let out = self.env.run(Cmd::new("docker").args(["builder", "prune", "-f"]).timeout(Duration::from_secs(1800)))?;
                log(out.trim());
            }
            SafeActionKind::RecycleBins => self.clean_bins(log, cancelled)?,
            SafeActionKind::Thumbnails => {
                let files = actions::cache_files(&self.thumbs);
                for (p, _, _) in &files {
                    if cancelled() {
                        break;
                    }
                    let _ = self.env.remove_tree(p);
                }
                log(&format!("{} thumbnails removed", files.len()));
            }
            SafeActionKind::CatalogSerials => {
                for p in actions::stale_serials(&self.catalog) {
                    self.env.remove_tree(&p)?;
                    log(&format!("removed {}", p.display()));
                }
            }
            SafeActionKind::IdenticalAppSnapshots => {
                let d = self.discover()?;
                let paths: Vec<String> = d
                    .sources
                    .values()
                    .flatten()
                    .filter(|s| s.kind == SourceKind::AppStack)
                    .flat_map(|s| s.snaps.iter())
                    .filter(|x| x.identical && x.protected.is_none())
                    .map(|x| x.path.display().to_string())
                    .collect();
                if paths.is_empty() {
                    log("no identical snapshot");
                } else {
                    self.run_delete(&paths, log, cancelled)?;
                }
            }
        }
        Ok(())
    }

    /// Empties the recycle bins of the shares and homes of what is older than their
    /// retention (0: kept).
    pub fn clean_bins(&self, log: &dyn Fn(&str), cancelled: &dyn Fn() -> bool) -> Result<(), String> {
        self.clean_bins_in(None, log, cancelled)
    }

    /// [`Space::clean_bins`] of the bins on the volume mounted at `mount` only (`None`: all).
    pub fn clean_bins_in(&self, mount: Option<&Path>, log: &dyn Fn(&str), cancelled: &dyn Fn() -> bool) -> Result<(), String> {
        let mounts: Vec<(String, PathBuf)> = self.env.volumes().into_iter().map(|(l, m, _)| (l, m)).collect();
        let on = |p: &str| mount.is_none_or(|m| discover::volume_of(Path::new(p), &mounts).and_then(|l| mounts.iter().find(|x| x.0 == l)).is_some_and(|x| x.1 == m));
        let shares: SharesConfig = self.ops.config.applied(SHARES_KEY).ok().flatten().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
        let homes: Vec<String> = std::fs::read_to_string(self.env.root().join("etc/passwd")).map(|t| actions::homes_of(&t)).unwrap_or_default();
        let mut bins: Vec<(String, String, u32)> = shares.shares.iter().filter(|s| s.smb.recycle.enabled).map(|s| (s.name.clone(), s.path.clone(), s.smb.recycle.retention_days)).collect();
        if shares.smb.homes.enabled && shares.smb.homes.recycle.enabled {
            bins.extend(homes.into_iter().map(|h| (h.clone(), h, shares.smb.homes.recycle.retention_days)));
        }
        let now = (now_ms() / 1000) as i64;
        // the system's files somewhere else than `/`: the dry system (and tests) touch no bin
        let dry = self.env.root() != Path::new("/");
        for (name, path, days) in bins {
            if days == 0 || cancelled() || !on(&path) {
                continue;
            }
            if dry {
                log(&format!("{name}: dry system, recycle bin kept"));
                continue;
            }
            match rynas_services::recycle::cleanup(&path, days, now, cancelled) {
                Ok(st) if st.files + st.dirs > 0 => log(&format!("{name}: {} files and {} folders older than {days} days removed ({} bytes)", st.files, st.dirs, st.bytes)),
                Ok(_) => {}
                Err(e) => log(&format!("{name}: {e}")),
            }
        }
        Ok(())
    }

    /// Forgets the pins of snapshots that are gone, and only those: the volume holding one is
    /// mounted and the snapshot's folder is there, on that volume's filesystem, without it.
    /// A missing or unmounted volume keeps every pin.
    pub fn prune_pins(&self) {
        self.prune_pins_in(None);
    }

    /// [`Space::prune_pins`] of the pins on the volume mounted at `mount` only (`None`: all;
    /// the others are not even looked at, so a sleeping volume is not read).
    pub fn prune_pins_in(&self, mount: Option<&Path>) {
        let mounts: Vec<(String, PathBuf)> = self.env.volumes().into_iter().map(|(l, m, _)| (l, m)).collect();
        let on = |p: &Path| mount.is_none_or(|m| discover::volume_of(p, &mounts).and_then(|l| mounts.iter().find(|x| x.0 == l)).is_some_and(|x| x.1 == m));
        self.pins.prune(&|p| !on(Path::new(p)) || !pin_gone(Path::new(p), &mounts, self.env.as_ref()));
    }

    /// Evicts the least recently used thumbnails above the cap.
    pub fn evict_thumbs(&self, log: &dyn Fn(&str)) {
        let cap = u64::from(self.settings().thumbs_cap_mib) << 20;
        let gone = actions::evictions(&actions::cache_files(&self.thumbs), cap);
        for p in &gone {
            let _ = self.env.remove_tree(p);
        }
        if !gone.is_empty() {
            log(&format!("thumbnail cache: {} least recently used removed", gone.len()));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// guardrails: low free space, growth, automatic thinning
// ---------------------------------------------------------------------------------------------

impl Space {
    /// One pass of the low-space watch (every few minutes): notifications with hysteresis
    /// and, on volumes where it is on, the automatic thinning.
    /// The volumes where the automatic cleanup should run now are returned: the caller takes
    /// their volume claim and runs [`Space::thin`].
    pub fn guard(&self, log: &dyn Fn(&str)) -> Vec<(String, PathBuf)> {
        let settings = self.settings();
        let mut entered: Vec<(String, PathBuf)> = Vec::new();
        for (label, mount, _) in self.env.volumes() {
            let Some((total, free)) = self.env.space(&mount) else { continue };
            let was = self.history.lock().unwrap().low.contains(&label);
            let low = model::low_state(was, free, total, settings.low_free_pct, settings.recover_pct);
            if low != was {
                {
                    let mut h = self.history.lock().unwrap();
                    h.low.retain(|l| *l != label);
                    if low {
                        h.low.push(label.clone());
                    }
                }
                self.save_history();
                let key = format!("space:{label}:low_free");
                let pct = if total == 0 { 0.0 } else { free as f64 * 100.0 / total as f64 };
                if low {
                    if settings.notify_low_free {
                        let mut ev = firing(NotifyCategory::Storage, NotifySeverity::Warning, "space", &key, format!("Little free space on {label}"));
                        ev.body = format!("{label} has {} free ({pct:.0} %). Storage > Space shows what takes room and what can go.", rynas_api_types::units::human_bytes(free));
                        ev.actions = vec![EventAction { label: "Open Space".into(), url: "/storage/space".into() }];
                        let ev = field(field(code(ev, "space.low_free"), "volume", &label), "free_pct", format!("{pct:.0}"));
                        self.notifier.emit(field(ev, "free", rynas_api_types::units::human_bytes(free)));
                    }
                } else {
                    let ev = resolved(NotifyCategory::Storage, "space", &key, format!("Enough free space on {label} again"));
                    self.notifier.emit(field(field(code(ev, "space.free_ok"), "volume", &label), "free_pct", format!("{pct:.0}")));
                    // the cleanup's "cannot free more" problem ends with it (quietly: the
                    // line above already tells)
                    if self.history.lock().unwrap().stuck_notified_ms.remove(&label).is_some() {
                        let stuck = resolved(NotifyCategory::Storage, "space", stuck_key(&label), format!("Enough free space on {label} again"));
                        self.notifier.emit(quietly(field(code(stuck, "space.free_ok"), "volume", &label)));
                        self.save_history();
                    }
                }
            }
            if low && settings.auto_free.contains(&label) {
                entered.push((label, mount));
            }
        }
        let _ = log;
        entered
    }

    /// The automatic thinning of one volume: the oldest unprotected snapshots first (budgets
    /// first), until free space is back above the recovery limit.
    ///
    /// Each deletion takes the volume (`claim`, released right after it, so replication,
    /// receives and snapshot runs get their turn between deletions), discovers that volume
    /// again, deletes one snapshot, persists the day's count at once, then waits for the
    /// filesystem (`btrfs subvolume sync`) and reads the free space. A pass stops at
    /// [`THIN_MAX`] deletions, the daily cap, [`THIN_TIME_CAP`], no gain, or when nothing
    /// more may go; every deletion is logged and one notification lists them. "Nothing more"
    /// and failures are one open problem per volume (fixed key, at most one per hour,
    /// resolved when free space is back) and the next pass waits an hour.
    pub fn thin(&self, label: &str, mount: &Path, settings: &SpaceSettings, claim: &dyn Fn() -> Result<Claim, String>, log: &dyn Fn(&str)) -> Vec<String> {
        self.thin_within(label, mount, settings, claim, log, THIN_TIME_CAP).0
    }

    /// Why an automatic cleanup pass of `label` would not start now (suspended, cooling down,
    /// the day's limit), without changing anything: the 5-minute watch starts no task then.
    pub fn thin_waits(&self, label: &str) -> Option<model::ThinStop> {
        // a pass a person stopped: changed settings release the wait at once
        let stopped = self.thin_stopped.lock().unwrap().get(label).cloned();
        if let Some(then) = stopped
            && serde_json::to_string(&self.settings()).unwrap_or_default() != then
        {
            self.thin_stopped.lock().unwrap().remove(label);
            let mut h = self.history.lock().unwrap();
            if let Some(d) = h.thin.get_mut(label) {
                d.cooldown_until_ms = 0;
            }
            drop(h);
            self.save_history();
        }
        let h = self.history.lock().unwrap();
        if h.thin_suspended.contains(label) {
            return Some(model::ThinStop::Suspended);
        }
        let mut day = h.thin.get(label).cloned().unwrap_or_default();
        day.allowance(now_ms()).err()
    }

    /// Task `space.auto-free` of `label` (subject: the volume; one at a time): a pass of
    /// [`Space::thin`] that ends **partial** when [`THIN_TIME_CAP`] stops it. `None` when one
    /// runs already or the task could not start.
    pub fn spawn_thin(self: &Arc<Self>, label: String, mount: PathBuf) -> Option<String> {
        let s = self.clone();
        let s2 = self.clone();
        let subject = label.clone();
        let subject2 = label.clone();
        let r = self.ops.tasks.spawn_once("space.auto-free", &subject, crate::scheduler::SCHEDULER_USER, move |ctx| async move {
            let (c, rt) = (ctx.clone(), tokio::runtime::Handle::current());
            let (_, stop) = in_thread(move || {
                let settings = s.settings();
                let token = c.token().clone();
                // the volume is taken for each deletion and given back between them
                let claim = || -> Result<Claim, String> {
                    if token.is_cancelled() {
                        return Err("cancelled".into());
                    }
                    let g = rt.block_on(s.claim_volumes(std::slice::from_ref(&mount), "space.auto-free"))?;
                    Ok(Box::new(g))
                };
                let log = |m: &str| {
                    tracing::info!("space: {m}");
                    c.log(m);
                };
                s.thin_within(&label, &mount, &settings, &claim, &log, THIN_TIME_CAP)
            })
            .await?;
            if ctx.is_cancelled() {
                // stopped by a person: no new pass at the next turn of the watch, only the
                // next day or after a change of the settings
                s2.thin_stopped_by_user(&subject2);
                return Err("cancelled".into());
            }
            if stop == Some(model::ThinStop::TimeCap) {
                ctx.partial(format!(
                    "the pass used its {} minutes; the next one goes on within 5 minutes while the volume stays low",
                    THIN_TIME_CAP.as_secs() / 60
                ));
            }
            Ok(())
        });
        match r {
            Ok(rynas_tasks::Spawned::New(id)) => Some(id),
            Ok(rynas_tasks::Spawned::Running(_)) => None,
            Err(e) => {
                tracing::warn!("space: automatic cleanup: {e}");
                None
            }
        }
    }

    /// A person stopped the pass of `label`: the next one waits for the next day (UTC) unless
    /// the settings change ([`Space::thin_waits`]).
    fn thin_stopped_by_user(&self, label: &str) {
        let now = now_ms();
        let next_day = (u64::from(model::day_of(now)) + 1) * 86_400_000;
        {
            let mut h = self.history.lock().unwrap();
            let d = h.thin.entry(label.to_string()).or_default();
            d.cooldown_until_ms = d.cooldown_until_ms.max(next_day);
        }
        self.save_history();
        let then = serde_json::to_string(&self.settings()).unwrap_or_default();
        self.thin_stopped.lock().unwrap().insert(label.to_string(), then);
        tracing::info!("space: {label}: automatic cleanup stopped by a person: the next pass waits for tomorrow or a change of the settings");
    }

    fn thin_within(&self, label: &str, mount: &Path, settings: &SpaceSettings, claim: &dyn Fn() -> Result<Claim, String>, log: &dyn Fn(&str), time_cap: Duration) -> (Vec<String>, Option<model::ThinStop>) {
        let now = now_ms();
        if self.history.lock().unwrap().thin_suspended.contains(label) {
            log(&format!("{label}: automatic cleanup suspended after an unusual growth of the snapshots"));
            return (Vec::new(), Some(model::ThinStop::Suspended));
        }
        let mut day = self.history.lock().unwrap().thin.get(label).cloned().unwrap_or_default();
        let allowance = match day.allowance(now) {
            Ok(n) => n.min(THIN_MAX as u32) as usize,
            Err(why) => {
                log(&format!("{label}: automatic cleanup waits ({why:?})"));
                return (Vec::new(), Some(why));
            }
        };
        let started = Instant::now();
        let mut deleted = Vec::new();
        let mut brake = model::Brake::default();
        let persist = |day: &model::ThinDay| {
            self.history.lock().unwrap().thin.insert(label.to_string(), day.clone());
            self.save_history();
        };
        let stop = loop {
            if deleted.len() >= allowance {
                break if day.count >= model::DAILY_CAP { model::ThinStop::DailyCap } else { model::ThinStop::PassCap };
            }
            if started.elapsed() >= time_cap {
                break model::ThinStop::TimeCap;
            }
            let Some((total, free)) = self.env.space(mount) else { break model::ThinStop::Failed };
            if !model::low_state(true, free, total, settings.low_free_pct, settings.recover_pct) {
                break model::ThinStop::Recovered;
            }
            // the volume, for this one deletion only
            let held = match claim() {
                Ok(c) => c,
                Err(e) => {
                    log(&format!("{label}: automatic cleanup waits: {e}"));
                    break model::ThinStop::Busy;
                }
            };
            let d = match self.discover_in(Some(label)) {
                Ok(d) => d,
                Err(e) => {
                    log(&e);
                    break model::ThinStop::Failed;
                }
            };
            let sources: Vec<SnapshotSource> = d.sources.get(label).into_iter().flatten().map(|s| to_api(label, s, Measured::default())).collect();
            let order = model::thin_order(&sources, settings, now_ms());
            let Some(next) = order.into_iter().find(|p| !deleted.contains(p)) else { break model::ThinStop::Nothing };
            if let Err(e) = self.delete_snapshot(&d, &next, log) {
                log(&e);
                break model::ThinStop::Failed;
            }
            drop(held);
            tracing::info!("space: {label}: automatic cleanup deleted {next}");
            deleted.push(next);
            day.count += 1;
            // the day's count survives a crash or restart right after this deletion
            persist(&day);
            // what the deletion gave back, once the filesystem freed it
            let _ = self.env.sync(mount);
            let after = self.env.space(mount).map(|x| x.1).unwrap_or(free);
            if brake.record(after.saturating_sub(free)) {
                day.cooldown_until_ms = now_ms() + model::COOLDOWN_MS;
                break model::ThinStop::NoGain;
            }
        };
        let stuck = matches!(stop, model::ThinStop::Nothing | model::ThinStop::Failed);
        if stuck {
            day.cooldown_until_ms = day.cooldown_until_ms.max(now_ms() + model::STUCK_RETRY_MS);
        }
        persist(&day);
        log(&format!("{label}: automatic cleanup: {} snapshots deleted, stopped: {stop:?}", deleted.len()));
        let why = match stop {
            model::ThinStop::Recovered => "free space is back above the recovery limit",
            model::ThinStop::Nothing => "nothing more may be removed automatically",
            model::ThinStop::NoGain => "the last deletions freed almost nothing: paused for 24 hours",
            model::ThinStop::DailyCap => "the daily limit of automatic deletions is reached",
            model::ThinStop::PassCap => "the limit of one pass is reached",
            model::ThinStop::Failed => "a deletion failed",
            model::ThinStop::TimeCap => "the time of one pass is used up",
            model::ThinStop::Busy => "other work holds the volume",
            model::ThinStop::Cooldown | model::ThinStop::Suspended => "paused",
        };
        let stopped = format!("{stop:?}").to_lowercase();
        if !deleted.is_empty() {
            let list: Vec<&str> = deleted.iter().take(10).map(String::as_str).collect();
            let sev = if matches!(stop, model::ThinStop::Recovered) { NotifySeverity::Warning } else { NotifySeverity::Error };
            let mut ev = oneshot(NotifyCategory::Storage, sev, "space", format!("space:{label}:auto_freed:{}", now_ms() / 60_000), format!("Old snapshots removed on {label} to free space"));
            ev.body = format!("Snapshots removed by the automatic cleanup: {}: {}; it stopped because {why}.", deleted.len(), list.join(", "));
            ev.actions = vec![EventAction { label: "Open Space".into(), url: "/storage/space".into() }];
            let ev = field(field(code(ev, "space.auto_freed"), "volume", label), "count", deleted.len());
            let ev = field(ev, "stopped", &stopped);
            self.notifier.emit(field(ev, "snapshots", list.join(", ")));
        }
        if stuck {
            let now = now_ms();
            let due = {
                let mut h = self.history.lock().unwrap();
                let last = h.stuck_notified_ms.get(label).copied().unwrap_or(0);
                let due = now.saturating_sub(last) >= model::STUCK_NOTIFY_MS;
                if due {
                    h.stuck_notified_ms.insert(label.to_string(), now);
                }
                due
            };
            if due {
                self.save_history();
                let mut ev = firing(NotifyCategory::Storage, NotifySeverity::Error, "space", stuck_key(label), format!("The automatic cleanup cannot free more space on {label}"));
                ev.body = format!("{label} is still low on free space and the automatic cleanup stopped because {why}. Storage > Space shows what takes room.");
                ev.actions = vec![EventAction { label: "Open Space".into(), url: "/storage/space".into() }];
                self.notifier.emit(field(field(code(ev, "space.auto_free_stuck"), "volume", label), "stopped", &stopped));
            }
        }
        (deleted, Some(stop))
    }

    /// The weekly growth check: snapshots of a volume grew more than the limits in 7 days.
    pub fn growth_check(&self) {
        let settings = self.settings();
        let now = now_ms();
        {
            let h = self.history.lock().unwrap();
            if now.saturating_sub(h.growth_checked_ms) < 7 * 86_400_000 {
                return;
            }
        }
        let ov = self.overview();
        let h = self.history.lock().unwrap().clone();
        let mut suspended = h.thin_suspended.clone();
        for v in &ov.volumes {
            let key = format!("volume:{}", v.volume);
            let Some(grown) = h.growth(&key, now, 7) else { continue };
            let before = h.series.get(&key).and_then(|s| s.iter().rev().find(|(d, _)| *d <= model::day_of(now).saturating_sub(7))).map(|x| x.1).unwrap_or(0);
            let alarming = model::growth_alarming(grown, before, &settings);
            // far more than the weeks before (a share encrypted or rewritten at once): the
            // oldest snapshots may be the only clean copies, so the automatic cleanup stops
            let anomalous = alarming && h.weekly_growths(&key, now, 4).is_some_and(|p| model::growth_anomalous(grown, &p));
            let thinning = settings.auto_free.contains(&v.volume);
            if anomalous && thinning && suspended.insert(v.volume.clone()) {
                let mut ev = firing(NotifyCategory::Storage, NotifySeverity::Error, "space", format!("space:{}:thin_suspended", v.volume), format!("Automatic cleanup suspended on {}", v.volume));
                ev.body = format!(
                    "Snapshots on {} grew by {} in a week, far more than in the weeks before. The automatic cleanup stops so that older snapshots stay; check the shares for mass changes.",
                    v.volume,
                    rynas_api_types::units::human_bytes(grown as u64)
                );
                ev.actions = vec![EventAction { label: "Open Space".into(), url: "/storage/space".into() }];
                let ev = field(code(ev, "space.thin_suspended"), "volume", &v.volume);
                self.notifier.emit(field(ev, "grown", rynas_api_types::units::human_bytes(grown as u64)));
            } else if !anomalous && suspended.remove(&v.volume) {
                let ev = resolved(NotifyCategory::Storage, "space", format!("space:{}:thin_suspended", v.volume), format!("Automatic cleanup resumed on {}", v.volume));
                self.notifier.emit(field(code(ev, "space.thin_resumed"), "volume", &v.volume));
            }
            if !settings.notify_growth || !alarming {
                continue;
            }
            let top: Vec<String> = model::top_growth(&v.sources, 3).into_iter().map(|(n, g)| format!("{n} +{}", rynas_api_types::units::human_bytes(g as u64))).collect();
            let mut ev = oneshot(NotifyCategory::Storage, NotifySeverity::Warning, "space", format!("space:{}:growth:{}", v.volume, model::day_of(now)), format!("Snapshots on {} grew by {} in a week", v.volume, rynas_api_types::units::human_bytes(grown as u64)));
            ev.body = format!("Largest growth: {}.", top.join(", "));
            ev.actions = vec![EventAction { label: "Open Space".into(), url: "/storage/space".into() }];
            let ev = field(field(code(ev, "space.snapshots_growth"), "volume", &v.volume), "grown", rynas_api_types::units::human_bytes(grown as u64));
            self.notifier.emit(field(ev, "top", top.join(", ")));
        }
        {
            let mut h = self.history.lock().unwrap();
            h.growth_checked_ms = now;
            h.thin_suspended = suspended;
        }
        self.save_history();
    }
}

/// Docker facts for a scan (empty without Docker).
pub async fn docker_facts(d: &crate::docker::DkState) -> DockerFacts {
    let Some(s) = d.as_ref() else { return DockerFacts::default() };
    let mut f = DockerFacts { volume: s.settings().volume.clone(), ..Default::default() };
    if s.engine().await.is_err() {
        return f;
    }
    if let Ok(df) = s.df().await {
        f.usage = df.usage;
        if let Ok(names) = crate::docker::unused_volume_names(s, false).await {
            f.unused_volumes = names.into_iter().map(|n| {
                let b = df.volumes.get(&n).map(|x| x.0.max(0) as u64).unwrap_or(0);
                (n, b)
            }).collect();
        }
    }
    if let Ok(v) = crate::docker::removable_now(s).await {
        for i in v {
            if i.dangling {
                f.dangling.push((i.id, i.size_bytes));
            } else {
                let name = i.tags.first().cloned().unwrap_or_else(|| i.id.clone());
                f.unused_images.push((i.id, name, i.size_bytes));
            }
        }
    }
    f
}

/// Removes Docker images (untagged ones nothing needs, or the named review items) that are
/// still removable now.
async fn remove_images(d: &crate::docker::DkState, which: Option<&BTreeSet<String>>, ctx: &TaskCtx) -> Result<(), String> {
    let s = d.as_ref().ok_or("Docker is not installed")?;
    let v = crate::docker::removable_now(s).await.map_err(|e| e.2)?;
    for i in v.into_iter().filter(|i| match which {
        None => i.dangling,
        Some(ids) => ids.contains(&i.id),
    }) {
        let id = i.id.clone();
        match s.call(move |e| async move { e.remove_image(&id).await }).await {
            Ok(()) => ctx.log(&format!("removed image {} ({})", i.tags.first().unwrap_or(&i.id), rynas_api_types::units::human_bytes(i.size_bytes))),
            Err(e) => ctx.log(&format!("{}: kept ({})", i.id, e.2)),
        }
    }
    s.changed();
    Ok(())
}

/// The holder (other than Space's own work and deduplication, which gives way) that holds
/// one of the volume `keys` in the lock table: a replication, a receive, a scrub, a snapshot
/// run, a backup.
pub fn volume_holder(t: &crate::claims::Table, keys: &[String]) -> Option<String> {
    use rynas_scheduler::queue::{Claim, EVERY, Gives};
    t.holders
        .iter()
        .filter(|h| h.gives != Gives::Dedup && !h.kind.as_deref().is_some_and(|k| k.starts_with("space.")))
        .find(|h| h.claims.iter().any(|c| matches!(c, Claim::Volume(k) if k == EVERY || keys.contains(k))))
        .map(|h| h.kind.clone().or(h.task.clone()).unwrap_or_else(|| h.name.clone()))
}

impl Space {
    /// Takes the volumes at `mounts` for a deletion: refused while other work holds one of
    /// them (the lock table), then held in the filesystem lock until the guard drops, so
    /// replication, receives, scrubs and snapshot runs wait for it in the scheduling gate.
    pub async fn claim_volumes(&self, mounts: &[PathBuf], what: &str) -> Result<crate::fslock::HeavyGuard, String> {
        let keys: Vec<String> = mounts.iter().filter_map(|m| crate::fslock::key_of(&m.display().to_string())).collect();
        let mdstat = std::fs::read_to_string("/proc/mdstat").unwrap_or_default();
        let table = crate::claims::table(self.gate.as_deref(), &self.ops.tasks, &mdstat, &[]);
        if let Some(h) = volume_holder(&table, &keys) {
            return Err(format!("the volume is busy ({h}): try again when it ends"));
        }
        Ok(crate::fslock::FsLocks::global().heavy(&keys, what).await)
    }

    /// The mount points of the volumes holding `paths`.
    fn mounts_of<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> Vec<PathBuf> {
        let vols: Vec<(String, PathBuf)> = self.env.volumes().into_iter().map(|(l, m, _)| (l, m)).collect();
        let mut out: Vec<PathBuf> = paths
            .into_iter()
            .filter_map(|p| discover::volume_of(Path::new(p), &vols).and_then(|l| vols.iter().find(|v| v.0 == l)).map(|v| v.1.clone()))
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// Runs `f` in a thread of its own (the scan lowers its priority; the pool's threads must
/// not keep that).
async fn in_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("space".into())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .map_err(|e| e.to_string())?;
    rx.await.map_err(|e| e.to_string())
}

impl Space {
    /// Reads the images every app stack runs now (unknown without Docker or when the engine
    /// does not answer: then no app snapshot is "identical").
    pub async fn refresh_stack_images(&self) {
        let now = match self.docker.as_ref() {
            Some(d) => crate::docker::all_current_images(d)
                .await
                .map(|m| m.into_iter().map(|(k, v)| (k, discover::image_set(&v))).collect()),
            None => None,
        };
        *self.stack_images.lock().unwrap() = now;
    }

    /// Starts task `space.scan` (one at a time; `only`: one volume, its subject).
    pub fn spawn_scan(self: &Arc<Self>, who: &str, only: Option<String>, mode: ScanMode) -> Res<String> {
        if self.scanning.swap(true, Ordering::AcqRel) {
            return Err(Error::new(StatusCode::CONFLICT, error_codes::BUSY, "a scan is running".into()));
        }
        let s = self.clone();
        let subject = only.clone();
        let r = self.ops.tasks.spawn_on("space.scan", subject.as_deref(), who, move |ctx| async move {
            let s2 = s.clone();
            // `docker system df` sizes every Docker volume: only when a person asked
            let docker = if mode == ScanMode::Full { docker_facts(&s.docker).await } else { DockerFacts::default() };
            s.refresh_stack_images().await;
            let c = ctx.clone();
            let r = in_thread(move || {
                background_priority();
                s2.scan(only.as_deref(), mode, &docker, &|m| c.log(m), &|p| c.progress(p), &|| c.is_cancelled())
            })
            .await;
            s.scanning.store(false, Ordering::Release);
            s.growth_check();
            // changes that came while it ran: their volumes again, when awake
            let left: Vec<String> = s.pending.lock().unwrap().iter().cloned().collect();
            for v in left {
                if let Some((_, m, _)) = s.env.volumes().into_iter().find(|x| x.0 == v) {
                    snapshots_changed(Some(&m));
                }
            }
            if let Some(why) = r??.partial {
                ctx.partial(why);
            }
            Ok(())
        });
        match r {
            Ok(id) => Ok(id),
            Err(e) => {
                self.scanning.store(false, Ordering::Release);
                Err(Error::internal(e))
            }
        }
    }

    /// The volume labels holding `paths` (`None`: every volume).
    fn volumes_of(&self, paths: &[Option<PathBuf>]) -> BTreeSet<String> {
        let vols: Vec<(String, PathBuf)> = self.env.volumes().into_iter().map(|(l, m, _)| (l, m)).collect();
        let mut out = BTreeSet::new();
        for p in paths {
            match p {
                None => out.extend(vols.iter().map(|v| v.0.clone())),
                Some(p) => out.extend(discover::volume_of(p, &vols).map(str::to_string)),
            }
        }
        out
    }

    /// Brings `volume` up to date after a change, once its disks are awake: never woken for
    /// it, no deadline (the numbers are not urgent; the page says they are pending). One wait
    /// per volume.
    fn update_when_awake(self: &Arc<Self>, volume: String) {
        if !self.waiting.lock().unwrap().insert(volume.clone()) {
            return;
        }
        let s = self.clone();
        tokio::spawn(async move {
            let Some((_, mount, _)) = s.env.volumes().into_iter().find(|x| x.0 == volume) else {
                s.waiting.lock().unwrap().remove(&volume);
                return;
            };
            let m = mount.display().to_string();
            // a week at a time: the wait itself costs nothing
            while crate::disk_standby::when_awake(&m, tokio::time::Instant::now() + Duration::from_secs(7 * 86_400)).await == crate::disk_standby::Woke::Deadline {}
            s.waiting.lock().unwrap().remove(&volume);
            if let Err(e) = s.spawn_scan("rynas", Some(volume.clone()), ScanMode::Changes) {
                // a scan runs: it hands the pending volumes back when it ends
                tracing::debug!("space: {volume}: {}", e.2);
            }
        });
    }
}

/// The changes of snapshots (from [`snapshots_changed`]): each volume is marked pending and
/// brought up to date when its disks are awake. Waits on the channel: nothing runs while
/// nothing changes.
fn spawn_changes(s: Arc<Space>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Option<PathBuf>>();
    if CHANGES.set(tx).is_err() {
        return;
    }
    tokio::spawn(async move {
        while let Some(first) = rx.recv().await {
            // a retention pass removes many: one update after they settle
            let mut paths = vec![first];
            let settle = tokio::time::sleep(CHANGES_SETTLE);
            tokio::pin!(settle);
            loop {
                tokio::select! {
                    p = rx.recv() => match p {
                        Some(p) => paths.push(p),
                        None => break,
                    },
                    _ = &mut settle => break,
                }
            }
            let vols = s.volumes_of(&paths);
            s.pending.lock().unwrap().extend(vols.iter().cloned());
            for v in vols {
                s.update_when_awake(v);
            }
        }
    });
}

/// The background work: the low-space watch every 5 minutes (free space from `statvfs`,
/// which btrfs answers from memory) and the daily housekeeping (recycle bins, thumbnails,
/// pins), each volume's part only while its disks are awake. Space is never measured on a
/// timer: on request ("Measure") and after snapshots changed ([`snapshots_changed`]).
pub fn spawn_loop(s: Arc<Space>) {
    spawn_changes(s.clone());
    tokio::spawn(async move {
        // let the system settle after a start
        tokio::time::sleep(Duration::from_secs(120)).await;
        let s2 = s.clone();
        let _ = in_thread(move || s2.ensure_applied(&|m| tracing::info!("space: {m}"))).await;
        let mut last_daily: Option<Instant> = None;
        loop {
            let s2 = s.clone();
            let low = in_thread(move || s2.guard(&|m| tracing::info!("space: {m}"))).await.unwrap_or_default();
            for (label, mount) in low {
                // a pass is a task (log, Stop, partial at its time limit); none while the
                // volume's cleanup waits (cooldown, the day's limit, suspended)
                if let Some(why) = s.thin_waits(&label) {
                    tracing::debug!("space: {label}: automatic cleanup waits ({why:?})");
                    continue;
                }
                s.spawn_thin(label, mount);
            }
            if last_daily.is_none_or(|t| t.elapsed() >= HOUSEKEEPING_EVERY) {
                let s2 = s.clone();
                let _ = in_thread(move || {
                    background_priority();
                    // the thumbnail cache is on the system disk
                    s2.evict_thumbs(&|m| tracing::info!("space: {m}"));
                })
                .await;
                // the bins and the pins are on the data volumes: each when its disks spin
                housekeeping_when_awake(&s);
                last_daily = Some(Instant::now());
            }
            tokio::time::sleep(Duration::from_secs(300)).await;
        }
    });
}

/// The daily recycle-bin cleanup and pin pruning of each volume, once its disks are awake
/// (deadline: the next day's turn; a bin's retention is in days, so a day late frees the
/// same items a day later and nothing is lost).
fn housekeeping_when_awake(s: &Arc<Space>) {
    for (label, mount, _) in s.env.volumes() {
        let s = s.clone();
        tokio::spawn(async move {
            let m = mount.display().to_string();
            let deadline = tokio::time::Instant::now() + HOUSEKEEPING_EVERY - Duration::from_secs(600);
            if crate::disk_standby::when_awake(&m, deadline).await == crate::disk_standby::Woke::Deadline {
                tracing::debug!("space: {label}: disks asleep all day, housekeeping left for tomorrow");
                return;
            }
            let _ = in_thread(move || {
                background_priority();
                if s.settings().recycle_daily {
                    let _ = s.clean_bins_in(Some(&mount), &|m| tracing::info!("space: recycle: {m}"), &|| false);
                }
                s.prune_pins_in(Some(&mount));
            })
            .await;
        });
    }
}

// ---------------------------------------------------------------------------------------------
// routes
// ---------------------------------------------------------------------------------------------

pub fn router(st: SpaceState) -> Router {
    Router::new()
        .route("/v1/space", get(overview))
        .route("/v1/space/scan", post(scan))
        .route("/v1/space/plan", get(plan))
        .route("/v1/space/preview", post(preview))
        .route("/v1/space/previews/{id}", get(preview_get))
        .route("/v1/space/delete", post(delete))
        .route("/v1/space/actions/{kind}", post(action))
        .route("/v1/space/review/delete", post(review_delete))
        .route("/v1/space/pins", get(pins_get))
        .route("/v1/space/pin", post(pin))
        .route("/v1/space/unpin", post(unpin))
        .route("/v1/space/settings", get(settings_get).put(settings_put))
        .with_state(st)
}

fn audit(who: &str, action: &str, target: &str) {
    tracing::info!(target: "rynas::audit", "{action} {target} by {who}");
}

fn admin(s: &Space, h: &HeaderMap) -> Res<String> {
    let who = user(h)?;
    s.ops.admins.require_admin(&who, "Space")?;
    Ok(who)
}

async fn overview(State(st): State<SpaceState>, h: HeaderMap) -> Res<Json<SpaceOverview>> {
    let s = sp(&st)?;
    admin(&s, &h)?;
    let s2 = s.clone();
    Ok(Json(tokio::task::spawn_blocking(move || s2.overview()).await.map_err(Error::internal)?))
}

#[derive(Deserialize)]
struct ScanQuery {
    volume: Option<String>,
}

async fn scan(State(st): State<SpaceState>, h: HeaderMap, Query(q): Query<ScanQuery>) -> Res<Response> {
    let s = sp(&st)?;
    let who = admin(&s, &h)?;
    if let Some(v) = &q.volume
        && !s.env.volumes().iter().any(|x| &x.0 == v)
    {
        return Err(Error::not_found(format!("volume {v}")));
    }
    Ok(started(s.spawn_scan(&who, q.volume, ScanMode::Full)?))
}

/// `GET /v1/space/plan`: what Measure would compare and search, and about how long.
async fn plan(State(st): State<SpaceState>, h: HeaderMap) -> Res<Json<SpaceScanPlan>> {
    let s = sp(&st)?;
    admin(&s, &h)?;
    let s2 = s.clone();
    let p = tokio::task::spawn_blocking(move || s2.plan()).await.map_err(Error::internal)?;
    Ok(Json(p.map_err(Error::internal)?))
}

fn check_paths(paths: &[String]) -> Res<()> {
    if paths.is_empty() || paths.len() > 2000 {
        return Err(Error::invalid("paths", "1 to 2000 snapshots"));
    }
    paths.iter().try_for_each(|p| check_path(p))
}

async fn preview(State(st): State<SpaceState>, h: HeaderMap, body: Result<Json<SpacePreviewRequest>, JsonRejection>) -> Res<Response> {
    let s = sp(&st)?;
    let who = admin(&s, &h)?;
    let req = json_body(body)?;
    check_paths(&req.paths)?;
    let s2 = s.clone();
    let id = s
        .ops
        .tasks
        .spawn("space.preview", &who, move |ctx| async move {
            let s3 = s2.clone();
            let c = ctx.clone();
            let paths = req.paths.clone();
            let p = in_thread(move || s3.run_preview(&paths, &|| c.is_cancelled(), &|m| c.log(m))).await?;
            ctx.log(&format!("{} snapshots can go, {} kept: {}", p.paths.len(), p.refused.len(), bytes_text(p.freed_bytes)));
            // stopped at the preview's time limit: no number, never a plain success
            if let Some(m) = p.timed_out_minutes {
                ctx.partial(format!("not measured: measuring took longer than {m} min"));
            }
            let err = p.error.clone();
            s2.keep_preview(ctx.id().to_string(), p);
            err.map_or(Ok(()), Err)
        })
        .map_err(Error::internal)?;
    s.previews.lock().unwrap().entry(id.clone()).or_insert_with(|| (now_ms(), SpacePreview::default()));
    Ok(started(id))
}

async fn preview_get(State(st): State<SpaceState>, h: HeaderMap, UrlPath(id): UrlPath<String>) -> Res<Json<SpacePreview>> {
    let s = sp(&st)?;
    admin(&s, &h)?;
    s.previews.lock().unwrap().get(&id).map(|x| x.1.clone()).map(Json).ok_or_else(|| Error::not_found(format!("preview {id}")))
}

async fn delete(State(st): State<SpaceState>, h: HeaderMap, body: Result<Json<SpaceDeleteRequest>, JsonRejection>) -> Res<Response> {
    let s = sp(&st)?;
    let who = admin(&s, &h)?;
    let req = json_body(body)?;
    check_paths(&req.paths)?;
    if req.confirm as usize != req.paths.len() {
        return Err(Error::new(StatusCode::BAD_REQUEST, error_codes::CONFIRMATION_REQUIRED, "confirm must be the number of snapshots".into()));
    }
    audit(&who, "space.delete", &format!("{} snapshots", req.paths.len()));
    let s2 = s.clone();
    let id = s
        .ops
        .tasks
        .spawn("space.delete", &who, move |ctx| async move {
            let mounts = s2.mounts_of(req.paths.iter().map(String::as_str));
            let _claim = s2.claim_volumes(&mounts, "space.delete").await?;
            let c = ctx.clone();
            in_thread(move || s2.run_delete(&req.paths, &|m| c.log(m), &|| c.is_cancelled())).await?
        })
        .map_err(Error::internal)?;
    Ok(started(id))
}

fn action_kind(k: &str) -> Option<SafeActionKind> {
    serde_json::from_value(serde_json::Value::String(k.replace('-', "_"))).ok()
}

async fn action(State(st): State<SpaceState>, h: HeaderMap, UrlPath(kind): UrlPath<String>) -> Res<Response> {
    let s = sp(&st)?;
    let who = admin(&s, &h)?;
    let kind = action_kind(&kind).ok_or_else(|| Error::not_found(format!("action {kind}")))?;
    audit(&who, "space.action", &format!("{kind:?}"));
    let s2 = s.clone();
    let id = s
        .ops
        .tasks
        .spawn("space.action", &who, move |ctx| async move {
            if kind == SafeActionKind::DockerDangling {
                remove_images(&s2.docker, None, &ctx).await?;
            }
            // deleting app snapshots takes their volume like every deletion
            let _claim = if kind == SafeActionKind::IdenticalAppSnapshots {
                // what the stacks run now decides which snapshots are identical
                s2.refresh_stack_images().await;
                let dir = s2.docker.as_ref().and_then(|d| d.layout()).map(|l| l.snapshots()).unwrap_or_default();
                let mounts = s2.mounts_of([dir.as_str()]);
                Some(s2.claim_volumes(&mounts, "space.action").await?)
            } else {
                None
            };
            let c = ctx.clone();
            let s3 = s2.clone();
            in_thread(move || s3.run_action(kind, &|m| c.log(m), &|| c.is_cancelled())).await??;
            Ok(())
        })
        .map_err(Error::internal)?;
    Ok(started(id))
}

async fn review_delete(State(st): State<SpaceState>, h: HeaderMap, body: Result<Json<ReviewDeleteRequest>, JsonRejection>) -> Res<Response> {
    let s = sp(&st)?;
    let who = admin(&s, &h)?;
    let req = json_body(body)?;
    if req.ids.is_empty() || req.ids.len() > 500 || req.ids.iter().any(|i| i.len() > 4200 || !i.contains(':')) {
        return Err(Error::invalid("ids", "1 to 500 review items"));
    }
    if req.confirm as usize != req.ids.len() {
        return Err(Error::new(StatusCode::BAD_REQUEST, error_codes::CONFIRMATION_REQUIRED, "confirm must be the number of items".into()));
    }
    audit(&who, "space.review-delete", &format!("{} items", req.ids.len()));
    let s2 = s.clone();
    let id = s
        .ops
        .tasks
        .spawn("space.review-delete", &who, move |ctx| async move {
            let images: BTreeSet<String> = req.ids.iter().filter_map(|i| i.strip_prefix("image:")).map(str::to_string).collect();
            let volumes: Vec<String> = req.ids.iter().filter_map(|i| i.strip_prefix("volume:")).map(str::to_string).collect();
            if !images.is_empty() {
                remove_images(&s2.docker, Some(&images), &ctx).await?;
            }
            if !volumes.is_empty() {
                let d = s2.docker.as_ref().ok_or("Docker is not installed")?;
                for v in volumes {
                    // a fresh disk usage right before each removal (never the cached one): a
                    // volume taken into use, or labeled for a known stack, stops the removals
                    let unused = crate::docker::unused_volume_names(d, true).await.map_err(|e| e.2)?;
                    if !unused.contains(&v) {
                        return Err(format!("{v}: in use now or part of a stack rynas knows: kept, nothing more removed"));
                    }
                    let env = s2.env.clone();
                    let v2 = v.clone();
                    match tokio::task::spawn_blocking(move || env.run(Cmd::new("docker").args(["volume", "rm"]).arg(&v2).timeout(Duration::from_secs(300)))).await.map_err(|e| e.to_string())? {
                        Ok(_) => ctx.log(&format!("removed volume {v}")),
                        Err(e) => ctx.log(&format!("{v}: {e}")),
                    }
                }
            }
            let rest: Vec<String> = req.ids.into_iter().filter(|i| !i.starts_with("image:") && !i.starts_with("volume:")).collect();
            if rest.is_empty() {
                return Ok(());
            }
            // the volumes of the paths the ids name (a top-level system copy: the root)
            let paths: Vec<String> = rest
                .iter()
                .filter_map(|i| i.split_once(':').map(|(k, p)| if k == "rollback-top" { "/".to_string() } else { p.to_string() }))
                .filter(|p| p.starts_with('/'))
                .collect();
            let mounts = s2.mounts_of(paths.iter().map(String::as_str));
            let _claim = s2.claim_volumes(&mounts, "space.review-delete").await?;
            let c = ctx.clone();
            in_thread(move || s2.run_review_delete(&rest, &|m| c.log(m))).await?
        })
        .map_err(Error::internal)?;
    Ok(started(id))
}

async fn pins_get(State(st): State<SpaceState>, h: HeaderMap) -> Res<Json<PinnedSnapshots>> {
    let s = sp(&st)?;
    admin(&s, &h)?;
    Ok(Json(PinnedSnapshots { paths: s.pins.all().into_iter().collect() }))
}

async fn set_pin(st: SpaceState, h: HeaderMap, body: Result<Json<PinRequest>, JsonRejection>, on: bool) -> Res<StatusCode> {
    let s = sp(&st)?;
    let who = admin(&s, &h)?;
    let req = json_body(body)?;
    check_path(&req.path).map_err(|e| e.field("path"))?;
    if on {
        // only a snapshot rynas knows (a subvolume), never any folder
        let p = PathBuf::from(&req.path);
        let s2 = s.clone();
        let known = tokio::task::spawn_blocking(move || s2.env.probe().subvol(&p).is_some_and(|f| f.readonly)).await.map_err(Error::internal)?;
        if !known {
            return Err(Error::invalid("path", "not a read-only snapshot"));
        }
    }
    s.pins.set(&req.path, on).map_err(|e| Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, e))?;
    audit(&who, if on { "space.pin" } else { "space.unpin" }, &req.path);
    // a PC fed over SSH learns its pinned snapshots from its key line
    if let Some(pc) = s.pc.clone() {
        let path = req.path.clone();
        tokio::task::spawn_blocking(move || pc.pins_changed(&path))
            .await
            .map_err(Error::internal)?
            .map_err(|e| Error::internal(format!("pinned, but the keys of the PC backup account were not written: {e}")))?;
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn pin(State(st): State<SpaceState>, h: HeaderMap, body: Result<Json<PinRequest>, JsonRejection>) -> Res<StatusCode> {
    set_pin(st, h, body, true).await
}

async fn unpin(State(st): State<SpaceState>, h: HeaderMap, body: Result<Json<PinRequest>, JsonRejection>) -> Res<StatusCode> {
    set_pin(st, h, body, false).await
}

async fn settings_get(State(st): State<SpaceState>, h: HeaderMap) -> Res<Json<SpaceSettings>> {
    let s = sp(&st)?;
    let who = admin(&s, &h)?;
    let s2 = s.clone();
    Ok(Json(
        tokio::task::spawn_blocking(move || -> Res<SpaceSettings> {
            let (eff, _, _): (SpaceSettings, SpaceSettings, Option<SpaceSettings>) = crate::services::docs(&s2.ops.config, &who, SPACE_KEY)?;
            Ok(eff)
        })
        .await
        .map_err(Error::internal)??,
    ))
}

/// Stages the settings (the client applies the key `space` with its Save); the new state.
async fn settings_put(State(st): State<SpaceState>, h: HeaderMap, body: Result<Json<SpaceSettings>, JsonRejection>) -> Res<Json<SpaceSettings>> {
    let s = sp(&st)?;
    let who = admin(&s, &h)?;
    let v = json_body(body)?;
    if let Some((f, m)) = v.problem() {
        return Err(Error::invalid(f, m));
    }
    let s2 = s.clone();
    Ok(Json(
        tokio::task::spawn_blocking(move || -> Res<SpaceSettings> {
            crate::services::stage(&s2.ops.config, &who, SPACE_KEY, &v)?;
            let (eff, _, _): (SpaceSettings, SpaceSettings, Option<SpaceSettings>) = crate::services::docs(&s2.ops.config, &who, SPACE_KEY)?;
            Ok(eff)
        })
        .await
        .map_err(Error::internal)??,
    ))
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn level1_qgroups_match_exactly() {
        let out = "qgroupid         rfer         excl child\n--------         ----         ---- -----\n0/257        16384        16384 -\n1/0        1000000       500000 0/257,0/258\n1/1        9           7 0/259\n";
        assert_eq!(parse_level1(out, &[257, 258].into()), Some(500000));
        assert_eq!(parse_level1(out, &[257].into()), None);
        assert_eq!(parse_level1(out, &[259].into()), Some(7));
    }

    #[test]
    fn paths_and_action_names() {
        assert!(check_path("/vol1/.snapshots/docs/docs.20261101T0100").is_ok());
        for bad in ["", "rel/path", "/vol1/../etc", "/vol1/./x", "/vol1/x/", "/a\0b"] {
            assert!(check_path(bad).is_err(), "{bad:?}");
        }
        assert_eq!(action_kind("journal-vacuum"), Some(SafeActionKind::JournalVacuum));
        assert_eq!(action_kind("identical_app_snapshots"), Some(SafeActionKind::IdenticalAppSnapshots));
        assert_eq!(action_kind("format-disk"), None);
    }

    fn snap(name: &str) -> discover::Snap {
        discover::Snap {
            path: PathBuf::from(format!("/v/pc/{name}")),
            name: name.into(),
            created_ms: None,
            facts: Default::default(),
            protected: None,
            identical: false,
        }
    }

    #[test]
    fn only_the_neighbours_of_each_run_are_kept() {
        // newest first, two lines (`@`, `@home`) interleaved as discovery sorts them by time
        let snaps: Vec<discover::Snap> = (1..=7)
            .rev()
            .flat_map(|d| [snap(&format!("@home.2026110{d}T0300")), snap(&format!("@.2026110{d}T0300"))])
            .collect();
        let p = |n: &str| PathBuf::from(format!("/v/pc/{n}"));
        // a run in the middle of @home (days 3-4) and the oldest @ (day 1)
        let set = vec![p("@home.20261104T0300"), p("@home.20261103T0300"), p("@.20261101T0300")];
        let mut keep = neighbour_keep(&snaps, &set);
        keep.sort();
        assert_eq!(keep, vec![p("@.20261102T0300"), p("@home.20261102T0300"), p("@home.20261105T0300")]);
        // two runs with one kept snapshot between them: kept once
        let set = vec![p("@.20261106T0300"), p("@.20261104T0300")];
        let mut keep = neighbour_keep(&snaps, &set);
        keep.sort();
        assert_eq!(keep, vec![p("@.20261103T0300"), p("@.20261105T0300"), p("@.20261107T0300")]);
        // the whole line: nothing kept in it
        let all: Vec<PathBuf> = snaps.iter().filter(|s| s.name.starts_with("@.")).map(|s| s.path.clone()).collect();
        assert!(neighbour_keep(&snaps, &all).is_empty());
        // names without a time (app stacks) are one line; a suffix after the time stays in it
        let stack = vec![snap("20261103T030000"), snap("20261102T030000"), snap("20261101T030000")];
        assert_eq!(neighbour_keep(&stack, &[p("20261102T030000")]), vec![p("20261103T030000"), p("20261101T030000")]);
        assert_eq!(line_of("@.20261110T0300_1"), "@");
        // the lines for the diff: oldest first, only those with a selected snapshot
        let l = lines_of(&snaps, &[p("@.20261102T0300")]);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].len(), 7);
        assert_eq!(l[0][0], (p("@.20261101T0300"), false));
        assert_eq!(l[0][1], (p("@.20261102T0300"), true));
        assert_eq!(l[0][6], (p("@.20261107T0300"), false));
        assert_eq!(line_of("@home.20261110T0300"), "@home");
    }

    fn ro(name: &str, uuid: &str) -> discover::Snap {
        let mut s = snap(name);
        s.facts.readonly = true;
        s.facts.uuid = Some(uuid.into());
        s
    }

    fn source(snaps: Vec<discover::Snap>) -> Source {
        Source { id: "job:docs".into(), kind: SourceKind::ShareJob, name: "docs".into(), dir: PathBuf::from("/v/pc"), live: None, keep_count: None, snaps }
    }

    #[test]
    fn lines_compare_by_neighbours_only_when_read_only_with_uuids() {
        let edge = |u: &str, b| Some(rynas_btrfs::pcspace::Edge { uuid: u.into(), bytes: b });
        let mut pc = BTreeMap::new();
        pc.insert("a".to_string(), Known { distinct: Some(7), ..Known::default() });
        pc.insert("b".to_string(), Known { distinct: Some(9), ..Known::default() });
        let mut own = BTreeMap::new();
        own.insert("b".to_string(), Known { born: edge("a", 3), ..Known::default() });
        // newest first, as discovery gives them; lines come oldest first
        let s = source(vec![ro("@.20261102T0300", "b"), ro("@.20261101T0300", "a")]);
        let l = source_lines(&s, false, &own, &pc);
        assert_eq!(l.len(), 1);
        assert!(l[0].cheap);
        assert_eq!(l[0].snaps.iter().map(|x| x.uuid.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        // Space's own numbers first, else the PC backups'
        assert_eq!(l[0].snaps[0].known.distinct, Some(7));
        assert_eq!(l[0].snaps[1].known.born, edge("a", 3));
        assert_eq!(l[0].snaps[1].known.distinct, None);
        // a writable snapshot, a missing UUID, a lone snapshot of a live folder: walked
        let mut w = ro("@.20261102T0300", "b");
        w.facts.readonly = false;
        assert!(!source_lines(&source(vec![w, ro("@.20261101T0300", "a")]), false, &own, &pc)[0].cheap);
        assert!(!source_lines(&source(vec![snap("@.20261102T0300"), ro("@.20261101T0300", "a")]), false, &own, &pc)[0].cheap);
        assert!(!source_lines(&source(vec![ro("@.20261101T0300", "a")]), true, &own, &pc)[0].cheap);
        assert!(source_lines(&source(vec![ro("@.20261101T0300", "a")]), false, &own, &pc)[0].cheap);
    }

    #[test]
    fn what_a_line_frees_with_and_without_a_live_folder() {
        let edge = |u: &str, b| Some(rynas_btrfs::pcspace::Edge { uuid: u.into(), bytes: b });
        let excl = |b| Some(rynas_btrfs::pcspace::Excl { bytes: b, older: None, newer: None });
        let r = LineSpace {
            known: vec![
                Known { died: edge("b", 10), exclusive: excl(4), ..Known::default() },
                Known { died: edge("c", 20), exclusive: excl(5), ..Known::default() },
                Known { exclusive: excl(6), ..Known::default() },
            ],
            union: 1000,
            ..LineSpace::default()
        };
        // with a live folder: what the older ones dropped plus what the newest holds alone
        assert_eq!(line_freed(&r, true), 10 + 20 + 6);
        // without one everything goes
        assert_eq!(line_freed(&r, false), 1000);
        assert_eq!(partial_of(&[]), None);
        assert!(partial_of(&["volume1: docs".into()]).unwrap().contains("volume1: docs"));
    }
}
