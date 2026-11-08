//! `rynas-backup-worker`: one repository operation per process (D-BACKUP).
//!
//! rynasd starts it (in a transient systemd unit with `MemoryMax`, `Nice=19`,
//! `IOSchedulingClass=idle` and `KillSignal=SIGINT`, or as a plain child in tests and dry
//! runs), writes one [`WorkerRequest`] as JSON on its standard input and closes it, then reads
//! [`WorkerEvent`] lines on its standard output until the process ends. The last line is the
//! result or an [`WorkerEvent::Error`].
//!
//! - The engine is `rustic_core` (restic repository format v2, default chunker and pack
//!   sizes: the repository opens with restic too). Backends from `rustic_backend`: local
//!   folders, REST (`rest-server`), OpenDAL (S3, B2, SFTP) and rclone (`rclone serve restic`).
//! - Secrets arrive on standard input only (never the command line or the environment of
//!   rynasd); files that a backend needs on disk (an SFTP private key, an rclone
//!   configuration) are written 0600 in a private folder removed at exit. Every message the
//!   worker prints has the secret values replaced by `***`.
//! - Cancel: SIGINT or SIGTERM end the process. A backup interrupted this way is resumed by
//!   the next run: rustic writes the index of the packs already uploaded every 5 minutes, so
//!   that data is not sent again.
//! - The process lowers its own CPU and I/O priority when it was started without a unit.

use std::collections::BTreeMap;
use std::fmt::Display;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use rustic_backend::BackendOptions;
use rustic_core::{
    BackupOptions, CheckOptions, ConfigOptions, Credentials, Excludes, KeepOptions, KeyOptions,
    LocalDestination, LocalSourceFilterOptions, LsOptions, OpenStatus, PathList, Progress,
    ProgressBars, ProgressType, PruneOptions, ReadSubsetOption, Repository, RepositoryOptions,
    RestoreOptions, RusticProgress, SnapshotOptions,
};
use rynas_api_types::backup::{
    NodeEntry, RepoSpec, Retention, SnapshotEntry, TestOutcome, WorkerEvent, WorkerOp,
    WorkerRequest, WorkerSource,
};

type Res<T> = Result<T, String>;

fn err(e: impl Display) -> String {
    e.to_string()
}

// ---- output ----

/// Secret values hidden from every line printed.
static SECRETS: OnceLock<Vec<String>> = OnceLock::new();
static OUT: Mutex<()> = Mutex::new(());

fn redact(text: &str) -> String {
    let mut s = text.to_string();
    for v in SECRETS.get().into_iter().flatten() {
        if v.len() >= 4 {
            s = s.replace(v.as_str(), "***");
        }
    }
    s
}

fn emit(ev: &WorkerEvent) {
    let line = serde_json::to_string(ev).unwrap_or_default();
    let line = redact(&line);
    let _g = OUT.lock();
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn log(text: impl Into<String>) {
    emit(&WorkerEvent::Log { text: text.into() });
}

/// rustic's progress, as `Step` and throttled `Progress` events.
#[derive(Debug)]
struct Bars;

#[derive(Debug)]
struct Bar {
    bytes: bool,
    total: AtomicU64,
    done: AtomicU64,
    last: Mutex<Instant>,
}

impl Bar {
    fn send(&self, force: bool) {
        let mut last = match self.last.lock() {
            Ok(l) => l,
            Err(p) => p.into_inner(),
        };
        if force || last.elapsed() >= Duration::from_millis(500) {
            *last = Instant::now();
            emit(&WorkerEvent::Progress {
                done: self.done.load(Ordering::Relaxed),
                total: self.total.load(Ordering::Relaxed),
                bytes: self.bytes,
            });
        }
    }
}

impl RusticProgress for Bar {
    fn is_hidden(&self) -> bool {
        false
    }
    fn set_length(&self, len: u64) {
        self.total.store(len, Ordering::Relaxed);
    }
    fn set_title(&self, title: &str) {
        if !title.is_empty() {
            emit(&WorkerEvent::Step {
                title: title.trim_end_matches('.').to_string(),
            });
        }
    }
    fn inc(&self, inc: u64) {
        self.done.fetch_add(inc, Ordering::Relaxed);
        self.send(false);
    }
    fn finish(&self) {
        if self.done.load(Ordering::Relaxed) > 0 {
            self.send(true);
        }
    }
}

impl ProgressBars for Bars {
    fn progress(&self, t: ProgressType, prefix: &str) -> Progress {
        let title = prefix.trim().trim_end_matches('.');
        if !title.is_empty() {
            emit(&WorkerEvent::Step {
                title: title.to_string(),
            });
        }
        Progress::new(Bar {
            bytes: matches!(t, ProgressType::Bytes),
            total: AtomicU64::new(0),
            done: AtomicU64::new(0),
            last: Mutex::new(Instant::now()),
        })
    }
}

// ---- repository ----

/// The private folder of the files a backend reads (removed on drop).
struct Private(PathBuf);

impl Private {
    fn new() -> Res<Self> {
        let base = std::env::var_os("RUNTIME_DIRECTORY")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("rynas-backup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(err)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(err)?;
        Ok(Private(dir))
    }
}

impl Drop for Private {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_private(dir: &Path, name: &str, content: &str) -> Res<PathBuf> {
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let p = dir.join(safe);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&p)
        .map_err(err)?;
    f.write_all(content.as_bytes()).map_err(err)?;
    Ok(p)
}

/// The backend options with the private files in place; environment names (upper case) are
/// set for the worker's children (rclone).
fn backend_options(spec: &RepoSpec, private: &Path) -> Res<(BackendOptions, Vec<(String, String)>)> {
    let mut options = spec.options.clone();
    let mut env: Vec<(String, String)> = spec.env.clone().into_iter().collect();
    for (name, content) in &spec.files {
        let p = write_private(private, name, content)?;
        let p = p.display().to_string();
        if name.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
            env.push((name.clone(), p));
        } else {
            options.insert(name.clone(), p);
        }
    }
    let be = BackendOptions::default()
        .repository(spec.repository.clone())
        .options(options);
    Ok((be, env))
}

fn repository(spec: &RepoSpec, private: &Path) -> Res<Repository<()>> {
    let (be, env) = backend_options(spec, private)?;
    for (k, v) in env {
        // SAFETY: set before any thread of this process starts (backends start theirs when
        // they are created, right below)
        unsafe { std::env::set_var(k, v) };
    }
    let backends = be.to_backends().map_err(err)?;
    // no local cache: the worker is short-lived and runs as root in a unit of its own
    let opts = RepositoryOptions::default().no_cache(true);
    Repository::new_with_progress(&opts, &backends, Bars).map_err(err)
}

fn credentials(spec: &RepoSpec) -> Credentials {
    Credentials::Password(spec.password.clone())
}

fn open(spec: &RepoSpec, private: &Path) -> Res<Repository<OpenStatus>> {
    repository(spec, private)?
        .open(&credentials(spec))
        .map_err(err)
}

fn ms(z: &rustic_core::jiff::Zoned) -> u64 {
    z.timestamp().as_millisecond().max(0) as u64
}

fn entry(s: &rustic_core::repofile::SnapshotFile) -> SnapshotEntry {
    SnapshotEntry {
        id: s.id.to_string(),
        time_ms: ms(&s.time),
        hostname: s.hostname.clone(),
        paths: s.paths.iter().cloned().collect(),
        tags: s.tags.iter().cloned().collect(),
        files: s.summary.as_ref().map(|x| x.total_files_processed),
        bytes: s.summary.as_ref().map(|x| x.total_bytes_processed),
        added_bytes: s.summary.as_ref().map(|x| x.data_added),
    }
}

fn keep_options(r: &Retention) -> KeepOptions {
    let n = |v: u32| (v > 0).then_some(v as i32);
    KeepOptions::default()
        .keep_last(n(r.last))
        .keep_hourly(n(r.hourly))
        .keep_daily(n(r.daily))
        .keep_weekly(n(r.weekly))
        .keep_monthly(n(r.monthly))
        .keep_yearly(n(r.yearly))
}

/// Built-in excludes: recycle bins, snapshot folders, indexing folders, rynas' own
/// temporary snapshots.
const BUILTIN_EXCLUDES: &[&str] = &["#recycle", ".snapshots", "@eaDir", ".rynas-backup"];

fn backup_options(src: &WorkerSource, excludes: &[String]) -> BackupOptions {
    let globs: Vec<String> = BUILTIN_EXCLUDES
        .iter()
        .map(|s| s.to_string())
        .chain(excludes.iter().cloned())
        .map(|g| format!("!{g}"))
        .collect();
    BackupOptions::default()
        .as_path(Some(PathBuf::from(&src.as_path)))
        .excludes(Excludes::default().globs(globs))
        .ignore_filter_opts(
            LocalSourceFilterOptions::default()
                .exclude_if_present(vec!["CACHEDIR.TAG".to_string(), ".nobackup".to_string()]),
        )
}

fn run(req: WorkerRequest) -> Res<WorkerEvent> {
    let private = Private::new()?;
    let spec = &req.repo;
    match req.op {
        WorkerOp::Test => {
            let repo = repository(spec, &private.0)?;
            match repo.config_id().map_err(err)? {
                None => Ok(WorkerEvent::Test {
                    outcome: TestOutcome::Empty,
                    snapshots: None,
                }),
                Some(_) => match repo.open(&credentials(spec)) {
                    Ok(r) => {
                        let n = r.get_all_snapshots().map_err(err)?.len() as u32;
                        Ok(WorkerEvent::Test {
                            outcome: TestOutcome::Opens,
                            snapshots: Some(n),
                        })
                    }
                    Err(e) => {
                        log(format!("open: {e}"));
                        Ok(WorkerEvent::Test {
                            outcome: TestOutcome::Locked,
                            snapshots: None,
                        })
                    }
                },
            }
        }
        WorkerOp::Init => {
            let repo = repository(spec, &private.0)?;
            if repo.config_id().map_err(err)?.is_some() {
                // must open with this password
                repo.open(&credentials(spec)).map_err(err)?;
                return Ok(WorkerEvent::Initialized { created: false });
            }
            // the restic-compatible profile: repository v2, default chunker and packs
            repo.init(
                &credentials(spec),
                &KeyOptions::default(),
                &ConfigOptions::default(),
            )
            .map_err(err)?;
            Ok(WorkerEvent::Initialized { created: true })
        }
        WorkerOp::Backup {
            sources,
            tags,
            host,
            excludes,
        } => {
            let repo = open(spec, &private.0)?.to_indexed_ids().map_err(err)?;
            let mut last = None;
            for src in &sources {
                emit(&WorkerEvent::Step {
                    title: format!("backing up {}", src.as_path),
                });
                let mut so = SnapshotOptions::default().host(Some(host.clone()));
                so = so.add_tags(&tags.join(",")).map_err(err)?;
                let snap = so.to_snapshot().map_err(err)?;
                let source = PathList::from_iter([PathBuf::from(&src.read)])
                    .sanitize()
                    .map_err(err)?;
                let snap = repo
                    .backup(&backup_options(src, &excludes), &source, snap)
                    .map_err(err)?;
                let e = entry(&snap);
                emit(&WorkerEvent::BackedUp {
                    snapshot: e.clone(),
                });
                last = Some(e);
            }
            Ok(WorkerEvent::BackedUp {
                snapshot: last.ok_or("no source")?,
            })
        }
        WorkerOp::Forget {
            tag,
            retention,
            dry_run,
        } => {
            if retention.is_empty() {
                return Err("no retention rule".into());
            }
            let repo = open(spec, &private.0)?;
            let all = repo.get_all_snapshots().map_err(err)?;
            // one group per path (each source of a job is its own snapshot series)
            let mut groups: BTreeMap<String, Vec<_>> = BTreeMap::new();
            for s in all.into_iter().filter(|s| s.tags.contains(&tag)) {
                groups.entry(s.paths.to_string()).or_default().push(s);
            }
            let keep_opts = keep_options(&retention);
            let now = rustic_core::jiff::Zoned::now();
            let (mut keep, mut remove, mut ids) = (Vec::new(), Vec::new(), Vec::new());
            for (_, snaps) in groups {
                for f in keep_opts.apply(snaps, &now).map_err(err)? {
                    if f.keep {
                        keep.push(entry(&f.snapshot));
                    } else {
                        ids.push(f.snapshot.id);
                        remove.push(entry(&f.snapshot));
                    }
                }
            }
            if !dry_run && !ids.is_empty() {
                repo.delete_snapshots(&ids).map_err(err)?;
            }
            Ok(WorkerEvent::Forgot { keep, remove })
        }
        WorkerOp::Prune => {
            let repo = open(spec, &private.0)?;
            // never instant delete: packs are removed after 23 h (rustic #1946)
            let opts = PruneOptions::default();
            let plan = repo.prune_plan(&opts).map_err(err)?;
            let removed_packs = plan.stats.packs_to_delete.remove;
            let freed_bytes = plan.stats.size_to_delete.remove;
            repo.prune(&opts, plan).map_err(err)?;
            Ok(WorkerEvent::Pruned {
                removed_packs,
                freed_bytes,
            })
        }
        WorkerOp::Check {
            read_percent,
            slice,
        } => {
            let repo = open(spec, &private.0)?;
            let mut opts = CheckOptions::default();
            if let Some((n, m)) = slice.filter(|(n, m)| *m > 0 && *n >= 1 && n <= m) {
                opts = opts
                    .read_data(true)
                    .read_data_subset(ReadSubsetOption::IdSubSet((n, m)));
            } else if read_percent > 0 {
                let subset = if read_percent >= 100 {
                    ReadSubsetOption::All
                } else {
                    ReadSubsetOption::from_str(&format!("{read_percent}%")).map_err(err)?
                };
                opts = opts.read_data(true).read_data_subset(subset);
            }
            let res = repo.check(opts).map_err(err)?;
            let (mut errors, mut warnings) = (Vec::new(), Vec::new());
            for (level, e) in &res.0 {
                if format!("{level:?}") == "Error" {
                    errors.push(e.to_string());
                } else {
                    warnings.push(e.to_string());
                }
            }
            Ok(WorkerEvent::Checked { errors, warnings })
        }
        WorkerOp::Snapshots { tag } => {
            let repo = open(spec, &private.0)?;
            let mut list: Vec<SnapshotEntry> = repo
                .get_all_snapshots()
                .map_err(err)?
                .iter()
                .filter(|s| tag.as_deref().is_none_or(|t| s.tags.contains(t)))
                .map(entry)
                .collect();
            list.sort_by_key(|s| std::cmp::Reverse(s.time_ms));
            Ok(WorkerEvent::Snapshots { list })
        }
        WorkerOp::Ls { snapshot, path } => {
            let repo = open(spec, &private.0)?.to_indexed_ids().map_err(err)?;
            let path = clean(&path);
            let node = repo
                .node_from_snapshot_path(&format!("{snapshot}:{path}"), |_| true)
                .map_err(err)?;
            if !node.is_dir() {
                return Err(format!("{path} is not a folder"));
            }
            let tree = node.subtree.ok_or("no folder contents")?;
            let tree = repo.get_tree(&tree).map_err(err)?;
            let base = path.trim_end_matches('/');
            let list = tree
                .nodes
                .iter()
                .map(|n| {
                    let name = n.name().to_string_lossy().into_owned();
                    NodeEntry {
                        path: format!("{base}/{name}"),
                        name,
                        dir: n.is_dir(),
                        size: n.meta.size,
                        mtime_ms: n
                            .meta
                            .mtime
                            .as_ref()
                            .map(|t| t.as_millisecond().max(0) as u64),
                    }
                })
                .collect();
            Ok(WorkerEvent::Entries { list })
        }
        WorkerOp::Restore {
            snapshot,
            paths,
            target,
        } => {
            let repo = open(spec, &private.0)?.to_indexed().map_err(err)?;
            let paths = if paths.is_empty() {
                let snap = repo
                    .get_snapshot_from_str(&snapshot, |_| true)
                    .map_err(err)?;
                snap.paths.iter().cloned().collect()
            } else {
                paths
            };
            let opts = RestoreOptions::default();
            let (mut files, mut bytes) = (0, 0);
            for p in &paths {
                let p = clean(p);
                let node = repo
                    .node_from_snapshot_path(&format!("{snapshot}:{p}"), |_| true)
                    .map_err(err)?;
                let dest = match &target {
                    None => p.clone(),
                    Some(t) => format!("{}/{}", t.trim_end_matches('/'), p.trim_start_matches('/')),
                };
                emit(&WorkerEvent::Step {
                    title: format!("restoring {p} to {dest}"),
                });
                let ls = repo.ls(&node, &LsOptions::default()).map_err(err)?;
                let dest = LocalDestination::new(&dest, true, !node.is_dir()).map_err(err)?;
                let plan = repo
                    .prepare_restore(&opts, ls.clone(), &dest, false)
                    .map_err(err)?;
                files += plan.stats.files.restore + plan.stats.files.modify;
                bytes += plan.restore_size;
                repo.restore(plan, &opts, ls, &dest).map_err(err)?;
            }
            Ok(WorkerEvent::Restored { files, bytes })
        }
        WorkerOp::Dump {
            snapshot,
            path,
            out,
        } => {
            let repo = open(spec, &private.0)?.to_indexed().map_err(err)?;
            let path = clean(&path);
            let node = repo
                .node_from_snapshot_path(&format!("{snapshot}:{path}"), |_| true)
                .map_err(err)?;
            if !node.is_file() {
                return Err(format!("{path} is not a file"));
            }
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&out)
                .map_err(err)?;
            repo.dump(&node, &mut f).map_err(err)?;
            f.flush().map_err(err)?;
            Ok(WorkerEvent::Dumped {
                bytes: node.meta.size,
            })
        }
        WorkerOp::Stats => {
            let repo = open(spec, &private.0)?;
            let infos = repo.infos_files().map_err(err)?;
            let (bytes, files) = infos
                .repo
                .iter()
                .fold((0, 0), |(b, f), i| (b + i.size, f + i.count));
            Ok(WorkerEvent::Stats { bytes, files })
        }
    }
}

/// `/a/b/` → `/a/b`; always absolute.
fn clean(p: &str) -> String {
    let t = p.trim().trim_end_matches('/');
    if t.is_empty() {
        "/".into()
    } else if t.starts_with('/') {
        t.into()
    } else {
        format!("/{t}")
    }
}

/// Nice 19 and the idle I/O class, when no unit has set them already.
fn lower_priority() {
    // SAFETY: plain system calls on this process
    unsafe {
        if libc::getpriority(libc::PRIO_PROCESS, 0) < 19 {
            libc::setpriority(libc::PRIO_PROCESS, 0, 19);
        }
        // ioprio_set(IOPRIO_WHO_PROCESS, 0, IOPRIO_CLASS_IDLE << 13)
        libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13);
    }
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("rynas-backup-worker {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    lower_priority();
    let mut input = String::new();
    if let Err(e) = std::io::stdin().take(4 << 20).read_to_string(&mut input) {
        emit(&WorkerEvent::Error {
            message: format!("reading the request: {e}"),
        });
        std::process::exit(2);
    }
    let req: WorkerRequest = match serde_json::from_str(&input) {
        Ok(r) => r,
        Err(e) => {
            emit(&WorkerEvent::Error {
                message: format!("invalid request: {e}"),
            });
            std::process::exit(2);
        }
    };
    let mut secrets = vec![req.repo.password.clone()];
    secrets.extend(req.repo.files.values().cloned());
    secrets.extend(req.repo.env.values().cloned());
    secrets.extend(
        req.repo
            .options
            .iter()
            .filter(|(k, _)| is_secret_option(k))
            .map(|(_, v)| v.clone()),
    );
    // the password inside a REST URL
    if let Some(pw) = url_password(&req.repo.repository) {
        secrets.push(pw);
    }
    let _ = SECRETS.set(secrets);
    match run(req) {
        Ok(ev) => emit(&ev),
        Err(e) => {
            emit(&WorkerEvent::Error { message: e });
            std::process::exit(1);
        }
    }
}

fn is_secret_option(k: &str) -> bool {
    matches!(
        k,
        "secret_access_key" | "application_key" | "password" | "session_token"
    )
}

/// `rest:https://user:pw@host/…` → `pw`.
fn url_password(repo: &str) -> Option<String> {
    let rest = repo.split_once("://")?.1;
    let auth = rest.split_once('@')?.0;
    let pw = auth.split_once(':')?.1;
    (!pw.is_empty()).then(|| pw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_helpers() {
        assert_eq!(clean("a/b/"), "/a/b");
        assert_eq!(clean(""), "/");
        assert_eq!(
            url_password("rest:https://u:s3cret@nas:8000/r").as_deref(),
            Some("s3cret")
        );
        assert_eq!(url_password("rest:https://nas:8000/r"), None);
        let _ = SECRETS.set(vec!["hunter22".into()]);
        assert_eq!(redact("x hunter22 y"), "x *** y");
    }
}
