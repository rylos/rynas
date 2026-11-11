//! One backup run: a consistent view of every filesystem, one version in the NAS' repository.
//!
//! 1. The manifest is written into the root filesystem (so the version carries it).
//! 2. Per filesystem: btrfs → read-only snapshots of the mounted and nested subvolumes; LVM →
//!    a snapshot mounted read-only; ext2/3/4, XFS → read hot (files changed meanwhile are
//!    listed) or, with the plan's `freeze`, frozen for the whole run with a watchdog that
//!    thaws at `freeze_max_secs`; the ESP (vfat) as files.
//! 3. In a private mount namespace (this process only; gone when it ends) a tree is put
//!    together under `/run/rynas-agent/stage` with every source bound at its own mount point,
//!    read-only, and backed up as `/` in one snapshot tagged `rynas-pc` and `pc-<device>`.
//! 4. Thaw, unmount, snapshots removed; the report goes to the NAS.
//!
//! Nothing is written to a frozen filesystem: no cache, no state, log lines kept in memory
//! until the thaw.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustic_core::{
    BackupOptions, Credentials, Excludes, LocalSourceFilterOptions, PathList, Progress, ProgressBars, ProgressType,
    Repository, RepositoryOptions, RusticProgress, SnapshotOptions,
};
use rynas_api_types::pcagent::*;

use crate::http::{Client, Endpoint};
use crate::mounts::{self, Mount};
use crate::state::{State, now_ms};
use crate::{btrfs, freeze, lvm, manifest, probe};

const STAGE: &str = "/run/rynas-agent/stage";
const CACHE: &str = "/var/cache/rynas-agent";

/// The agent's own excludes (anchored ones are paths of the PC).
pub const BUILTIN_EXCLUDES: &[&str] = &[
    "/proc/*",
    "/sys/*",
    "/dev/*",
    "/run/*",
    "/tmp/*",
    "/var/tmp/*",
    "/var/cache/*",
    "/var/lib/docker/*",
    "/var/lib/containers/*",
    "/var/lib/systemd/coredump/*",
    "/swapfile",
    "/swap/*",
    "/home/*/.cache/*",
    "/root/.cache/*",
    ".rynas-snap",
];

// ------------------------------------------------------------------ log kept while frozen

static QUIET: AtomicBool = AtomicBool::new(false);
static HELD: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// A line for the terminal or the journal; held in memory while a filesystem is frozen.
pub fn say(line: impl Into<String>) {
    let line = line.into();
    if QUIET.load(Ordering::SeqCst) {
        if let Ok(mut h) = HELD.lock() {
            h.push(line);
        }
    } else {
        eprintln!("{line}");
    }
}

fn speak_again() {
    QUIET.store(false, Ordering::SeqCst);
    if let Ok(mut h) = HELD.lock() {
        for l in h.drain(..) {
            eprintln!("{l}");
        }
    }
}

// ------------------------------------------------------------------ progress

#[derive(Default)]
pub struct Counters {
    pub bytes_done: AtomicU64,
    pub bytes_total: AtomicU64,
    pub files_done: AtomicU64,
    pub phase: Mutex<AgentPhase>,
}

#[derive(Clone)]
struct Bars(Arc<Counters>);

impl std::fmt::Debug for Bars {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Bars")
    }
}

#[derive(Debug)]
struct Bar {
    bytes: bool,
    counters: Arc<CountersRef>,
}

#[derive(Debug)]
struct CountersRef(*const Counters);
// SAFETY: points into an Arc<Counters> kept alive by `Bars` for the whole backup
unsafe impl Send for CountersRef {}
unsafe impl Sync for CountersRef {}

impl Bar {
    fn c(&self) -> &Counters {
        // SAFETY: see CountersRef
        unsafe { &*self.counters.0 }
    }
}

impl RusticProgress for Bar {
    fn is_hidden(&self) -> bool {
        false
    }
    fn set_length(&self, len: u64) {
        if self.bytes {
            self.c().bytes_total.store(len, Ordering::Relaxed);
        }
    }
    fn set_title(&self, _title: &str) {}
    fn inc(&self, inc: u64) {
        if self.bytes {
            self.c().bytes_done.fetch_add(inc, Ordering::Relaxed);
        } else {
            self.c().files_done.fetch_add(inc, Ordering::Relaxed);
        }
    }
    fn finish(&self) {}
}

impl ProgressBars for Bars {
    fn progress(&self, t: ProgressType, _prefix: &str) -> Progress {
        Progress::new(Bar {
            bytes: matches!(t, ProgressType::Bytes),
            counters: Arc::new(CountersRef(Arc::as_ptr(&self.0))),
        })
    }
}

// ------------------------------------------------------------------ mount namespace

fn cstr(p: &Path) -> CString {
    CString::new(p.as_os_str().as_encoded_bytes()).unwrap_or_default()
}

/// This process gets mounts of its own (call before any thread starts).
pub fn private_mounts() -> Result<(), String> {
    // SAFETY: plain syscalls; nothing else runs yet
    unsafe {
        if libc::unshare(libc::CLONE_NEWNS) != 0 {
            return Err(format!("unshare: {}", std::io::Error::last_os_error()));
        }
        let root = cstr(Path::new("/"));
        if libc::mount(std::ptr::null(), root.as_ptr(), std::ptr::null(), libc::MS_REC | libc::MS_PRIVATE, std::ptr::null()) != 0 {
            return Err(format!("mount --make-rprivate /: {}", std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

fn mount_raw(src: Option<&Path>, dst: &Path, fstype: Option<&str>, flags: libc::c_ulong, data: Option<&str>) -> Result<(), String> {
    let s = src.map(cstr);
    let d = cstr(dst);
    let t = fstype.map(|t| CString::new(t).unwrap_or_default());
    let o = data.map(|o| CString::new(o).unwrap_or_default());
    // SAFETY: valid C strings or null
    let r = unsafe {
        libc::mount(
            s.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            d.as_ptr(),
            t.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            flags,
            o.as_ref().map_or(std::ptr::null(), |c| c.as_ptr().cast()),
        )
    };
    if r != 0 {
        return Err(format!("mount {} on {}: {}", src.map(|p| p.display().to_string()).unwrap_or_default(), dst.display(), std::io::Error::last_os_error()));
    }
    Ok(())
}

/// `src` bound read-only at `dst` (not recursive: only that filesystem).
fn bind_ro(src: &Path, dst: &Path) -> Result<(), String> {
    mount_raw(Some(src), dst, None, libc::MS_BIND, None)?;
    mount_raw(None, dst, None, libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY, None)
}

fn umount_lazy(p: &Path) {
    let c = cstr(p);
    // SAFETY: valid C string
    unsafe {
        libc::umount2(c.as_ptr(), libc::MNT_DETACH);
    }
}

fn stage_path(path: &str) -> PathBuf {
    if path == "/" { PathBuf::from(STAGE) } else { Path::new(STAGE).join(path.trim_start_matches('/')) }
}

// ------------------------------------------------------------------ the run

/// How one mount point is read in this run, and what to release afterwards.
struct Source {
    mount: Mount,
    method: CaptureMethod,
    /// What is bound at the mount point (a snapshot, or the live path).
    read: PathBuf,
    /// Nested btrfs subvolume snapshots: (path below the mount, snapshot).
    nested: Vec<(String, PathBuf)>,
    snaps: Vec<PathBuf>,
    lvm: Option<lvm::Snap>,
    note: Option<String>,
}

fn lvm_of(m: &Mount) -> Option<(String, String)> {
    let b = probe::block_of_devnum(&m.dev)?;
    if !b.dm_uuid.as_deref().is_some_and(|u| u.starts_with("LVM-")) {
        return None;
    }
    let (vg, lv) = manifest::split_lvm_name(&b.dm_name?);
    (!lv.is_empty()).then_some((vg, lv))
}

/// Leftovers of an interrupted run: frozen filesystems, snapshots, LVM snapshots.
pub fn cleanup_leftovers() {
    freeze::thaw_leftovers();
    for m in mounts::backup_mounts(&mounts::read()) {
        if m.fstype == "btrfs" {
            btrfs::cleanup_leftovers(Path::new(&m.path));
        }
    }
    if Path::new("/sbin/lvm").exists() || Path::new("/usr/sbin/lvm").exists() {
        lvm::cleanup_leftovers();
    }
}

fn prepare(mounts: &[Mount], plan: &PcPlan, run: &str) -> Vec<Source> {
    let mut out = Vec::new();
    for m in mounts {
        let mut s = Source {
            mount: m.clone(),
            method: CaptureMethod::Hot,
            read: PathBuf::from(&m.path),
            nested: Vec::new(),
            snaps: Vec::new(),
            lvm: None,
            note: None,
        };
        match m.fstype.as_str() {
            "btrfs" => match btrfs::snapshot(Path::new(&m.path), Path::new(&m.path), run) {
                Ok(snap) => {
                    s.method = CaptureMethod::BtrfsSnapshot;
                    s.read = snap.clone();
                    s.snaps.push(snap);
                    for (i, (rel, _)) in btrfs::nested(Path::new(&m.path), 6).into_iter().enumerate() {
                        if btrfs::excluded_subvolume(&rel) {
                            continue;
                        }
                        match btrfs::snapshot(&Path::new(&m.path).join(&rel), Path::new(&m.path), &format!("{run}-n{i}")) {
                            Ok(n) => {
                                s.snaps.push(n.clone());
                                s.nested.push((rel, n));
                            }
                            Err(e) => say(format!("{}/{rel}: no snapshot ({e}); left out", m.path)),
                        }
                    }
                }
                Err(e) => {
                    say(format!("{}: no snapshot ({e}); read in use", m.path));
                    s.note = Some(e);
                }
            },
            "vfat" => s.method = CaptureMethod::Files,
            _ => {
                if let Some((vg, lv)) = lvm_of(m) {
                    match lvm::snapshot(&vg, &lv) {
                        Ok(snap) => {
                            s.method = CaptureMethod::LvmSnapshot;
                            s.lvm = Some(snap);
                        }
                        Err(e) => {
                            say(format!("{}: no LVM snapshot ({e})", m.path));
                            s.note = Some(e);
                        }
                    }
                }
                if s.method == CaptureMethod::Hot && plan.freeze == FreezeMode::Freeze {
                    s.method = CaptureMethod::Frozen;
                }
            }
        }
        out.push(s);
    }
    out
}

/// Binds every source at its mount point below the stage, parents first.
fn build_stage(sources: &mut [Source]) -> Result<(), String> {
    std::fs::create_dir_all(STAGE).map_err(|e| format!("{STAGE}: {e}"))?;
    mount_raw(Some(Path::new("tmpfs")), Path::new(STAGE), Some("tmpfs"), 0, Some("mode=0700,size=1m"))?;
    sources.sort_by_key(|s| s.mount.path.matches('/').count() + usize::from(s.mount.path != "/"));
    for s in sources.iter_mut() {
        let dst = stage_path(&s.mount.path);
        if !dst.is_dir() {
            s.method = CaptureMethod::Skipped;
            s.note = Some("its mount point is missing in the parent filesystem".into());
            say(format!("{}: left out ({})", s.mount.path, s.note.as_deref().unwrap_or("")));
            continue;
        }
        if let Some(l) = &s.lvm {
            let opts = if s.mount.fstype == "xfs" { "ro,nouuid" } else { "ro" };
            if let Err(e) = mount_raw(Some(Path::new(&l.dev)), &dst, Some(&s.mount.fstype), libc::MS_RDONLY, Some(opts)) {
                say(format!("{}: the LVM snapshot does not mount ({e}); read in use", s.mount.path));
                l.remove();
                s.lvm = None;
                s.method = CaptureMethod::Hot;
            } else {
                continue;
            }
        }
        let src = if s.method == CaptureMethod::BtrfsSnapshot { s.read.clone() } else { PathBuf::from(&s.mount.path) };
        bind_ro(&src, &dst)?;
        for (rel, snap) in &s.nested {
            let d = dst.join(rel);
            if d.is_dir() {
                bind_ro(snap, &d)?;
            }
        }
    }
    Ok(())
}

fn teardown(sources: &mut [Source]) {
    // the deepest first
    let mut paths: Vec<PathBuf> = Vec::new();
    for s in sources.iter() {
        let dst = stage_path(&s.mount.path);
        for (rel, _) in &s.nested {
            paths.push(dst.join(rel));
        }
        paths.push(dst);
    }
    paths.sort_by_key(|p| std::cmp::Reverse(p.as_os_str().len()));
    for p in &paths {
        umount_lazy(p);
    }
    umount_lazy(Path::new(STAGE));
    for s in sources.iter_mut() {
        for snap in s.snaps.iter().rev() {
            if let Err(e) = btrfs::delete(snap) {
                say(e);
            }
        }
        if let Some(parent) = s.snaps.first().and_then(|p| p.parent()) {
            let _ = std::fs::remove_dir(parent);
        }
        if let Some(l) = s.lvm.take() {
            l.remove();
        }
    }
}

/// Globs for rustic: anchored ones moved below the stage.
fn excludes(plan: &PcPlan) -> Vec<String> {
    let mut v: Vec<String> = BUILTIN_EXCLUDES.iter().map(|s| s.to_string()).collect();
    // swap files
    for l in std::fs::read_to_string("/proc/swaps").unwrap_or_default().lines().skip(1) {
        let mut it = l.split_whitespace();
        if let (Some(p), Some("file")) = (it.next(), it.next()) {
            v.push(mounts::unescape(p));
        }
    }
    v.extend(plan.excludes.iter().cloned());
    v.into_iter()
        .map(|g| {
            let neg = g.starts_with('!');
            let body = g.trim_start_matches('!');
            let anchored = if body.starts_with('/') { format!("{STAGE}{body}") } else { body.to_string() };
            // rustic's globs exclude with a leading `!`
            if neg { anchored } else { format!("!{anchored}") }
        })
        .collect()
}

/// Files of hot-read filesystems changed since `since` (seconds), up to `max` names, at most
/// `budget` of walking.
fn changed_since(roots: &[String], skip: &[String], since: i64, max: usize, budget: Duration) -> (Vec<String>, u64) {
    use std::os::unix::fs::MetadataExt;
    let start = Instant::now();
    let mut names = Vec::new();
    let mut count = 0u64;
    for root in roots {
        let dev = std::fs::symlink_metadata(root).map(|m| m.dev()).unwrap_or(0);
        let mut stack = vec![PathBuf::from(root)];
        while let Some(d) = stack.pop() {
            if start.elapsed() > budget {
                return (names, count);
            }
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                let Ok(m) = e.metadata() else { continue };
                if m.dev() != dev {
                    continue;
                }
                let ps = p.to_string_lossy();
                if skip.iter().any(|s| ps == s.as_str() || ps.starts_with(&format!("{s}/"))) {
                    continue;
                }
                if m.is_dir() {
                    stack.push(p);
                } else if m.is_file() && m.ctime() >= since {
                    count += 1;
                    if names.len() < max {
                        names.push(ps.into_owned());
                    }
                }
            }
        }
    }
    (names, count)
}

/// Reports progress every 2 s until `stop`.
fn reporter(client: Arc<Client>, run_id: String, c: Arc<Counters>, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            let phase = c.phase.lock().map(|p| *p).unwrap_or_default();
            let s = AgentStatus {
                run_id: run_id.clone(),
                phase,
                bytes_done: c.bytes_done.load(Ordering::Relaxed),
                bytes_total: c.bytes_total.load(Ordering::Relaxed),
                files_done: c.files_done.load(Ordering::Relaxed),
                files_total: 0,
                message: None,
            };
            let _: Result<serde_json::Value, String> = client.post_json("/api/v1/pcbackup/agent/status", &s);
            for _ in 0..20 {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    })
}

pub fn client_of(st: &State) -> Result<Arc<Client>, String> {
    let ep = Endpoint::parse(&st.url)?;
    Ok(Arc::new(Client::new(ep, &st.fingerprint).with_token(&st.token)))
}

/// One backup, start to report. `private_mounts` must have run already (the caller does it
/// first thing, before any thread).
pub fn run(st: &State) -> PcRun {
    let started_ms = now_ms();
    let run_id = format!("run-{started_ms}");
    let mut out = PcRun {
        run_id: run_id.clone(),
        started_ms,
        ..Default::default()
    };
    let client = match client_of(st) {
        Ok(c) => c,
        Err(e) => {
            out.error = Some(e);
            out.ended_ms = now_ms();
            return out;
        }
    };
    let counters = Arc::new(Counters::default());
    let stop = Arc::new(AtomicBool::new(false));
    let rep = reporter(client.clone(), run_id.clone(), counters.clone(), stop.clone());
    let set_phase = |p: AgentPhase| {
        if let Ok(mut x) = counters.phase.lock() {
            *x = p;
        }
    };
    let result = run_inner(st, &client, &counters, &set_phase, &run_id, started_ms);
    stop.store(true, Ordering::SeqCst);
    let _ = rep.join();
    speak_again();
    match result {
        Ok((r, manifest)) => {
            out = r;
            out.ok = true;
            send_report(&client, out.clone(), manifest);
        }
        Err(f) => {
            let (e, captures, manifest) = *f;
            out.error = Some(e);
            out.captures = captures;
            out.ended_ms = now_ms();
            send_report(&client, out.clone(), manifest);
        }
    }
    out
}

fn send_report(client: &Client, run: PcRun, manifest: Option<Manifest>) {
    let r = AgentReport {
        run,
        manifest,
        agent_version: env!("CARGO_PKG_VERSION").into(),
    };
    for attempt in 0..5 {
        match client.post_json::<_, serde_json::Value>("/api/v1/pcbackup/agent/report", &r) {
            Ok(_) => return,
            Err(e) => {
                say(format!("report to the NAS: {e}"));
                std::thread::sleep(Duration::from_secs(5 << attempt));
            }
        }
    }
}

type Failure = Box<(String, Vec<Capture>, Option<Manifest>)>;

fn run_inner(
    st: &State,
    client: &Arc<Client>,
    counters: &Arc<Counters>,
    set_phase: &dyn Fn(AgentPhase),
    run_id: &str,
    started_ms: u64,
) -> Result<(PcRun, Option<Manifest>), Failure> {
    let plan = &st.plan.plan;
    set_phase(AgentPhase::Snapshot);
    let all = mounts::read();
    let backed = mounts::backup_mounts(&all);
    // what each mount point will be (for the manifest written before the snapshots)
    let mut planned: BTreeMap<String, CaptureMethod> = BTreeMap::new();
    for m in &backed {
        let method = match m.fstype.as_str() {
            "btrfs" => CaptureMethod::BtrfsSnapshot,
            "vfat" => CaptureMethod::Files,
            _ if lvm_of(m).is_some() => CaptureMethod::LvmSnapshot,
            _ if plan.freeze == FreezeMode::Freeze => CaptureMethod::Frozen,
            _ => CaptureMethod::Hot,
        };
        planned.insert(m.path.clone(), method);
    }
    let man = manifest::collect(&planned);
    if let Err(e) = manifest::write(&man, Path::new("/")) {
        say(format!("manifest: {e}"));
    }
    // a sync so the manifest is in every snapshot
    // SAFETY: plain syscall
    unsafe { libc::sync() };
    let mut sources = prepare(&backed, plan, run_id);
    let captures = |sources: &[Source]| -> Vec<Capture> {
        sources
            .iter()
            .map(|s| Capture {
                path: s.mount.path.clone(),
                fstype: s.mount.fstype.clone(),
                method: s.method,
                note: s.note.clone(),
            })
            .collect()
    };
    if let Err(e) = build_stage(&mut sources) {
        let c = captures(&sources);
        teardown(&mut sources);
        return Err(Box::new((e, c, Some(man))));
    }
    // freezes, with the watchdog
    let mut frozen: Vec<freeze::Freeze> = Vec::new();
    let thawed_early = Arc::new(AtomicBool::new(false));
    if plan.freeze == FreezeMode::Freeze {
        QUIET.store(true, Ordering::SeqCst);
        for s in sources.iter_mut().filter(|s| s.method == CaptureMethod::Frozen) {
            match freeze::Freeze::new(&s.mount.path) {
                Ok(f) => frozen.push(f),
                Err(e) => {
                    s.method = CaptureMethod::Hot;
                    s.note = Some(format!("freeze refused: {e}"));
                }
            }
        }
    }
    let frozen = Arc::new(Mutex::new(frozen));
    let watchdog_done = Arc::new(AtomicBool::new(false));
    let watchdog = {
        let (frozen, done, early) = (frozen.clone(), watchdog_done.clone(), thawed_early.clone());
        let limit = Duration::from_secs(u64::from(plan.freeze_max_secs.max(5)));
        std::thread::spawn(move || {
            let t = Instant::now();
            while t.elapsed() < limit {
                if done.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            if let Ok(mut f) = frozen.lock()
                && !f.is_empty()
            {
                f.clear();
                early.store(true, Ordering::SeqCst);
                speak_again();
                say("freeze limit reached: thawed, the rest is read in use");
            }
        })
    };
    set_phase(AgentPhase::Uploading);
    let freeze_mode = plan.freeze == FreezeMode::Freeze;
    let backup = (|| -> Result<rustic_core::repofile::SnapshotFile, String> {
        let backends = crate::backend::NasBackend::new(client.clone(), &st.group).backends();
        let opts = if freeze_mode {
            RepositoryOptions::default().no_cache(true)
        } else {
            let _ = std::fs::create_dir_all(CACHE);
            RepositoryOptions::default().cache_dir(PathBuf::from(CACHE))
        };
        let repo = Repository::new_with_progress(&opts, &backends, Bars(counters.clone()))
            .map_err(|e| e.to_string())?
            .open(&Credentials::Password(st.repo_password.clone()))
            .map_err(|e| format!("opening the repository: {e}"))?
            .to_indexed_ids()
            .map_err(|e| e.to_string())?;
        let tags = format!("{AGENT_TAG},{}", device_tag(&st.device));
        let snap = SnapshotOptions::default()
            .host(Some(manifest::hostname()))
            .add_tags(&tags)
            .map_err(|e| e.to_string())?
            .to_snapshot()
            .map_err(|e| e.to_string())?;
        let source = PathList::from_iter([PathBuf::from(STAGE)]).sanitize().map_err(|e| e.to_string())?;
        let bopts = BackupOptions::default()
            .as_path(Some(PathBuf::from("/")))
            .excludes(Excludes::default().globs(excludes(plan)))
            .ignore_filter_opts(LocalSourceFilterOptions::default().exclude_if_present(vec!["CACHEDIR.TAG".to_string(), ".nobackup".to_string()]));
        repo.backup(&bopts, &source, snap).map_err(|e| e.to_string())
    })();
    watchdog_done.store(true, Ordering::SeqCst);
    let _ = watchdog.join();
    if let Ok(mut f) = frozen.lock() {
        f.clear();
    }
    speak_again();
    if thawed_early.load(Ordering::SeqCst) {
        for s in sources.iter_mut().filter(|s| s.method == CaptureMethod::Frozen) {
            s.method = CaptureMethod::FrozenThawed;
        }
    }
    set_phase(AgentPhase::Finishing);
    teardown(&mut sources);
    let caps = captures(&sources);
    let snap = match backup {
        Ok(s) => s,
        Err(e) => return Err(Box::new((e, caps, Some(man)))),
    };
    // files changed while read, on the filesystems read in use
    let hot: Vec<String> = sources
        .iter()
        .filter(|s| matches!(s.method, CaptureMethod::Hot | CaptureMethod::FrozenThawed))
        .map(|s| s.mount.path.clone())
        .collect();
    let skip: Vec<String> = ["/proc", "/sys", "/dev", "/run", "/tmp", "/var/tmp", "/var/cache", CACHE].iter().map(|s| s.to_string()).collect();
    let (changed, changed_count) = changed_since(&hot, &skip, (started_ms / 1000) as i64, MAX_CHANGED, Duration::from_secs(300));
    let summary = snap.summary.clone();
    Ok((
        PcRun {
            run_id: run_id.to_string(),
            started_ms,
            ended_ms: now_ms(),
            ok: true,
            snapshot: Some(snap.id.to_string()),
            files: summary.as_ref().map(|s| s.total_files_processed).unwrap_or(0),
            bytes_read: summary.as_ref().map(|s| s.total_bytes_processed).unwrap_or(0),
            bytes_added: summary.as_ref().map(|s| s.data_added).unwrap_or(0),
            changed_during: changed,
            changed_count,
            error: None,
            captures: caps,
            restore: false,
        },
        Some(man),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclude_globs() {
        let p = PcPlan {
            excludes: vec!["/data/scratch".into(), "*.iso".into()],
            ..Default::default()
        };
        let g = excludes(&p);
        assert!(g.contains(&format!("!{STAGE}/proc/*")));
        assert!(g.contains(&format!("!{STAGE}/data/scratch")));
        assert!(g.contains(&"!*.iso".to_string()));
        assert!(g.contains(&"!.rynas-snap".to_string()));
    }

    #[test]
    fn stage_paths() {
        assert_eq!(stage_path("/"), PathBuf::from(STAGE));
        assert_eq!(stage_path("/boot/efi"), PathBuf::from(format!("{STAGE}/boot/efi")));
    }
}
