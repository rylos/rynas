//! The `snapshots` configuration domain: one btrbk configuration per job
//! (`/etc/rynas/btrbk/<job>.conf`, managed header) and the apt hook of the system restore
//! points (`/etc/apt/apt.conf.d/80rynas-snapshot`).
//!
//! The schedule of a job is not here: it is the scheduler task `snapshot-<job>` (key
//! `scheduler`), staged together with the job by rynasd. Removing a job removes its
//! configuration, never its snapshots.

use std::path::Path;

use rynas_api_types::snapshots::{SnapshotJob, SnapshotsConfig};
use rynas_config::files::HEADER_PREFIX;
use rynas_config::{Applier, ApplyCtx, Candidate, ManagedFile};
use rynas_tasks::Cmd;
use serde_json::Value;

use crate::btrbk::{self, clean_abs, conf_path, layout, normalize_retention, valid_min};
use crate::ioctl;

pub const SNAPSHOTS_KEY: &str = "snapshots";
pub const APT_HOOK: &str = "/etc/apt/apt.conf.d/80rynas-snapshot";
/// Where rynasctl is installed (the apt hook and the scheduler units call it).
pub const RYNASCTL: &str = "/usr/libexec/rynas/rynasctl";

/// Facts about paths the domain checks after an apply, behind a trait so that tests and
/// `--dry-system` need no btrfs.
pub trait SnapshotHost: Send + Sync {
    /// `path` is the top of a btrfs subvolume.
    fn is_subvolume(&self, path: &str) -> bool;
    /// Both paths are on the same btrfs filesystem.
    fn same_filesystem(&self, a: &str, b: &str) -> bool;
    /// Creates a snapshot directory (and its parents), mode 0755.
    fn ensure_dir(&self, path: &str) -> Result<(), String>;
}

/// The real filesystem.
pub struct HostSnapshots;

impl SnapshotHost for HostSnapshots {
    fn is_subvolume(&self, path: &str) -> bool {
        ioctl::open_dir(Path::new(path)).is_ok_and(|f| ioctl::is_subvolume(&f))
    }

    fn same_filesystem(&self, a: &str, b: &str) -> bool {
        let id = |p: &str| {
            ioctl::open_dir(Path::new(p))
                .and_then(|f| ioctl::fs_uuid(&f))
                .ok()
        };
        matches!((id(a), id(b)), (Some(x), Some(y)) if x == y)
    }

    fn ensure_dir(&self, path: &str) -> Result<(), String> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(path)
            .map_err(|e| format!("{path}: {e}"))
    }
}

/// Tests and `--dry-system`: subvolumes are declared (or probed on the host, read-only),
/// directories are only recorded.
#[derive(Default)]
pub struct MemorySnapshots {
    probe_host: bool,
    pub state: std::sync::Mutex<MemorySnapshotsState>,
}

#[derive(Default, Debug)]
pub struct MemorySnapshotsState {
    pub subvolumes: Vec<String>,
    pub dirs: Vec<String>,
}

impl MemorySnapshots {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn probing_host() -> Self {
        Self {
            probe_host: true,
            ..Default::default()
        }
    }

    pub fn add_subvolume(&self, p: &str) {
        self.state.lock().unwrap().subvolumes.push(p.into());
    }
}

impl SnapshotHost for MemorySnapshots {
    fn is_subvolume(&self, path: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .subvolumes
            .iter()
            .any(|s| s == path)
            || (self.probe_host && HostSnapshots.is_subvolume(path))
    }

    fn same_filesystem(&self, a: &str, b: &str) -> bool {
        !self.probe_host || HostSnapshots.same_filesystem(a, b) || !Path::new(b).exists()
    }

    fn ensure_dir(&self, path: &str) -> Result<(), String> {
        tracing::info!("dry system: mkdir -p {path}");
        self.state.lock().unwrap().dirs.push(path.into());
        Ok(())
    }
}

/// The domain.
pub struct SnapshotsDomain {
    host: std::sync::Arc<dyn SnapshotHost>,
}

impl SnapshotsDomain {
    pub fn new(host: std::sync::Arc<dyn SnapshotHost>) -> Self {
        Self { host }
    }

    pub fn host(&self) -> &dyn SnapshotHost {
        self.host.as_ref()
    }
}

/// A job name: `[A-Za-z0-9][A-Za-z0-9_-]{0,47}`.
pub fn valid_job_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 48
        && n.as_bytes()[0].is_ascii_alphanumeric()
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The default snapshot directory of a subvolume: `<parent>/.snapshots/<job>`.
pub fn default_snapshot_dir(subvolume: &str, job: &str) -> String {
    let parent = Path::new(subvolume)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".into());
    if parent == "/" {
        format!("/.snapshots/{job}")
    } else {
        format!("{parent}/.snapshots/{job}")
    }
}

/// Validates and normalizes one job (defaults filled, preset applied).
pub fn normalize_job(mut j: SnapshotJob) -> Result<SnapshotJob, String> {
    j.name = j.name.trim().to_string();
    if !valid_job_name(&j.name) {
        return Err(format!(
            "invalid job name {:?}: 1-48 letters, digits, '_' and '-', starting with a letter or digit",
            j.name
        ));
    }
    let n = &j.name;
    if let Some(s) = &j.share
        && (s.is_empty() || s.len() > 64 || s.chars().any(|c| c.is_control() || c == '/'))
    {
        return Err(format!("job {n}: invalid share name {s:?}"));
    }
    if j.subvolume.is_empty() || j.subvolume == "/" || !clean_abs(&j.subvolume) {
        return Err(format!(
            "job {n}: the subvolume must be an absolute path below / (got {:?})",
            j.subvolume
        ));
    }
    if j.snapshot_dir.is_empty() {
        j.snapshot_dir = default_snapshot_dir(&j.subvolume, n);
    }
    if !clean_abs(&j.snapshot_dir) || j.snapshot_dir == "/" {
        return Err(format!(
            "job {n}: invalid snapshot directory {:?}",
            j.snapshot_dir
        ));
    }
    layout(&j).map_err(|e| format!("job {n}: {e}"))?;
    j.retention = normalize_retention(&j.retention);
    let r = &j.retention;
    if [r.hourly, r.daily, r.weekly, r.monthly, r.yearly]
        .iter()
        .any(|&c| c > 10_000)
    {
        return Err(format!("job {n}: retention counts are at most 10000"));
    }
    if !valid_min(&r.min) {
        return Err(format!(
            "job {n}: invalid minimum {:?} (all, latest, or a number followed by h, d, w, m or y)",
            r.min
        ));
    }
    j.replicas = crate::replication::normalize_targets(&j)?;
    Ok(j)
}

fn parse(v: &Value) -> Result<SnapshotsConfig, String> {
    serde_json::from_value(v.clone()).map_err(|e| format!("invalid snapshots document: {e}"))
}

/// The apt hook line: never fails apt (`|| true`), works without rynasd.
pub fn apt_hook(c: &SnapshotsConfig) -> String {
    let s = &c.system;
    format!(
        "// System restore point before dpkg runs: rynas, `snapshots` configuration key.\n\
         // Generated by rynas: change the restore points in rynas, edits here are drift.\n\
         DPkg::Pre-Invoke {{ \"if [ -x {ctl} ]; then {ctl} snapshot system --reason apt \
         --subvolume {sv} --snapshot-dir {sd} --prefix {p} --keep {k} || true; fi\"; }};\n",
        ctl = RYNASCTL,
        sv = s.subvolume,
        sd = s.snapshot_dir,
        p = s.prefix,
        k = s.keep,
    )
}

impl Applier for SnapshotsDomain {
    fn key(&self) -> &str {
        SNAPSHOTS_KEY
    }

    /// After `shares` (20): a changeset that adds a share and its job creates the share
    /// first. Before `scheduler` (40), whose units run the jobs.
    fn order(&self) -> i32 {
        30
    }

    fn validate(&self, value: Value) -> Result<Value, rynas_config::Invalid> {
        let mut c = parse(&value)?;
        let mut jobs = Vec::with_capacity(c.jobs.len());
        for j in c.jobs {
            let j = normalize_job(j)?;
            if jobs
                .iter()
                .any(|x: &SnapshotJob| x.name.eq_ignore_ascii_case(&j.name))
            {
                return Err(format!("job {} is defined twice", j.name).into());
            }
            if jobs.iter().any(|x: &SnapshotJob| {
                x.snapshot_dir == j.snapshot_dir || x.subvolume == j.subvolume
            }) {
                return Err(format!(
                    "job {}: another job already snapshots {} or uses {}",
                    j.name, j.subvolume, j.snapshot_dir
                )
                .into());
            }
            jobs.push(j);
        }
        c.jobs = jobs;
        let s = &mut c.system;
        if s.enabled {
            if !clean_abs(&s.subvolume) || !clean_abs(&s.snapshot_dir) || s.snapshot_dir == "/" {
                return Err(
                    "system restore points: invalid subvolume or snapshot directory".into(),
                );
            }
            if !valid_job_name(&s.prefix)
                || c.jobs
                    .iter()
                    .any(|j| j.name.eq_ignore_ascii_case(&s.prefix))
            {
                return Err(format!(
                    "system restore points: invalid prefix {:?} (or the name of a job)",
                    s.prefix
                )
                .into());
            }
            if !(1..=1000).contains(&s.keep) {
                return Err("system restore points: keep 1 to 1000".into());
            }
        }
        serde_json::to_value(c).map_err(|e| e.to_string().into())
    }

    fn render(&self, value: &Value, ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        let c = parse(value)?;
        let mut files = Vec::new();
        for j in &c.jobs {
            files.push(ManagedFile::whole(
                conf_path(&j.name),
                btrbk::render_conf(j)?,
                true,
            ));
        }
        // configurations of removed jobs (only files rynas generated)
        if let Ok(rd) = std::fs::read_dir(ctx.path(Path::new(btrbk::CONF_DIR))) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let Some(job) = name.strip_suffix(".conf") else {
                    continue;
                };
                let ours =
                    std::fs::read_to_string(e.path()).is_ok_and(|t| t.starts_with(HEADER_PREFIX));
                if ours && !c.jobs.iter().any(|j| j.name == job) {
                    files.push(ManagedFile::absent(conf_path(job)));
                }
            }
        }
        files.extend(crate::replication::managed_files(&c));
        files.push(if c.system.enabled {
            // apt.conf has no `#` comments: no header, the hash alone detects drift
            ManagedFile::whole(APT_HOOK, apt_hook(&c), false)
        } else {
            ManagedFile::absent(APT_HOOK)
        });
        Ok(files)
    }

    fn check_files(&self, files: &[Candidate], ctx: &ApplyCtx) -> Result<(), String> {
        for c in files {
            if c.file.path.starts_with(btrbk::CONF_DIR) {
                ctx.system
                    .run(
                        &Cmd::new("btrbk")
                            .arg("-c")
                            .arg(&c.staged)
                            .args(["config", "print"])
                            .env_clear(),
                    )
                    .map_err(|e| format!("btrbk refuses {}: {e}", c.file.path.display()))?;
            }
        }
        Ok(())
    }

    fn reconcile(
        &self,
        _from: Option<&Value>,
        to: Option<&Value>,
        ctx: &ApplyCtx,
    ) -> Result<(), String> {
        let Some(v) = to else { return Ok(()) };
        let c = parse(v)?;
        for j in &c.jobs {
            if self.host.is_subvolume(&j.subvolume) {
                self.host.ensure_dir(&j.snapshot_dir)?;
                (ctx.log)(&format!("snapshot job {}: {}", j.name, j.snapshot_dir));
            }
        }
        if c.system.enabled && self.host.is_subvolume(&c.system.subvolume) {
            self.host.ensure_dir(&c.system.snapshot_dir)?;
        }
        Ok(())
    }

    fn health(&self, value: Option<&Value>, _ctx: &ApplyCtx) -> Result<(), String> {
        let Some(v) = value else { return Ok(()) };
        let c = parse(v)?;
        for j in &c.jobs {
            if !self.host.is_subvolume(&j.subvolume) {
                return Err(format!(
                    "job {}: {} is not a btrfs subvolume",
                    j.name, j.subvolume
                ));
            }
            if !self.host.same_filesystem(&j.subvolume, &j.snapshot_dir) {
                return Err(format!(
                    "job {}: {} is not on the filesystem of {}",
                    j.name, j.snapshot_dir, j.subvolume
                ));
            }
        }
        if c.system.enabled && !self.host.is_subvolume(&c.system.subvolume) {
            return Err(format!(
                "system restore points: {} is not a btrfs subvolume",
                c.system.subvolume
            ));
        }
        Ok(())
    }
}
