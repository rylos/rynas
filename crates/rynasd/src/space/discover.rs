//! Where the snapshots and leftovers of a volume are (Space, `docs/phase2/space.md`).
//!
//! Sources come from two places: the configuration (share jobs, the system restore points,
//! the app stacks of the Docker volume, the PC backup folders) and a short walk of each
//! volume (depth [`MAX_DEPTH`], at most [`MAX_ENTRIES`] directories) that finds what no
//! configuration names any more: `.snapshots/<job>` folders of deleted jobs, folders of
//! received snapshots no PC or replica target owns, and the copies rollbacks, restores and
//! adoptions keep next to what they replaced. Everything goes through [`Probe`], so the
//! tests use canned trees.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rynas_api_types::space::{Protection, ReviewKind, SourceKind};

/// What a directory is when it is a subvolume.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubvolFacts {
    pub readonly: bool,
    pub generation: u64,
    pub otransid: u64,
    pub otime_ms: Option<u64>,
    pub uuid: Option<String>,
    pub parent_uuid: Option<String>,
    pub received_uuid: Option<String>,
}

/// Reads the filesystem (the host, or a canned tree in tests).
pub trait Probe: Send + Sync {
    /// `None`: not a subvolume (or unreadable).
    fn subvol(&self, path: &Path) -> Option<SubvolFacts>;
    /// The directories in `dir` (names), symbolic links left out.
    fn dirs(&self, dir: &Path) -> Vec<String>;
    /// The directories and regular files in `dir`: `(name, is a directory)`, links left out.
    fn entries(&self, dir: &Path) -> Vec<(String, bool)> {
        self.dirs(dir).into_iter().map(|n| (n, true)).collect()
    }
}

/// [`Probe::entries`] of the real filesystem.
pub fn host_entries(dir: &Path) -> Vec<(String, bool)> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<(String, bool)> = rd
        .flatten()
        .filter_map(|e| {
            let t = e.file_type().ok()?;
            (t.is_dir() || t.is_file()).then(|| e.file_name().into_string().ok().map(|n| (n, t.is_dir())))?
        })
        .collect();
    v.sort();
    v
}

/// The machine: subvolume facts from the unprivileged ioctl.
pub struct HostProbe;

impl Probe for HostProbe {
    fn subvol(&self, path: &Path) -> Option<SubvolFacts> {
        let f = rynas_btrfs::ioctl::open_dir(path).ok()?;
        if !rynas_btrfs::ioctl::is_subvolume(&f) {
            return None;
        }
        let i = rynas_btrfs::ioctl::subvol_info(&f).ok()?;
        Some(SubvolFacts {
            readonly: i.readonly,
            generation: i.generation,
            otransid: i.otransid,
            otime_ms: (i.otime > 0).then(|| i.otime as u64 * 1000),
            uuid: i.uuid,
            parent_uuid: i.parent_uuid,
            received_uuid: i.received_uuid,
        })
    }

    fn dirs(&self, dir: &Path) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
        let mut v: Vec<String> = rd
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        v.sort();
        v
    }

    fn entries(&self, dir: &Path) -> Vec<(String, bool)> {
        host_entries(dir)
    }
}

/// A share snapshot job as the configuration has it.
#[derive(Debug, Clone, Default)]
pub struct JobFact {
    pub name: String,
    pub subvolume: String,
    pub snapshot_dir: String,
    /// Most snapshots its retention keeps (sum of the GFS counts), when it has a limit.
    pub keep_count: Option<u32>,
    /// The newest replica of each **enabled** replication target (`None`: the target has
    /// none yet, or its state file is missing or unreadable).
    pub replica_latest: Vec<Option<String>>,
}

/// Whether a replication target may still need snapshot `name` of a job, by the newest
/// replica each enabled target received (`latest`): its latest replica (the parent of the
/// next incremental send) and every snapshot newer than it (not received yet); all of them
/// while a target has none or its state is unknown. The same rule as rynas's own prune of
/// jobs with pins (`jobs::prune_candidates`) and as btrbk, which keeps them.
pub fn replica_needed(name: &str, latest: &[Option<String>]) -> bool {
    latest.iter().any(|l| match l {
        None => true,
        Some(l) => rynas_btrfs::name::compare_names(name, l) != std::cmp::Ordering::Less,
    })
}

/// The system restore points.
#[derive(Debug, Clone, Default)]
pub struct SystemFact {
    pub dir: String,
    pub prefix: String,
    pub subvolume: String,
    pub keep: u32,
    /// UUID of the subvolume the system runs from: its parent is the restore point a
    /// rollback started it from.
    pub running_parent_uuid: Option<String>,
}

/// Everything the configuration says.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub jobs: Vec<JobFact>,
    pub system: Option<SystemFact>,
    /// `(stacks folder, snapshots folder, keep)` of the Docker volume.
    pub docker: Option<(String, String, u32)>,
    /// `(device, folder)` of the PC backup folders rynas knows.
    pub pc_folders: Vec<(String, String)>,
    /// Folders of configured local replica targets.
    pub replica_dirs: Vec<String>,
    pub pins: BTreeSet<String>,
    /// The images each app stack's services run now, by stack (`None`: unknown, the engine
    /// did not answer or was not asked; then no app snapshot is "identical").
    pub stack_images: Option<BTreeMap<String, ImageSet>>,
    /// Live folders a leftover may never be, nor hold: shares, job subvolumes, app folders,
    /// PC backup and replica folders.
    pub live: Vec<PathBuf>,
}

impl Facts {
    /// Fills [`Facts::live`] from the rest and the share folders.
    pub fn with_live(mut self, shares: impl IntoIterator<Item = String>) -> Self {
        let mut v: Vec<PathBuf> = shares.into_iter().map(PathBuf::from).collect();
        v.extend(self.jobs.iter().map(|j| PathBuf::from(&j.subvolume)));
        if let Some((stacks, _, _)) = &self.docker {
            v.push(PathBuf::from(stacks));
        }
        v.extend(self.pc_folders.iter().map(|(_, f)| PathBuf::from(f)));
        v.extend(self.replica_dirs.iter().map(PathBuf::from));
        v.retain(|p| p.is_absolute() && p != Path::new("/"));
        self.live = v;
        self
    }
}

/// One snapshot found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snap {
    pub path: PathBuf,
    pub name: String,
    pub created_ms: Option<u64>,
    pub facts: SubvolFacts,
    pub protected: Option<Protection>,
    pub identical: bool,
}

/// A group of snapshots made by one thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub id: String,
    pub kind: SourceKind,
    pub name: String,
    pub dir: PathBuf,
    /// The live subvolume the snapshots were taken of (what keeps sharing their data).
    pub live: Option<PathBuf>,
    pub keep_count: Option<u32>,
    /// Newest first.
    pub snaps: Vec<Snap>,
}

/// A leftover found by the walk (rollback, restore, adoption, stack copies).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leftover {
    pub kind: ReviewKind,
    pub path: PathBuf,
    pub created_ms: Option<u64>,
}

/// How deep the volume walk goes below the mount point.
pub const MAX_DEPTH: usize = 4;
/// Most directories the walk opens per volume.
pub const MAX_ENTRIES: usize = 20_000;

/// Folders the walk never enters.
const SKIP: &[&str] = &["#recycle", "@eaDir", ".rynas-crypt", "lost+found"];
/// The Docker engine's own data inside `<volume>/docker` (layers, containers).
const ENGINE: &[&str] = &["engine", "containerd"];

fn ms_of_stamp(s: &str) -> Option<u64> {
    // YYYYMMDDTHHMMSS or YYYYMMDDTHHMM (local time is close enough for an age)
    let (d, t) = s.split_once('T')?;
    if d.len() != 8 || !(t.len() == 4 || t.len() == 6) || !d.chars().chain(t.chars()).all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n = |a: &str| a.parse::<i64>().ok();
    let (y, mo, da) = (n(&d[0..4])?, n(&d[4..6])?, n(&d[6..8])?);
    let (h, mi) = (n(&t[0..2])?, n(&t[2..4])?);
    let sec = if t.len() == 6 { n(&t[4..6])? } else { 0 };
    let date = jiff::civil::Date::new(y as i16, mo as i8, da as i8).ok()?;
    let dt = date.at(h as i8, mi as i8, sec as i8, 0);
    let ts = dt.to_zoned(jiff::tz::TimeZone::UTC).ok()?.timestamp();
    u64::try_from(ts.as_millisecond()).ok()
}

/// The leftover a name is, with its time.
pub fn leftover_of(name: &str) -> Option<(ReviewKind, Option<u64>)> {
    if let Some((_, stamp)) = name.rsplit_once(".rynas-rollback-") {
        return ms_of_stamp(stamp).map(|t| (ReviewKind::RollbackCopy, Some(t)));
    }
    if let Some((_, stamp)) = name.rsplit_once(".rynas-before-restore-") {
        return ms_of_stamp(stamp).map(|t| (ReviewKind::RestoreCopy, Some(t)));
    }
    if let Some((base, ms)) = name.rsplit_once(".before-adopt-")
        && !base.is_empty()
        && !ms.is_empty()
        && ms.chars().all(|c| c.is_ascii_digit())
    {
        return Some((ReviewKind::AdoptCopy, ms.parse().ok()));
    }
    if let Some(stack) = name.strip_prefix('.').and_then(|n| n.strip_suffix(".rynas-old"))
        && !stack.is_empty()
    {
        return Some((ReviewKind::StackCopy, None));
    }
    None
}

fn time_of(name: &str, f: &SubvolFacts) -> Option<u64> {
    rynas_btrfs::name::split_snapshot_name(name)
        .and_then(|(_, ts)| rynas_btrfs::name::parse_timestamp(ts))
        .map(|s| s.epoch_ms().max(0) as u64)
        .or_else(|| name.split('.').next_back().and_then(ms_of_stamp))
        .or(f.otime_ms)
}

/// The subvolumes in `dir` whose name `keep` accepts, newest first.
fn snaps_in(p: &dyn Probe, dir: &Path, keep: &dyn Fn(&str) -> bool) -> Vec<Snap> {
    let mut v: Vec<Snap> = p
        .dirs(dir)
        .into_iter()
        .filter(|n| keep(n))
        .filter_map(|n| {
            let path = dir.join(&n);
            let facts = p.subvol(&path)?;
            Some(Snap { created_ms: time_of(&n, &facts), path, name: n, facts, protected: None, identical: false })
        })
        .collect();
    v.sort_by(|a, b| b.created_ms.cmp(&a.created_ms).then_with(|| b.name.cmp(&a.name)));
    v
}

fn is_job_snapshot(job: &str) -> impl Fn(&str) -> bool + '_ {
    move |n: &str| rynas_btrfs::name::split_snapshot_name(n).is_some_and(|(p, _)| p == job)
}

/// The configured sources: share jobs, system restore points (also when they are off),
/// app stacks, PC backup folders.
pub fn configured(f: &Facts, p: &dyn Probe) -> Vec<Source> {
    let mut out = Vec::new();
    for j in &f.jobs {
        let dir = PathBuf::from(&j.snapshot_dir);
        out.push(Source {
            id: format!("job:{}", j.name),
            kind: SourceKind::ShareJob,
            name: j.name.clone(),
            snaps: snaps_in(p, &dir, &is_job_snapshot(&j.name)),
            dir,
            live: Some(PathBuf::from(&j.subvolume)),
            keep_count: j.keep_count,
        });
    }
    if let Some(s) = &f.system {
        let dir = PathBuf::from(&s.dir);
        out.push(Source {
            id: "system".into(),
            kind: SourceKind::SystemPoints,
            name: s.prefix.clone(),
            snaps: snaps_in(p, &dir, &is_job_snapshot(&s.prefix)),
            dir,
            live: Some(PathBuf::from(&s.subvolume)),
            keep_count: Some(s.keep),
        });
    }
    if let Some((stacks, snapshots, keep)) = &f.docker {
        let root = PathBuf::from(snapshots);
        for stack in p.dirs(&root) {
            let dir = root.join(&stack);
            let snaps = snaps_in(p, &dir, &|n| rynas_docker::parse_stamp(n).is_some());
            if snaps.is_empty() {
                continue;
            }
            out.push(Source {
                id: format!("stack:{stack}"),
                kind: SourceKind::AppStack,
                live: Some(Path::new(stacks).join(&stack)),
                name: stack,
                dir,
                keep_count: Some(*keep),
                snaps,
            });
        }
    }
    for (device, folder) in &f.pc_folders {
        let dir = PathBuf::from(folder);
        let snaps = snaps_in(p, &dir, &|n| !n.starts_with('.'));
        out.push(Source { id: format!("pc:{device}"), kind: SourceKind::PcBackup, name: device.clone(), dir, live: None, keep_count: None, snaps });
    }
    out
}

/// What the walk of one volume found besides the configured sources.
#[derive(Debug, Default)]
pub struct Found {
    pub sources: Vec<Source>,
    pub leftovers: Vec<Leftover>,
}

/// Walks `base` (a volume's folder) for orphaned job snapshots, unowned received snapshots
/// and leftovers. `known` are the folders of the configured sources (never reported again).
pub fn walk(base: &Path, known: &BTreeSet<PathBuf>, f: &Facts, p: &dyn Probe) -> Found {
    let mut found = Found::default();
    let mut opened = 0usize;
    let mut stack: Vec<(PathBuf, usize)> = vec![(base.to_path_buf(), 0)];
    let replica_dirs: BTreeSet<PathBuf> = f.replica_dirs.iter().map(PathBuf::from).collect();
    let docker_snapshots = f.docker.as_ref().map(|(_, s, _)| PathBuf::from(s));
    while let Some((dir, depth)) = stack.pop() {
        opened += 1;
        if opened > MAX_ENTRIES {
            break;
        }
        let entries = p.entries(&dir);
        // a folder of received snapshots nothing configured owns. rynas keeps no record of
        // removed PCs or targets, so it is never called one: replicas another NAS sends here
        // (`.replicas/<name>`) are protected, anything else is "received, owner unknown"
        if depth > 0 && !known.contains(&dir) && !replica_dirs.contains(&dir) {
            let received: Vec<Snap> = snaps_in(p, &dir, &|n| !n.starts_with('.'))
                .into_iter()
                .filter(|s| s.facts.received_uuid.is_some())
                .collect();
            if !received.is_empty() {
                let incoming = dir.parent().and_then(|x| x.file_name()).is_some_and(|n| n == ".replicas");
                let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                found.sources.push(Source {
                    id: format!("{}:{}", if incoming { "incoming" } else { "received" }, dir.display()),
                    kind: if incoming { SourceKind::IncomingReplica } else { SourceKind::Received },
                    name,
                    dir: dir.clone(),
                    live: None,
                    keep_count: None,
                    snaps: received,
                });
                continue;
            }
        }
        for (n, is_dir) in entries {
            let path = dir.join(&n);
            if let Some((kind, created_ms)) = leftover_of(&n) {
                // a regular file only as the copy a restore keeps
                let file_ok = !is_dir && kind == ReviewKind::RestoreCopy;
                // never something live: a share, a job's subvolume, an app folder, a PC or
                // replica folder, or a folder holding one
                if (is_dir || file_ok) && !f.live.iter().any(|l| l.starts_with(&path)) {
                    found.leftovers.push(Leftover { kind, path, created_ms });
                }
                continue;
            }
            if !is_dir {
                continue;
            }
            if n == ".snapshots" {
                if Some(&path) != docker_snapshots.as_ref() {
                    orphans(&path, known, p, &mut found);
                }
                continue;
            }
            let engine_data = ENGINE.contains(&n.as_str()) && dir.file_name().is_some_and(|d| d == "docker");
            if engine_data || SKIP.contains(&n.as_str()) || known.contains(&path) || depth + 1 > MAX_DEPTH {
                continue;
            }
            stack.push((path, depth + 1));
        }
    }
    found
}

/// The `.snapshots/<name>` folders no job owns: snapshots of deleted jobs.
fn orphans(dot: &Path, known: &BTreeSet<PathBuf>, p: &dyn Probe, found: &mut Found) {
    for job in p.dirs(dot) {
        let dir = dot.join(&job);
        if known.contains(&dir) {
            continue;
        }
        let snaps = snaps_in(p, &dir, &is_job_snapshot(&job));
        if snaps.is_empty() {
            continue;
        }
        found.sources.push(Source {
            id: format!("orphan:{}", dir.display()),
            kind: SourceKind::OrphanJob,
            name: job,
            dir,
            live: None,
            keep_count: None,
            snaps,
        });
    }
}

/// Marks what may not be removed, in the order a person reads it: pinned, the restore
/// point the system runs from, a replication parent, the newest complete PC backup, the
/// newest of the source, a writable subvolume.
pub fn protect(s: &mut Source, f: &Facts, pc_parents: &BTreeSet<String>) {
    let job = f.jobs.iter().find(|j| s.kind == SourceKind::ShareJob && j.name == s.name);
    let boot = f.system.as_ref().and_then(|x| x.running_parent_uuid.clone()).filter(|_| s.kind == SourceKind::SystemPoints);
    let newest_ro = s.snaps.iter().position(|x| x.facts.readonly);
    let newest_applies = matches!(s.kind, SourceKind::ShareJob | SourceKind::AppStack | SourceKind::SystemPoints);
    // received snapshots without a known owner: the newest of each line (the snapshots of
    // one sending subvolume, `<name>.<time>`) may be the parent of the next receive
    let mut lines_seen: BTreeSet<String> = BTreeSet::new();
    let newest_of_line: Vec<bool> = s
        .snaps
        .iter()
        .map(|x| s.kind == SourceKind::Received && x.facts.readonly && lines_seen.insert(lineage(&x.name)))
        .collect();
    for (i, x) in s.snaps.iter_mut().enumerate() {
        let path = x.path.display().to_string();
        x.protected = if s.kind == SourceKind::IncomingReplica {
            Some(Protection::IncomingReplica)
        } else if f.pins.contains(&path) {
            Some(Protection::Pinned)
        } else if newest_of_line[i] {
            Some(Protection::Newest)
        } else if boot.is_some() && x.facts.uuid == boot {
            Some(Protection::BootReference)
        } else if job.is_some_and(|j| replica_needed(&x.name, &j.replica_latest)) {
            Some(Protection::ReplicationParent)
        } else if s.kind == SourceKind::PcBackup && pc_parents.contains(&x.name) {
            Some(Protection::NewestBackup)
        } else if !x.facts.readonly {
            Some(Protection::Writable)
        } else if newest_applies && Some(i) == newest_ro {
            Some(Protection::Newest)
        } else {
            None
        };
    }
}

/// The line a received snapshot belongs to: its name without the time (`root.20261101T0100`
/// → `root`; btrbk's long names, PC agent names).
pub fn lineage(name: &str) -> String {
    rynas_btrfs::name::split_snapshot_name(name)
        .map(|(p, _)| p.to_string())
        .or_else(|| name.rsplit_once('.').map(|(p, _)| p.to_string()))
        .unwrap_or_else(|| name.to_string())
}

/// App stack snapshots identical to the stack folder now: the folder's generation is not
/// newer than the transaction that made the snapshot (btrbk's "onchange" rule). Only marks
/// the flag; the newest stays protected anyway.
///
/// Checked on a real btrfs (Debian 13, kernel 7.3, a 4-bay NAS, 2026-11-24): taking a read-only
/// snapshot does not bump the source subvolume's generation (it stayed 1645 across two
/// snapshots with otransid 1646 and 1647), and any write after a snapshot raises it above
/// that snapshot's otransid.
///
/// The folder is only half of a rollback point: the snapshot's metadata (`<id>.json`) records
/// the images the services ran. A snapshot is identical only when those images (service and
/// image id) are also the ones the stack runs now (`images`, by stack; `None`: unknown, so
/// nothing is identical); otherwise it is the way back to the older images.
pub fn mark_identical(s: &mut Source, live: Option<&SubvolFacts>, images: Option<&BTreeMap<String, ImageSet>>) {
    let Some(live) = live.filter(|_| s.kind == SourceKind::AppStack) else { return };
    let Some(images) = images else { return };
    let empty = ImageSet::new();
    let now = images.get(&s.name).unwrap_or(&empty);
    for x in &mut s.snaps {
        let same_folder = x.facts.readonly && x.facts.otransid > 0 && live.generation <= x.facts.otransid;
        x.identical = same_folder && recorded_images(&s.dir, &x.name).is_some_and(|r| r == *now);
    }
}

/// `(service, image id)` pairs: the images a stack runs.
pub type ImageSet = BTreeSet<(String, String)>;

/// The images an app stack's services ran, from a list of them.
pub fn image_set(v: &[rynas_api_types::docker::ServiceImage]) -> ImageSet {
    v.iter().filter(|i| !i.image_id.is_empty()).map(|i| (i.service.clone(), i.image_id.clone())).collect()
}

/// The images a stack snapshot recorded (`<dir>/<id>.json`); `None` when the file is missing
/// or does not read.
fn recorded_images(dir: &Path, id: &str) -> Option<ImageSet> {
    let t = std::fs::read_to_string(dir.join(format!("{id}.json"))).ok()?;
    let m: rynas_docker::snapshots::SnapshotMeta = serde_json::from_str(&t).ok()?;
    Some(image_set(&m.images))
}

/// The volume (label) a path belongs to: the longest mount point containing it.
pub fn volume_of<'a>(path: &Path, mounts: &'a [(String, PathBuf)]) -> Option<&'a str> {
    mounts
        .iter()
        .filter(|(_, m)| path.starts_with(m))
        .max_by_key(|(_, m)| m.as_os_str().len())
        .map(|(l, _)| l.as_str())
}

/// Groups sources by volume.
pub fn by_volume(sources: Vec<Source>, mounts: &[(String, PathBuf)]) -> BTreeMap<String, Vec<Source>> {
    let mut m: BTreeMap<String, Vec<Source>> = BTreeMap::new();
    for s in sources {
        if let Some(v) = volume_of(&s.dir, mounts) {
            m.entry(v.to_string()).or_default().push(s);
        }
    }
    m
}

#[cfg(test)]
pub mod fake {
    //! A canned tree: a directory is a subvolume when it holds `.subvol` (JSON of
    //! [`super::SubvolFacts`] fields, all optional).
    use super::*;

    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct Marker {
        readonly: bool,
        generation: u64,
        otransid: u64,
        uuid: Option<String>,
        parent_uuid: Option<String>,
        received_uuid: Option<String>,
    }

    pub struct TreeProbe;

    impl Probe for TreeProbe {
        fn subvol(&self, path: &Path) -> Option<SubvolFacts> {
            let t = std::fs::read_to_string(path.join(".subvol")).ok()?;
            let m: Marker = serde_json::from_str(&t).unwrap_or_default();
            Some(SubvolFacts {
                readonly: m.readonly,
                generation: m.generation,
                otransid: m.otransid,
                otime_ms: None,
                uuid: m.uuid,
                parent_uuid: m.parent_uuid,
                received_uuid: m.received_uuid,
            })
        }
        fn dirs(&self, dir: &Path) -> Vec<String> {
            HostProbe.dirs(dir)
        }
        fn entries(&self, dir: &Path) -> Vec<(String, bool)> {
            host_entries(dir)
        }
    }

    /// Makes `path` a fake subvolume with `json` facts.
    pub fn subvol(path: &Path, json: &str) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join(".subvol"), json).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{TreeProbe, subvol};
    use super::*;

    const RO: &str = r#"{"readonly": true}"#;

    fn tree() -> (tempfile::TempDir, Facts) {
        let d = tempfile::tempdir().unwrap();
        let v = d.path();
        // a configured job with three snapshots, the middle one a replication parent
        subvol(&v.join("docs"), r#"{"generation": 50}"#);
        for t in ["20261101T0100", "20261102T0100", "20261103T0100"] {
            subvol(&v.join(format!(".snapshots/docs/docs.{t}")), RO);
        }
        // a deleted job's snapshots
        for t in ["20261001T0100", "20261002T0100"] {
            subvol(&v.join(format!(".snapshots/old/old.{t}")), RO);
        }
        // app stacks: one stack, the newest two identical to the folder (generation 7)
        subvol(&v.join("docker/stacks/web"), r#"{"generation": 7}"#);
        subvol(&v.join("docker/.snapshots/web/20261101T010000"), r#"{"readonly": true, "otransid": 5}"#);
        subvol(&v.join("docker/.snapshots/web/20261102T010000"), r#"{"readonly": true, "otransid": 7}"#);
        subvol(&v.join("docker/.snapshots/web/20261103T010000"), r#"{"readonly": true, "otransid": 9}"#);
        // leftovers
        std::fs::create_dir_all(v.join("docker/stacks/.web.rynas-old")).unwrap();
        std::fs::create_dir_all(v.join("docs.rynas-rollback-20261105T101010")).unwrap();
        std::fs::create_dir_all(v.join("docs/report.pdf.rynas-before-restore-20261106T101010")).unwrap();
        std::fs::create_dir_all(v.join("docker/stacks/sync/config.before-adopt-1795000000000")).unwrap();
        // received snapshots: a known PC folder, a removed PC, a removed replica target
        subvol(&v.join("backups/laptop/root.20261101T0100"), r#"{"readonly": true, "received_uuid": "r1"}"#);
        subvol(&v.join("backups/oldpc/root.20261001T0100"), r#"{"readonly": true, "received_uuid": "r2"}"#);
        subvol(&v.join("backups/oldpc/root.20261002T0100"), r#"{"readonly": true, "received_uuid": "r4"}"#);
        subvol(&v.join("backups/oldpc/home.20261002T0100"), r#"{"readonly": true, "received_uuid": "r5"}"#);
        subvol(&v.join(".replicas/gone/docs.20261001T0100"), r#"{"readonly": true, "received_uuid": "r3"}"#);
        let f = Facts {
            jobs: vec![JobFact {
                name: "docs".into(),
                subvolume: v.join("docs").display().to_string(),
                snapshot_dir: v.join(".snapshots/docs").display().to_string(),
                keep_count: Some(10),
                replica_latest: vec![Some("docs.20261102T0100".to_string())],
            }],
            docker: Some((v.join("docker/stacks").display().to_string(), v.join("docker/.snapshots").display().to_string(), 5)),
            pc_folders: vec![("laptop".into(), v.join("backups/laptop").display().to_string())],
            ..Default::default()
        };
        (d, f)
    }

    fn known(sources: &[Source]) -> BTreeSet<PathBuf> {
        sources.iter().map(|s| s.dir.clone()).collect()
    }

    #[test]
    fn finds_configured_orphaned_and_unowned_sources() {
        let (d, f) = tree();
        let conf = configured(&f, &TreeProbe);
        let ids: Vec<&str> = conf.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["job:docs", "stack:web", "pc:laptop"]);
        assert_eq!(conf[0].snaps.len(), 3);
        assert_eq!(conf[0].snaps[0].name, "docs.20261103T0100", "newest first");
        let found = walk(d.path(), &known(&conf), &f, &TreeProbe);
        let mut ids: Vec<(String, SourceKind)> = found.sources.iter().map(|s| (s.name.clone(), s.kind)).collect();
        ids.sort_by(|a, b| a.0.cmp(&b.0));
        // without a record of a removed target or PC: an incoming replica and "owner unknown"
        assert_eq!(
            ids,
            [("gone".into(), SourceKind::IncomingReplica), ("old".into(), SourceKind::OrphanJob), ("oldpc".into(), SourceKind::Received)]
        );
        let mut kinds: Vec<ReviewKind> = found.leftovers.iter().map(|l| l.kind).collect();
        kinds.sort_by_key(|k| format!("{k:?}"));
        assert_eq!(kinds, [ReviewKind::AdoptCopy, ReviewKind::RestoreCopy, ReviewKind::RollbackCopy, ReviewKind::StackCopy]);
        // the configured PC folder is never reported as unowned
        assert!(!found.sources.iter().any(|s| s.name == "laptop"));
        // incoming replicas are never removable; received ones keep the newest of each line
        let mut incoming = found.sources.iter().find(|s| s.name == "gone").unwrap().clone();
        protect(&mut incoming, &f, &BTreeSet::new());
        assert!(incoming.snaps.iter().all(|s| s.protected == Some(Protection::IncomingReplica)));
        let mut received = found.sources.iter().find(|s| s.name == "oldpc").unwrap().clone();
        protect(&mut received, &f, &BTreeSet::new());
        let p: Vec<(String, Option<Protection>)> = received.snaps.iter().map(|s| (s.name.clone(), s.protected)).collect();
        assert_eq!(
            p,
            [
                ("root.20261002T0100".to_string(), Some(Protection::Newest)),
                ("home.20261002T0100".to_string(), Some(Protection::Newest)),
                ("root.20261001T0100".to_string(), None),
            ]
        );
    }

    #[test]
    fn leftovers_are_never_live() {
        let (d, mut f) = tree();
        let v = d.path();
        // a "rollback copy" that is the share itself, one that holds a share, a restore copy as
        // a file; a rollback "copy" file is not one
        std::fs::create_dir_all(v.join("shares.rynas-rollback-20261105T101010/music")).unwrap();
        std::fs::create_dir_all(v.join("live.rynas-rollback-20261105T101010")).unwrap();
        std::fs::write(v.join("docs/a.txt.rynas-before-restore-20261106T101010"), b"x").unwrap();
        std::fs::write(v.join("x.rynas-rollback-20261106T101010"), b"x").unwrap();
        f = f.with_live([
            v.join("shares.rynas-rollback-20261105T101010/music").display().to_string(),
            v.join("live.rynas-rollback-20261105T101010").display().to_string(),
        ]);
        let conf = configured(&f, &TreeProbe);
        let found = walk(v, &known(&conf), &f, &TreeProbe);
        let paths: Vec<String> = found.leftovers.iter().map(|l| l.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert!(!paths.iter().any(|p| p.starts_with("shares.") || p.starts_with("live.") || p.starts_with("x.")), "{paths:?}");
        assert!(paths.contains(&"a.txt.rynas-before-restore-20261106T101010".to_string()), "{paths:?}");
    }

    #[test]
    fn protections_never_newest_parent_or_pinned() {
        let (_d, mut f) = tree();
        let conf = configured(&f, &TreeProbe);
        let oldest = conf[0].snaps[2].path.display().to_string();
        f.pins.insert(oldest);
        let mut job = conf[0].clone();
        protect(&mut job, &f, &BTreeSet::new());
        let p: Vec<Option<Protection>> = job.snaps.iter().map(|s| s.protected).collect();
        // the target's latest replica and everything newer (not received yet)
        assert_eq!(p, [Some(Protection::ReplicationParent), Some(Protection::ReplicationParent), Some(Protection::Pinned)]);
        // an orphan: nothing protects the newest, only pins and writable ones
        let mut orphan = job.clone();
        orphan.kind = SourceKind::OrphanJob;
        f.pins.clear();
        protect(&mut orphan, &f, &BTreeSet::new());
        assert!(orphan.snaps.iter().all(|s| s.protected.is_none()));
        // a PC folder: the parents of the next incremental
        let mut pc = conf[2].clone();
        protect(&mut pc, &f, &["root.20261101T0100".to_string()].into());
        assert_eq!(pc.snaps[0].protected, Some(Protection::NewestBackup));
    }

    #[test]
    fn replication_keeps_what_a_target_still_needs() {
        let (_d, mut f) = tree();
        let conf = configured(&f, &TreeProbe);
        let run = |f: &Facts| {
            let mut job = conf[0].clone();
            protect(&mut job, f, &BTreeSet::new());
            job.snaps.iter().map(|s| s.protected).collect::<Vec<_>>()
        };
        let r = Some(Protection::ReplicationParent);
        // two targets: the older latest replica decides
        f.jobs[0].replica_latest = vec![Some("docs.20261103T0100".into()), Some("docs.20261101T0100".into())];
        assert_eq!(run(&f), [r, r, r]);
        // every target has the newest: the older ones may go, the newest stays
        f.jobs[0].replica_latest = vec![Some("docs.20261103T0100".into())];
        assert_eq!(run(&f), [r, None, None]);
        // a target without a replica yet (or an unreadable state file): all of them
        f.jobs[0].replica_latest = vec![Some("docs.20261103T0100".into()), None];
        assert_eq!(run(&f), [r, r, r]);
        // no enabled target: only the newest
        f.jobs[0].replica_latest = Vec::new();
        assert_eq!(run(&f), [Some(Protection::Newest), None, None]);
        assert!(replica_needed("docs.20261104T0100", &[Some("docs.20261103T0100".into())]));
        assert!(!replica_needed("docs.20261102T0100", &[Some("docs.20261103T0100".into())]));
    }

    #[test]
    fn identical_stack_snapshots_follow_the_generation() {
        let (_d, f) = tree();
        let conf = configured(&f, &TreeProbe);
        let live = TreeProbe.subvol(conf[1].live.as_ref().unwrap());
        let meta = |id: &str, image_id: &str| {
            let m = format!(r#"{{"reason":"update","created_s":1,"images":[{{"service":"app","image":"web:1","image_id":"{image_id}","digest":null}}]}}"#);
            std::fs::write(conf[1].dir.join(format!("{id}.json")), m).unwrap();
        };
        meta("20261103T010000", "sha256:new");
        meta("20261102T010000", "sha256:new");
        meta("20261101T010000", "sha256:new");
        let now: BTreeMap<String, ImageSet> = [("web".to_string(), [("app".to_string(), "sha256:new".to_string())].into())].into();
        let run = |images: Option<&BTreeMap<String, ImageSet>>| {
            let mut stack = conf[1].clone();
            mark_identical(&mut stack, live.as_ref(), images);
            protect(&mut stack, &f, &BTreeSet::new());
            stack.snaps.iter().map(|s| (s.identical, s.protected)).collect::<Vec<_>>()
        };
        // generation 7: snapshots taken at transaction 7 and 9 saw the same folder
        assert_eq!(run(Some(&now)), [(true, Some(Protection::Newest)), (true, None), (false, None)]);
        // the images the stack runs now are unknown: nothing is identical
        assert_eq!(run(None), [(false, Some(Protection::Newest)), (false, None), (false, None)]);
        // the middle one recorded older images: a rollback point, not identical
        meta("20261102T010000", "sha256:old");
        assert_eq!(run(Some(&now)), [(true, Some(Protection::Newest)), (false, None), (false, None)]);
        // no metadata: not identical either
        std::fs::remove_file(conf[1].dir.join("20261103T010000.json")).unwrap();
        assert!(!run(Some(&now))[0].0);
    }

    #[test]
    fn system_points_protect_the_one_the_system_runs_from() {
        let d = tempfile::tempdir().unwrap();
        subvol(&d.path().join("rootfs.20261101T0100"), r#"{"readonly": true, "uuid": "a"}"#);
        subvol(&d.path().join("rootfs.20261102T0100"), r#"{"readonly": true, "uuid": "b"}"#);
        subvol(&d.path().join("rootfs.20261103T0100"), r#"{"readonly": true, "uuid": "c"}"#);
        let f = Facts {
            system: Some(SystemFact {
                dir: d.path().display().to_string(),
                prefix: "rootfs".into(),
                subvolume: "/".into(),
                keep: 10,
                running_parent_uuid: Some("a".into()),
            }),
            ..Default::default()
        };
        let mut s = configured(&f, &TreeProbe).remove(0);
        protect(&mut s, &f, &BTreeSet::new());
        let p: Vec<Option<Protection>> = s.snaps.iter().map(|x| x.protected).collect();
        assert_eq!(p, [Some(Protection::Newest), None, Some(Protection::BootReference)]);
    }

    #[test]
    fn leftover_names() {
        assert_eq!(leftover_of("@rootfs.rynas-rollback-20261101T101010").map(|x| x.0), Some(ReviewKind::RollbackCopy));
        assert_eq!(leftover_of("a.txt.rynas-before-restore-20261101T101010").map(|x| x.0), Some(ReviewKind::RestoreCopy));
        assert_eq!(leftover_of("config.before-adopt-17").map(|x| x.0), Some(ReviewKind::AdoptCopy));
        assert_eq!(leftover_of(".web.rynas-old").map(|x| x.0), Some(ReviewKind::StackCopy));
        assert_eq!(leftover_of("docs.rynas-rollback-garbage"), None);
        assert_eq!(leftover_of("config.before-adopt-x"), None);
        assert_eq!(leftover_of("plain"), None);
        assert!(leftover_of("x.rynas-rollback-20261101T101010").unwrap().1.is_some());
    }

    #[test]
    fn volumes_by_longest_mount() {
        let mounts = vec![("system".to_string(), PathBuf::from("/")), ("vol1".to_string(), PathBuf::from("/vol1"))];
        assert_eq!(volume_of(Path::new("/vol1/.snapshots/x"), &mounts), Some("vol1"));
        assert_eq!(volume_of(Path::new("/.snapshots/rootfs"), &mounts), Some("system"));
        assert_eq!(volume_of(Path::new("/vol10/x"), &mounts), Some("system"));
    }
}
