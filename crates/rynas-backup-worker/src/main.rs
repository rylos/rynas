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
//!   configuration, the pinned SSH host key) are written 0600 in a fresh private folder
//!   removed at exit, also by the SIGINT/SIGTERM handler. Every message the worker prints
//!   has the secret values replaced by `***` (before serialization, then on the line). A
//!   secret file the backend rewrote (rclone's refreshed OAuth token, a host key learned on
//!   first use) is reported once, as [`WorkerEvent::SecretChanged`], for rynasd to seal.
//! - SFTP: OpenDAL runs `ssh` from `PATH`; the worker puts a wrapper first that runs
//!   `/usr/bin/ssh -F none` with the job's own `known_hosts` and key login only.
//! - The job's persistent cache (`cache_dir`) keeps index and tree packs between runs.
//! - Snapshots are scoped ([`SnapshotScope`]): `latest` resolves within the job's tag and
//!   this NAS' host, an explicit id must carry the tag, retention groups by host and path.
//! - Cancel: SIGINT or SIGTERM end the process. A backup interrupted this way is resumed by
//!   the next run: rustic writes the index of the packs already uploaded every 5 minutes, so
//!   that data is not sent again.
//! - The process lowers its own CPU and I/O priority when it was started without a unit.

use std::collections::BTreeMap;
use std::fmt::Display;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use rustic_backend::BackendOptions;
use rustic_core::{
    BackupOptions, CheckOptions, ConfigOptions, Credentials, Excludes, KeepOptions, KeyOptions,
    LocalDestination, LocalSourceFilterOptions, LsOptions, OpenStatus, PathList, Progress,
    ProgressBars, ProgressType, PruneOptions, ReadSubsetOption, Repository, RepositoryOptions,
    RestoreOptions, RusticProgress, SnapshotOptions,
};
use rustic_core::repofile::SnapshotFile;
use rynas_api_types::backup::{
    NodeEntry, RepoSpec, Retention, SnapshotEntry, SnapshotScope, TestOutcome, WorkerEvent,
    WorkerOp, WorkerRequest, WorkerSource, redact_secrets,
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
    redact_secrets(text, SECRETS.get().map(Vec::as_slice).unwrap_or_default())
}

/// The event with its free texts redacted before serialization (a secret with `"` or `\`
/// would not match once escaped in JSON); the line is redacted again after.
fn scrub(ev: &WorkerEvent) -> WorkerEvent {
    match ev {
        WorkerEvent::Step { title } => WorkerEvent::Step { title: redact(title) },
        WorkerEvent::Log { text } => WorkerEvent::Log { text: redact(text) },
        WorkerEvent::Error { message } => WorkerEvent::Error {
            message: redact(message),
        },
        WorkerEvent::Checked { errors, warnings } => WorkerEvent::Checked {
            errors: errors.iter().map(|e| redact(e)).collect(),
            warnings: warnings.iter().map(|e| redact(e)).collect(),
        },
        other => other.clone(),
    }
}

fn emit(ev: &WorkerEvent) {
    let line = serde_json::to_string(&scrub(ev)).unwrap_or_default();
    write_line(&redact(&line));
}

/// A changed secret file for rynasd: the only line that carries a secret, never redacted.
fn emit_secret(name: &str, content: &str) {
    let ev = WorkerEvent::SecretChanged {
        name: name.to_string(),
        content: content.to_string(),
    };
    write_line(&serde_json::to_string(&ev).unwrap_or_default());
}

fn write_line(line: &str) {
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

/// The private folder of the files a backend reads (removed on drop, and by the SIGINT
/// and SIGTERM handler).
struct Private {
    dir: PathBuf,
    /// Secret files a backend may rewrite (an rclone configuration with a refreshed
    /// token, `known_hosts` filled on first use): name → (path, content written).
    watched: Mutex<Vec<(String, PathBuf, String)>>,
}

/// Paths the signal handler removes: up to [`CLEAN_SLOTS`] files, then the folder.
const CLEAN_SLOTS: usize = 8;
static CLEAN: [AtomicPtr<libc::c_char>; CLEAN_SLOTS + 1] =
    [const { AtomicPtr::new(std::ptr::null_mut()) }; CLEAN_SLOTS + 1];

/// Remembers `p` for the signal handler (slot `CLEAN_SLOTS` is the folder itself).
fn clean_on_signal(slot: Option<usize>, p: &Path) {
    let Ok(c) = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()) else {
        return;
    };
    let slot = match slot {
        Some(s) => s,
        None => match CLEAN[..CLEAN_SLOTS]
            .iter()
            .position(|a| a.load(Ordering::SeqCst).is_null())
        {
            Some(s) => s,
            None => return,
        },
    };
    // leaked on purpose: the handler may read it until the process ends
    CLEAN[slot].store(c.into_raw(), Ordering::SeqCst);
}

extern "C" fn on_signal(sig: libc::c_int) {
    // only async-signal-safe calls: unlink, rmdir, _exit
    for (i, a) in CLEAN.iter().enumerate() {
        let p = a.load(Ordering::SeqCst);
        if !p.is_null() {
            // SAFETY: a C string leaked by clean_on_signal, never freed
            unsafe {
                if i == CLEAN_SLOTS {
                    libc::rmdir(p);
                } else {
                    libc::unlink(p);
                }
            }
        }
    }
    // SAFETY: ends the process without running anything else
    unsafe { libc::_exit(128 + sig) };
}

fn install_signal_handlers() {
    for sig in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: a handler that only calls async-signal-safe functions
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as *const () as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

impl Private {
    fn new() -> Res<Self> {
        let base = std::env::var_os("RUNTIME_DIRECTORY")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("rynas-backup-{}", std::process::id()));
        // a fresh folder, never one that is already there (a shared /tmp in direct runs)
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        clean_on_signal(Some(CLEAN_SLOTS), &dir);
        Ok(Private {
            dir,
            watched: Mutex::new(Vec::new()),
        })
    }

    /// The watched files whose content changed: (name, new content).
    fn changed(&self) -> Vec<(String, String)> {
        let w = self.watched.lock().unwrap_or_else(|p| p.into_inner());
        w.iter()
            .filter_map(|(name, p, old)| {
                let now = std::fs::read_to_string(p).ok()?;
                (now != *old && !now.trim().is_empty()).then(|| (name.clone(), now))
            })
            .collect()
    }

    fn read(&self, name: &str) -> Option<String> {
        let w = self.watched.lock().unwrap_or_else(|p| p.into_inner());
        let (_, p, _) = w.iter().find(|(n, _, _)| n == name)?;
        std::fs::read_to_string(p).ok()
    }
}

impl Drop for Private {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn write_private(dir: &Path, name: &str, content: &str, mode: u32) -> Res<PathBuf> {
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let p = dir.join(safe);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&p)
        .map_err(err)?;
    clean_on_signal(None, &p);
    f.write_all(content.as_bytes()).map_err(err)?;
    Ok(p)
}

/// `ssh` for OpenDAL's SFTP (the `openssh` crate runs `ssh` from `PATH`): no system or user
/// configuration (`-F none`), only the job's pinned host key file, key login only.
fn ssh_wrapper(dir: &Path, known_hosts: &Path) -> Res<()> {
    let kh = known_hosts.display().to_string();
    if kh.contains(['\'', '\n']) {
        return Err("unsupported private folder name".into());
    }
    let script = format!(
        "#!/bin/sh\nexec /usr/bin/ssh -F none -o 'UserKnownHostsFile={kh}' -o GlobalKnownHostsFile=/dev/null \
-o HashKnownHosts=no -o UpdateHostKeys=no -o IdentitiesOnly=yes -o PasswordAuthentication=no \
-o KbdInteractiveAuthentication=no -o ForwardAgent=no -o ClearAllForwardings=yes \"$@\"\n"
    );
    write_private(dir, "ssh", &script, 0o700)?;
    Ok(())
}

/// The backend options with the private files in place; environment names (upper case) are
/// set for the worker's children (rclone). `known_hosts` goes to the `ssh` wrapper.
fn backend_options(spec: &RepoSpec, private: &Private) -> Res<(BackendOptions, Vec<(String, String)>)> {
    let mut options = spec.options.clone();
    let mut env: Vec<(String, String)> = spec.env.clone().into_iter().collect();
    for (name, content) in &spec.files {
        let p = write_private(&private.dir, name, content, 0o600)?;
        if matches!(name.as_str(), "RCLONE_CONFIG" | "known_hosts") {
            private
                .watched
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push((name.clone(), p.clone(), content.clone()));
        }
        if name == "known_hosts" {
            ssh_wrapper(&private.dir, &p)?;
            let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
            env.push(("PATH".into(), format!("{}:{path}", private.dir.display())));
            continue;
        }
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

fn repository(spec: &RepoSpec, private: &Private, cache: Option<&str>) -> Res<Repository<()>> {
    let (be, env) = backend_options(spec, private)?;
    for (k, v) in env {
        // SAFETY: set before any thread of this process starts (backends start theirs when
        // they are created, right below)
        unsafe { std::env::set_var(k, v) };
    }
    let backends = be.to_backends().map_err(err)?;
    // the job's persistent cache (index and tree packs: a run downloads only what is
    // new); none without one
    let opts = match cache {
        Some(c) => RepositoryOptions::default().cache_dir(PathBuf::from(c)),
        None => RepositoryOptions::default().no_cache(true),
    };
    Repository::new_with_progress(&opts, &backends, Bars).map_err(err)
}

fn credentials(spec: &RepoSpec) -> Credentials {
    Credentials::Password(spec.password.clone())
}

fn open(cx: &Cx) -> Res<Repository<OpenStatus>> {
    repository(cx.spec, cx.private, cx.cache)?
        .open(&credentials(cx.spec))
        .map_err(err)
}

/// What every operation needs.
struct Cx<'a> {
    spec: &'a RepoSpec,
    private: &'a Private,
    cache: Option<&'a str>,
}

/// Whether a snapshot is the job's (tag) and, for `latest` and lists, this NAS' (host).
fn tag_ok(s: &SnapshotFile, scope: Option<&SnapshotScope>) -> bool {
    scope.is_none_or(|sc| s.tags.contains(&sc.tag))
}

fn host_ok(s: &SnapshotFile, scope: Option<&SnapshotScope>) -> bool {
    scope.is_none_or(|sc| sc.host.is_empty() || s.hostname == sc.host)
}

/// `latest` resolved within the scope; an explicit id must carry the job's tag.
fn pick<S: rustic_core::Open>(
    repo: &Repository<S>,
    snapshot: &str,
    scope: Option<&SnapshotScope>,
) -> Res<SnapshotFile> {
    let snap = repo
        .get_snapshot_from_str(snapshot, |s| tag_ok(s, scope) && host_ok(s, scope))
        .map_err(err)?;
    if !tag_ok(&snap, scope) {
        return Err(format!("snapshot {snapshot} is not one of this job's"));
    }
    Ok(snap)
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
        .chain(src.excludes.iter().cloned())
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

fn run(req: WorkerRequest, private: &Private) -> Res<WorkerEvent> {
    let cx = Cx {
        spec: &req.repo,
        private,
        cache: req.cache_dir.as_deref(),
    };
    let spec = cx.spec;
    match req.op {
        WorkerOp::Test => {
            let repo = repository(spec, private, None)?;
            let outcome = match repo.config_id().map_err(err)? {
                None => (TestOutcome::Empty, None),
                Some(_) => match repo.open(&credentials(spec)) {
                    Ok(r) => {
                        let n = r.get_all_snapshots().map_err(err)?.len() as u32;
                        (TestOutcome::Opens, Some(n))
                    }
                    Err(e) => {
                        log(format!("open: {e}"));
                        (TestOutcome::Locked, None)
                    }
                },
            };
            let host_key = private
                .read("known_hosts")
                .filter(|k| !k.trim().is_empty());
            Ok(WorkerEvent::Test {
                outcome: outcome.0,
                snapshots: outcome.1,
                host_key,
            })
        }
        WorkerOp::Init => {
            let repo = repository(spec, private, cx.cache)?;
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
            let repo = open(&cx)?.to_indexed_ids().map_err(err)?;
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
            scope,
            retention,
            dry_run,
        } => {
            if retention.is_empty() {
                return Err("no retention rule".into());
            }
            if scope.tag.is_empty() {
                return Err("forget needs the job's tag".into());
            }
            let repo = open(&cx)?;
            let all = repo.get_all_snapshots().map_err(err)?;
            // one group per host and path (each source of a job is its own snapshot
            // series; another NAS writing the same job name into this repository is never
            // counted against, nor deleted by, this one's retention)
            let mut groups: BTreeMap<(String, String), Vec<_>> = BTreeMap::new();
            for s in all
                .into_iter()
                .filter(|s| tag_ok(s, Some(&scope)) && host_ok(s, Some(&scope)))
            {
                groups
                    .entry((s.hostname.clone(), s.paths.to_string()))
                    .or_default()
                    .push(s);
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
            let repo = open(&cx)?;
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
            let repo = open(&cx)?;
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
        WorkerOp::Snapshots { scope } => {
            let repo = open(&cx)?;
            let mut list: Vec<SnapshotEntry> = repo
                .get_all_snapshots()
                .map_err(err)?
                .iter()
                .filter(|s| tag_ok(s, scope.as_ref()) && host_ok(s, scope.as_ref()))
                .map(entry)
                .collect();
            list.sort_by_key(|s| std::cmp::Reverse(s.time_ms));
            Ok(WorkerEvent::Snapshots { list })
        }
        WorkerOp::Ls {
            snapshot,
            path,
            scope,
        } => {
            let repo = open(&cx)?.to_indexed_ids().map_err(err)?;
            let path = clean(&path);
            let snap = pick(&repo, &snapshot, scope.as_ref())?;
            let node = repo.node_from_snapshot_and_path(&snap, &path).map_err(err)?;
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
            scope,
        } => {
            let target = target.ok_or("a restore needs a target folder")?;
            let repo = open(&cx)?.to_indexed().map_err(err)?;
            let snap = pick(&repo, &snapshot, scope.as_ref())?;
            let paths = if paths.is_empty() {
                snap.paths.iter().cloned().collect()
            } else {
                paths
            };
            let opts = RestoreOptions::default();
            let (mut files, mut bytes) = (0, 0);
            for p in &paths {
                let p = clean(p);
                let node = repo.node_from_snapshot_and_path(&snap, &p).map_err(err)?;
                let dest = format!("{}/{}", target.trim_end_matches('/'), p.trim_start_matches('/'));
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
            max_bytes,
            scope,
        } => {
            let repo = open(&cx)?.to_indexed().map_err(err)?;
            let path = clean(&path);
            let snap = pick(&repo, &snapshot, scope.as_ref())?;
            let node = repo.node_from_snapshot_and_path(&snap, &path).map_err(err)?;
            if !node.is_file() {
                return Err(format!("{path} is not a file"));
            }
            if max_bytes > 0 && node.meta.size > max_bytes {
                return Err(format!(
                    "{TOO_LARGE} ({} bytes): restore it to a folder",
                    node.meta.size
                ));
            }
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&out)
                .map_err(err)?;
            repo.dump(&node, &mut f).map_err(err)?;
            f.flush().map_err(err)?;
            Ok(WorkerEvent::Dumped {
                bytes: node.meta.size,
            })
        }
        WorkerOp::Stats => {
            let repo = open(&cx)?;
            let infos = repo.infos_files().map_err(err)?;
            let (bytes, files) = infos
                .repo
                .iter()
                .fold((0, 0), |(b, f), i| (b + i.size, f + i.count));
            Ok(WorkerEvent::Stats { bytes, files })
        }
    }
}

/// The start of the error of a dump over its `max_bytes` (rynasd answers 409 on it).
const TOO_LARGE: &str = "the file is too large to download";

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
    let _ = SECRETS.set(req.repo.secret_values());
    install_signal_handlers();
    let private = match Private::new() {
        Ok(p) => p,
        Err(e) => {
            emit(&WorkerEvent::Error { message: e });
            std::process::exit(1);
        }
    };
    let res = run(req, &private);
    // a backend that rewrote a secret file (rclone's refreshed token, a host key learned
    // on first use): rynasd seals the new content, also when the operation failed
    for (name, content) in private.changed() {
        emit_secret(&name, &content);
    }
    drop(private);
    match res {
        Ok(ev) => emit(&ev),
        Err(e) => {
            emit(&WorkerEvent::Error { message: e });
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_helpers() {
        assert_eq!(clean("a/b/"), "/a/b");
        assert_eq!(clean(""), "/");
        let _ = SECRETS.set(vec!["hun\"ter".into(), "ab".into()]);
        assert_eq!(redact("x hun\"ter y ab"), "x *** y ***");
        // redacted before serialization: the escaped form never reaches the line
        let ev = scrub(&WorkerEvent::Error {
            message: "bad hun\"ter".into(),
        });
        assert_eq!(ev, WorkerEvent::Error { message: "bad ***".into() });
    }

    #[test]
    fn private_files_and_ssh_wrapper() {
        use std::os::unix::fs::PermissionsExt;
        let p = Private::new().unwrap();
        let spec = RepoSpec {
            repository: "opendal:sftp".into(),
            password: "pw".into(),
            files: [
                ("known_hosts".to_string(), String::new()),
                ("key".to_string(), "PRIVATE".to_string()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let (_, env) = backend_options(&spec, &p).unwrap();
        let path = env.iter().find(|(k, _)| k == "PATH").unwrap();
        assert!(path.1.starts_with(&p.dir.display().to_string()));
        let wrapper = std::fs::read_to_string(p.dir.join("ssh")).unwrap();
        assert!(wrapper.contains("-F none"));
        assert!(wrapper.contains(&format!("UserKnownHostsFile={}/known_hosts", p.dir.display())));
        let mode = std::fs::metadata(p.dir.join("key")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // a host key learned on first use is reported
        assert!(p.changed().is_empty());
        std::fs::write(p.dir.join("known_hosts"), "nas2 ssh-ed25519 AAAA\n").unwrap();
        assert_eq!(p.changed(), vec![("known_hosts".into(), "nas2 ssh-ed25519 AAAA\n".into())]);
        let dir = p.dir.clone();
        drop(p);
        assert!(!dir.exists());
    }
}
