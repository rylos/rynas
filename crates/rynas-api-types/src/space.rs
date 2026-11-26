//! Space (`/api/v1/space/...`): what takes room on each volume and how to get it back
//! (`docs/phase2/space.md`, D-SPACE).
//!
//! rynasd scans every volume in the background (idle I/O priority) and keeps the answer in
//! `<state>/space/<volume>.json`: totals, categories (snapshots, recycle bins, Docker, system,
//! leftovers, data), the snapshot sources with what removing them would give back, the safe
//! one-click actions and the review list. Deleting is always a task, after an explicit
//! selection; protected snapshots (the newest of a source, a replication parent, the newest
//! complete PC backup, the restore point the system boots from, a pinned snapshot) are
//! refused everywhere.
//!
//! Configuration key [`SPACE_KEY`] ([`SpaceSettings`]): notifications, the opt-in automatic
//! thinning per volume, per-source budgets, the journal cap, the daily recycle cleanup and
//! the thumbnail cache cap.

use serde::{Deserialize, Serialize};

/// The configuration key of the guardrails.
pub const SPACE_KEY: &str = "space";

/// Where the bytes of a volume go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpaceCategory {
    Snapshots,
    RecycleBins,
    Docker,
    System,
    Leftovers,
    /// Everything else (the files of the shares, the apps' data).
    Data,
}

/// One category of a volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryUsage {
    pub category: SpaceCategory,
    /// Bytes this category holds on the volume (for snapshots: what removing all of them
    /// would give back, protected ones included).
    pub bytes: u64,
    /// What the safe actions and the unprotected snapshots of this category would free.
    #[serde(default)]
    pub reclaimable_bytes: u64,
    /// `false`: a lower bound (a measurement did not finish or could not be made).
    #[serde(default = "yes")]
    pub exact: bool,
}

fn yes() -> bool {
    true
}

/// What made a group of snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// A snapshot job of a share (`<parent>/.snapshots/<job>/<job>.<ts>`).
    ShareJob,
    /// Snapshots of a job that no longer exists.
    OrphanJob,
    /// Snapshots of an app stack (`/<vol>/docker/.snapshots/<stack>/<id>`).
    AppStack,
    /// System restore points (`/.snapshots/rootfs/rootfs.<ts>`), also while they are off.
    SystemPoints,
    /// A PC backup folder (removed PCs included).
    PcBackup,
    /// Replicas received from this NAS's own jobs for a target that is no longer configured
    /// (only with a record of that target).
    Replica,
    /// Received snapshots whose owner rynas does not know (no PC, no target on record).
    Received,
    /// Replicas another NAS sends here (`.replicas/<name>` without a local job): never removed
    /// from Space.
    IncomingReplica,
}

/// Why a snapshot cannot be removed from Space (or by the automatic thinning).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protection {
    /// The newest snapshot of its source: the base of the next incremental and of "previous
    /// versions".
    Newest,
    /// A replication target still needs it as the parent of its next incremental send.
    ReplicationParent,
    /// The newest complete backup of a PC subvolume (the parent of its next incremental).
    NewestBackup,
    /// The system boots from it or a rollback refers to it.
    BootReference,
    /// Pinned by a user.
    Pinned,
    /// Not a read-only snapshot (an interrupted receive, a writable copy): left alone.
    Writable,
    /// A replica another NAS sends here: its sender decides.
    IncomingReplica,
}

/// How the "freed if removed" number of a source was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MeasureMethod {
    /// Not measured yet (or the measurement failed): no number.
    #[default]
    None,
    /// The filesystem's quota groups (only when quotas were already on and consistent).
    Qgroup,
    /// rynas's own walk over the extents of the set and of what keeps sharing them.
    ExtentWalk,
}

/// One snapshot of a source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SpaceSnapshot {
    /// Absolute path (the identifier of every Space request).
    pub path: String,
    pub name: String,
    /// When it was taken, if the name or the subvolume tells.
    #[serde(default)]
    pub created_ms: Option<u64>,
    #[serde(default)]
    pub pinned: bool,
    /// Why it cannot be removed; `None`: it can.
    #[serde(default)]
    pub protected: Option<Protection>,
    /// An app stack snapshot identical to the stack folder now (nothing changed since).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub identical: bool,
}

/// Limits of a source: its own retention (read-only here) and the budget of
/// [`SpaceSettings::budgets`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SourceCaps {
    /// The source's own retention keeps at most this many (`None`: no count limit).
    #[serde(default)]
    pub keep_count: Option<u32>,
    /// The budget: most snapshots.
    #[serde(default)]
    pub max_count: Option<u32>,
    /// The budget: oldest age in days.
    #[serde(default)]
    pub max_age_days: Option<u32>,
    /// The budget: most bytes "freed if all removed".
    #[serde(default)]
    pub max_bytes: Option<u64>,
}

/// A group of snapshots made by one thing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotSource {
    /// Stable identifier (`job:<name>`, `stack:<volume>:<stack>`, `system`, `pc:<folder>`,
    /// `orphan:<path>`, `replica:<path>`): the key of budgets and history.
    pub id: String,
    pub kind: SourceKind,
    /// Job, stack, folder or PC name.
    pub name: String,
    pub volume: String,
    /// The folder holding the snapshots.
    pub dir: String,
    pub count: u32,
    #[serde(default)]
    pub oldest_ms: Option<u64>,
    #[serde(default)]
    pub newest_ms: Option<u64>,
    /// ≈ bytes given back if every snapshot of the source were removed (protected ones
    /// included). `None`: not measured.
    #[serde(default)]
    pub freed_bytes: Option<u64>,
    /// Bytes only one snapshot of the source references: given back for sure.
    #[serde(default)]
    pub freed_at_least_bytes: Option<u64>,
    #[serde(default)]
    pub method: MeasureMethod,
    /// Change of [`SnapshotSource::freed_bytes`] against 7 days ago (`None`: no history yet).
    #[serde(default)]
    pub growth_7d_bytes: Option<i64>,
    #[serde(default)]
    pub caps: SourceCaps,
    /// The budget is exceeded (count, age or bytes).
    #[serde(default)]
    pub over_budget: bool,
    pub snapshots: Vec<SpaceSnapshot>,
}

/// A one-click cleanup that only removes what nothing needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafeActionKind {
    /// The system journal down to its cap.
    JournalVacuum,
    /// Downloaded package files (`apt-get clean`).
    AptClean,
    /// Untagged Docker images nothing needs, and the build cache.
    DockerDangling,
    /// Items of the recycle bins (shares and homes) older than their retention.
    RecycleBins,
    /// The file manager's thumbnail cache.
    Thumbnails,
    /// Downloaded app catalogues older than the current and the previous one.
    CatalogSerials,
    /// App stack snapshots identical to the stack folder now (never the newest).
    IdenticalAppSnapshots,
}

/// A safe action with its dry-run numbers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafeAction {
    pub kind: SafeActionKind,
    /// What it would free now (a dry run of the scan).
    pub bytes: u64,
    /// Files, images or snapshots it would remove.
    pub items: u32,
    /// `false`: nothing to do or not possible here (no Docker, no apt).
    pub available: bool,
}

/// Something only a person should decide on: never removed automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewKind {
    /// `<subvolume>.rynas-rollback-<stamp>`: the folder as it was before a rollback.
    RollbackCopy,
    /// `<name>.rynas-before-restore-<stamp>`: an item replaced by a restore.
    RestoreCopy,
    /// `<folder>.before-adopt-<ms>`: an app's previous data kept by an adoption.
    AdoptCopy,
    /// `.<stack>.rynas-old`: a stack folder left by a failed rollback.
    StackCopy,
    /// The snapshots of a job that no longer exists.
    OrphanSnapshots,
    /// Replicas kept for a target that is no longer configured.
    RemovedTargetReplicas,
    /// The folder of a PC that is no longer paired.
    RemovedPcFolder,
    /// Received snapshots whose owner rynas does not know (the newest of each line stays).
    ReceivedUnknownOwner,
    /// An installed kernel that is neither running, the newest of its series nor held.
    OldKernel,
    /// A tagged Docker image no container, stack or rollback point uses.
    UnusedImage,
    /// A named Docker volume no container uses.
    UnusedVolume,
}

/// One entry of the review list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewItem {
    /// What [`ReviewDeleteRequest::ids`] names: `<kind>:<path or name>`.
    pub id: String,
    pub kind: ReviewKind,
    #[serde(default)]
    pub volume: Option<String>,
    /// Path, package, image or volume name.
    pub label: String,
    #[serde(default)]
    pub bytes: Option<u64>,
    #[serde(default)]
    pub created_ms: Option<u64>,
}

/// One volume of `GET /api/v1/space`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeSpace {
    /// The volume label (`system` for the root filesystem).
    pub volume: String,
    /// Where it is mounted.
    pub mount: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
    pub categories: Vec<CategoryUsage>,
    pub sources: Vec<SnapshotSource>,
    /// What the safe actions and the unprotected snapshots of this volume would free.
    #[serde(default)]
    pub reclaimable_bytes: u64,
    /// When the last scan of this volume finished (`None`: never).
    #[serde(default)]
    pub scanned_at_ms: Option<u64>,
    /// How long it took.
    #[serde(default)]
    pub scan_s: u64,
    /// The opt-in automatic thinning is on for this volume.
    #[serde(default)]
    pub auto_free: bool,
    /// Free space is below the low limit ([`SpaceSettings::low_free_pct`]).
    #[serde(default)]
    pub low_free: bool,
    /// The filesystem is btrfs (snapshots are measured).
    #[serde(default)]
    pub btrfs: bool,
}

/// `GET /api/v1/space`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SpaceOverview {
    pub volumes: Vec<VolumeSpace>,
    pub safe_actions: Vec<SafeAction>,
    pub review: Vec<ReviewItem>,
    /// A scan is running.
    #[serde(default)]
    pub scanning: bool,
    /// When the last scan of the whole system finished.
    #[serde(default)]
    pub scanned_at_ms: Option<u64>,
    /// The journal's size limit in force (bytes; `None`: journald's own default).
    #[serde(default)]
    pub journal_cap_bytes: Option<u64>,
    /// That limit is the administrator's own `SystemMaxUse=` (rynas then writes none).
    #[serde(default)]
    pub journal_cap_by_admin: bool,
    /// Volumes whose snapshots changed since their last result: brought up to date when
    /// their disks are next awake (never woken for it).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<String>,
}

/// `GET /api/v1/space/plan`: what "Measure" would do now and about how long it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SpaceScanPlan {
    /// Pairs of neighbouring snapshots to compare (the ones not compared before).
    pub pairs: u32,
    /// Full tree searches: one per line of snapshots without a live folder never measured.
    pub walks: u32,
    /// Metadata those searches read.
    pub walk_tree_bytes: u64,
    /// Sources whose snapshots cannot be compared pair by pair (writable snapshots, a lone
    /// snapshot of a live folder): walked file by file, not in the estimate.
    #[serde(default)]
    pub full_walks: u32,
    /// About how long it takes, in seconds (the pairs and the tree searches).
    pub estimate_s: u64,
    /// The speeds come from earlier measures on this NAS (otherwise a reference NAS).
    #[serde(default)]
    pub measured_here: bool,
}

/// `POST /api/v1/space/preview`: measures exactly what removing these snapshots frees
/// (a task; the answer is read with `GET /api/v1/space/previews/{task_id}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SpacePreviewRequest {
    pub paths: Vec<String>,
}

/// A snapshot a request named that is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusedPath {
    pub path: String,
    /// `None`: not a snapshot Space knows (gone, or never one).
    #[serde(default)]
    pub protected: Option<Protection>,
}

/// `GET /api/v1/space/previews/{task_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SpacePreview {
    /// The snapshots that can go.
    pub paths: Vec<String>,
    pub refused: Vec<RefusedPath>,
    /// Bytes given back by removing `paths` together.
    #[serde(default)]
    pub freed_bytes: Option<u64>,
    /// Bytes only one of them references (given back for sure).
    #[serde(default)]
    pub freed_at_least_bytes: Option<u64>,
    #[serde(default)]
    pub method: MeasureMethod,
    /// The measuring task finished.
    pub done: bool,
    #[serde(default)]
    pub error: Option<String>,
    /// Not measured: measuring took longer than this many minutes (the preview's limit).
    #[serde(default)]
    pub timed_out_minutes: Option<u32>,
}

/// `POST /api/v1/space/delete`: removes the snapshots (a task). `confirm` must be the
/// number of paths: the client says how many it showed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SpaceDeleteRequest {
    pub paths: Vec<String>,
    pub confirm: u32,
}

/// `POST /api/v1/space/review/delete`: removes review items (a task); `confirm` is their
/// number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ReviewDeleteRequest {
    pub ids: Vec<String>,
    pub confirm: u32,
}

/// `POST /api/v1/space/pin` and `/unpin`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PinRequest {
    pub path: String,
}

/// `GET /api/v1/space/pins`: the pinned snapshots (absolute paths).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PinnedSnapshots {
    pub paths: Vec<String>,
}

/// A space budget for one source ([`SnapshotSource::id`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SourceBudget {
    pub source: String,
    #[serde(default)]
    pub max_count: Option<u32>,
    #[serde(default)]
    pub max_age_days: Option<u32>,
    #[serde(default)]
    pub max_bytes: Option<u64>,
}

/// The configuration key [`SPACE_KEY`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceSettings {
    /// Volumes where rynas removes the oldest unprotected snapshots by itself when free
    /// space falls below [`SpaceSettings::low_free_pct`], until it is back above
    /// [`SpaceSettings::recover_pct`]. Empty: notify only (the default).
    #[serde(default)]
    pub auto_free: Vec<String>,
    /// Free space below this percent of the volume: `space.low_free` (and thinning).
    #[serde(default = "default_low")]
    pub low_free_pct: u8,
    /// The low-space state ends (and thinning stops) above this percent.
    #[serde(default = "default_recover")]
    pub recover_pct: u8,
    #[serde(default = "yes")]
    pub notify_low_free: bool,
    /// Weekly `space.snapshots_growth` when snapshots grew more than [`growth_gib`] or
    /// [`growth_pct`] in 7 days.
    ///
    /// [`growth_gib`]: SpaceSettings::growth_gib
    /// [`growth_pct`]: SpaceSettings::growth_pct
    #[serde(default = "yes")]
    pub notify_growth: bool,
    #[serde(default = "default_growth_gib")]
    pub growth_gib: u32,
    #[serde(default = "default_growth_pct")]
    pub growth_pct: u32,
    /// rynas writes a journald drop-in with this cap (MiB); `None`: min(2 GiB, 5 % of the
    /// filesystem of the journal). `Some(0)`: rynas sets no cap (journald's own default).
    #[serde(default)]
    pub journal_cap_mib: Option<u32>,
    /// Empty the recycle bins of the shares and homes every day of what is older than their
    /// retention (a retention of 0 keeps everything).
    #[serde(default = "yes")]
    pub recycle_daily: bool,
    /// Largest size of the thumbnail cache (MiB); the least recently used go first.
    #[serde(default = "default_thumbs")]
    pub thumbs_cap_mib: u32,
    #[serde(default)]
    pub budgets: Vec<SourceBudget>,
}

fn default_low() -> u8 {
    10
}
fn default_recover() -> u8 {
    15
}
fn default_growth_gib() -> u32 {
    50
}
fn default_growth_pct() -> u32 {
    25
}
fn default_thumbs() -> u32 {
    2048
}

impl Default for SpaceSettings {
    fn default() -> Self {
        Self {
            auto_free: Vec::new(),
            low_free_pct: default_low(),
            recover_pct: default_recover(),
            notify_low_free: true,
            notify_growth: true,
            growth_gib: default_growth_gib(),
            growth_pct: default_growth_pct(),
            journal_cap_mib: None,
            recycle_daily: true,
            thumbs_cap_mib: default_thumbs(),
            budgets: Vec::new(),
        }
    }
}

impl SpaceSettings {
    /// The checks of the configuration domain: `(field, message)` of the first problem.
    pub fn problem(&self) -> Option<(&'static str, String)> {
        if !(1..=50).contains(&self.low_free_pct) {
            return Some(("low_free_pct", "the low limit is 1 to 50 percent".into()));
        }
        if self.recover_pct <= self.low_free_pct || self.recover_pct > 90 {
            return Some((
                "recover_pct",
                "the recovery limit is above the low limit and at most 90 percent".into(),
            ));
        }
        if self.growth_gib == 0 && self.growth_pct == 0 {
            return Some(("growth_gib", "a growth limit is needed".into()));
        }
        if self.thumbs_cap_mib < 64 {
            return Some(("thumbs_cap_mib", "the thumbnail cache needs at least 64 MiB".into()));
        }
        let mut seen = std::collections::BTreeSet::new();
        for b in &self.budgets {
            if b.source.is_empty() || b.source.len() > 512 || !seen.insert(b.source.as_str()) {
                return Some(("budgets", format!("invalid or repeated source {:?}", b.source)));
            }
            if b.max_count == Some(0) {
                return Some(("budgets", "a budget keeps at least one snapshot".into()));
            }
        }
        if self.auto_free.iter().any(|v| v.is_empty() || v.len() > 64) {
            return Some(("auto_free", "invalid volume name".into()));
        }
        None
    }

    /// The budget of a source, if any.
    pub fn budget(&self, source: &str) -> Option<&SourceBudget> {
        self.budgets.iter().find(|b| b.source == source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_and_validate() {
        let s = SpaceSettings::default();
        assert!(s.problem().is_none());
        let back: SpaceSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(back, s);
        let mut bad = s.clone();
        bad.recover_pct = bad.low_free_pct;
        assert_eq!(bad.problem().unwrap().0, "recover_pct");
        let mut bad = s;
        bad.budgets = vec![
            SourceBudget { source: "job:a".into(), ..Default::default() },
            SourceBudget { source: "job:a".into(), ..Default::default() },
        ];
        assert_eq!(bad.problem().unwrap().0, "budgets");
    }
}
