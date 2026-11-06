//! The transfer jobs, kept by rynasd: it runs their workers (already the user who asked),
//! follows their progress and remembers how each one went, so a transfer goes on when the
//! browser closes and its dialog, opened again, shows the whole history (speed series,
//! current file, conflicts waiting for an answer, pause, live speed limit).
//!
//! Each running job is also a rynas task (UPID, task panel, `task.*` events, cancel)
//! through [`Hooks`]: rynasd implements them over `rynas-tasks`.
//!
//! Synchronous on purpose (threads and a mutex, as SynoFile's daemon): rynasd calls it from
//! blocking threads; the workers' pipes are read by one thread per running job.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rynas_api_types::error_codes;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::protocol::{Command, ConflictInfo, Event, Op, Policy, Progress, Spec, Status};
use crate::identity::User;
use crate::wire::ApiError;

const MAX_PARALLEL: usize = 2;
const SAMPLE_EVERY: Duration = Duration::from_millis(500);
/// Speed series cap per job (M1): past it the older half is thinned.
const MAX_SAMPLES: usize = 2400;
/// Error list cap per job (M1).
const MAX_ERRORS: usize = 500;
const KEEP_FINISHED: Duration = Duration::from_secs(24 * 3600);
/// Jobs kept in memory, all users (M1): the oldest finished go first.
const MAX_JOBS: usize = 200;

/// What the interface shows about the job, besides the progress.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Info {
    pub sources: Vec<String>,
    pub dest: Option<String>,
    #[serde(default)]
    pub permanent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Queued,
    Running,
    Paused,
    Conflict,
    Done,
    Failed,
    Cancelled,
}

impl State {
    pub fn finished(self) -> bool {
        matches!(self, State::Done | State::Failed | State::Cancelled)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorItem {
    pub path: String,
    pub msg: String,
    /// an `error_codes` constant, for the interface to translate
    pub code: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobView {
    pub id: u64,
    pub op: Op,
    pub info: Info,
    pub state: State,
    pub created: u64,
    pub started: Option<u64>,
    pub finished: Option<u64>,
    /// time spent running, pauses and waits for an answer left out
    pub active_ms: u64,
    pub progress: Progress,
    pub conflict: Option<ConflictInfo>,
    pub errors: Vec<ErrorItem>,
    /// English
    pub msg: String,
    /// why it ended, for the interface to translate: "" (done), `cancelled`,
    /// `interrupted`, `password_required`, or an `error_codes` constant
    pub code: String,
    pub verify: bool,
    pub limit: u64,
    pub policy: Policy,
    /// [ms since start, bytes done, files done, bytes verified]
    pub samples: Vec<[u64; 4]>,
    /// stopped by a restart of rynasd: it can be run again
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub interrupted: bool,
    /// the rynas task (UPID) while and after it runs
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

/// Job message codes that are not `error_codes`.
pub const CODE_CANCELLED: &str = "cancelled";
pub const CODE_INTERRUPTED: &str = "interrupted";

/// The rynas task behind a running job.
pub trait JobTask: Send {
    /// The task's id (UPID).
    fn id(&self) -> String;
    fn progress(&self, pct: u8);
    fn log(&self, line: &str);
    /// The job ended: `Ok(())`, cancelled, or failed with a message.
    fn finished(self: Box<Self>, state: State, msg: &str);
}

/// How jobs become rynas tasks; `cancel` of a task comes back as [`Manager::cancel_task`].
pub trait Hooks: Send + Sync {
    /// A job starts running for `owner`: `kind` is `files.copy`, `files.move`… `None` when
    /// no task could be made (the job runs anyway).
    fn job_started(&self, kind: &str, owner: &str) -> Option<Box<dyn JobTask>>;
}

/// No tasks (tests).
pub struct NoHooks;
impl Hooks for NoHooks {
    fn job_started(&self, _: &str, _: &str) -> Option<Box<dyn JobTask>> {
        None
    }
}

struct Job {
    view: JobView,
    owner: String,
    user: User,
    admin: bool,
    spec: Spec,
    stdin: Option<ChildStdin>,
    run_since: Option<Instant>,
    active: Duration,
    last_sample: Option<Instant>,
    start_instant: Option<Instant>,
    finished_at: Option<Instant>,
    force: bool,
    /// back from the journal, its temporary files still being removed: it
    /// cannot run again yet, and its journal entry stays until done
    cleaning: bool,
    task: Option<Box<dyn JobTask>>,
    last_pct: Option<u8>,
}

impl Job {
    fn set_state(&mut self, state: State, journal: &Path) {
        let running = |s: State| s == State::Running;
        if running(self.view.state) && !running(state) {
            if let Some(since) = self.run_since.take() {
                self.active += since.elapsed();
            }
        } else if !running(self.view.state) && running(state) {
            self.run_since = Some(Instant::now());
        }
        self.view.state = state;
        if state.finished() {
            self.finished_at = Some(Instant::now());
            self.view.finished = Some(now_ms());
            self.stdin = None;
            if !self.cleaning {
                let _ = std::fs::remove_file(journal_path(journal, self.view.id));
            }
            if let Some(task) = self.task.take() {
                task.finished(state, &self.view.msg);
            }
        }
    }

    fn active_ms(&self) -> u64 {
        (self.active + self.run_since.map(|s| s.elapsed()).unwrap_or_default()).as_millis() as u64
    }

    fn sample(&mut self, force: bool) {
        let Some(start) = self.start_instant else { return };
        if !force && self.last_sample.is_some_and(|t| t.elapsed() < SAMPLE_EVERY) {
            return;
        }
        self.last_sample = Some(Instant::now());
        let p = &self.view.progress;
        self.view.samples.push([start.elapsed().as_millis() as u64, p.bytes_done, p.files_done, p.verify_bytes]);
        if self.view.samples.len() > MAX_SAMPLES {
            // halve the resolution of the older half, keep the recent one sharp
            let half = self.view.samples.len() / 2;
            let old: Vec<[u64; 4]> = self.view.samples[..half].iter().step_by(2).copied().collect();
            self.view.samples.splice(..half, old);
        }
        let pct = percent(&self.view.progress);
        if pct != self.last_pct {
            self.last_pct = pct;
            if let (Some(t), Some(p)) = (&self.task, pct) {
                t.progress(p);
            }
        }
    }

    fn send(&mut self, cmd: &Command) -> bool {
        let Some(stdin) = self.stdin.as_mut() else { return false };
        let mut line = serde_json::to_vec(cmd).unwrap_or_default();
        line.push(b'\n');
        stdin.write_all(&line).and_then(|()| stdin.flush()).is_ok()
    }
}

/// 0..=100 by bytes, else by files; none while counting.
fn percent(p: &Progress) -> Option<u8> {
    if p.scanning {
        return None;
    }
    let by_bytes = (p.bytes_done.min(p.bytes_total) * 100).checked_div(p.bytes_total);
    by_bytes.or_else(|| (p.files_done.min(p.files_total) * 100).checked_div(p.files_total)).map(|v| v as u8)
}

/// What a manager needs from rynasd.
pub struct Config {
    /// `rynas-files-worker`
    pub worker: PathBuf,
    /// where unfinished jobs are journalled (`<state>/files/journal`)
    pub journal: PathBuf,
    pub hooks: Arc<dyn Hooks>,
}

struct Inner {
    jobs: Mutex<VecDeque<Job>>,
    cfg: Config,
    next_id: Mutex<u64>,
}

/// The job table. Cheap to clone.
#[derive(Clone)]
pub struct Manager {
    inner: Arc<Inner>,
}

/// `jobs/control`
#[derive(Debug, Deserialize)]
pub struct Control {
    pub id: u64,
    pub action: String,
    #[serde(default)]
    pub policy: Option<Policy>,
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub bps: Option<u64>,
    #[serde(default)]
    pub conflict: Option<u64>,
    #[serde(default)]
    pub password: Option<String>,
}

fn kind_of(op: Op) -> &'static str {
    match op {
        Op::Copy => "files.copy",
        Op::Move => "files.move",
        Op::Delete => "files.delete",
        Op::Compress => "files.compress",
        Op::Extract => "files.extract",
        Op::Cleanup => "files.cleanup",
    }
}

impl Manager {
    /// The manager, with the jobs a restart cut short shown as interrupted at once and
    /// their temporary files cleaned up in the background.
    pub fn new(cfg: Config) -> Self {
        let _ = std::fs::create_dir_all(&cfg.journal);
        let _ = std::fs::set_permissions(&cfg.journal, std::fs::Permissions::from_mode(0o700));
        let m = Self { inner: Arc::new(Inner { jobs: Mutex::new(VecDeque::new()), cfg, next_id: Mutex::new(now_ms()) }) };
        let cleanups = m.load_journal();
        if !cleanups.is_empty() {
            let m2 = m.clone();
            std::thread::spawn(move || m2.clean_up(cleanups));
        }
        m
    }

    fn journal(&self) -> &Path {
        &self.inner.cfg.journal
    }

    fn new_id(&self) -> u64 {
        let mut n = self.inner.next_id.lock().unwrap();
        *n = (*n + 1).max(now_ms());
        *n
    }

    /// `jobs/new`: queues a job for `user` (the spec already resolved by
    /// [`crate::api::job_spec`]) and starts it when there is room. `{ "id": n }`.
    pub fn submit(&self, user: User, admin: bool, mut spec: Spec, info: Info) -> Result<Value, ApiError> {
        // the clean-up of a job a restart cut short removes every temporary file in its
        // destination: a new job writing there now would lose its own
        if let Some(dest) = &info.dest {
            let list = self.inner.jobs.lock().unwrap();
            if list.iter().any(|j| j.cleaning && j.view.info.dest.as_deref().is_some_and(|d| overlaps(d, dest))) {
                return Err(ApiError::new(409, t!("the temporary files are still being cleaned up: try again shortly")).code(error_codes::BUSY));
            }
        }
        // An SFTP server with no key known yet gets it pinned now, by rynasd: the worker
        // runs as the user and takes no unknown key. A server out of reach fails in the job.
        let start = Instant::now();
        for c in &mut spec.remotes {
            if start.elapsed() < Duration::from_secs(6) {
                let _ = crate::conns::pin_host_key(&user.name, c);
            }
        }
        let id = self.new_id();
        let view = JobView {
            id,
            op: spec.op,
            info,
            state: State::Queued,
            created: now_ms(),
            started: None,
            finished: None,
            active_ms: 0,
            progress: Progress::default(),
            conflict: None,
            errors: Vec::new(),
            msg: String::new(),
            code: String::new(),
            verify: spec.verify,
            limit: spec.limit,
            policy: spec.conflict,
            samples: Vec::new(),
            interrupted: false,
            task_id: None,
        };
        journal(self.journal(), &JournalEntry { id, user: user.clone(), admin, spec: spec.without_secrets(), info: view.info.clone(), created: view.created });
        // deletions are quick and should not wait behind a big copy
        let force = spec.op == Op::Delete;
        let job = Job {
            view,
            owner: user.name.clone(),
            user,
            admin,
            spec,
            stdin: None,
            run_since: None,
            active: Duration::ZERO,
            last_sample: None,
            start_instant: None,
            finished_at: None,
            force,
            cleaning: false,
            task: None,
            last_pct: None,
        };
        {
            let mut list = self.inner.jobs.lock().unwrap();
            prune(&mut list);
            list.push_back(job);
        }
        self.schedule();
        Ok(json!({ "id": id }))
    }

    /// `jobs`: the user's jobs, with only the samples taken after `since` (ms).
    pub fn list(&self, user: &str, since: u64) -> Value {
        let list = self.inner.jobs.lock().unwrap();
        let views: Vec<JobView> = list
            .iter()
            .filter(|j| j.owner == user)
            .map(|j| {
                let mut v = j.view.clone();
                v.active_ms = j.active_ms();
                let started = v.started.unwrap_or(0);
                v.samples.retain(|s| started + s[0] > since);
                v
            })
            .collect();
        json!({ "jobs": views, "now": now_ms() })
    }

    /// Whether `user` has a job that is not finished (queued, running, paused, waiting for
    /// an answer).
    pub fn busy(&self, user: &str) -> bool {
        self.inner.jobs.lock().unwrap().iter().any(|j| j.owner == user && !j.view.state.finished())
    }

    /// The rynas task of a job was cancelled (task panel): the job is cancelled too.
    pub fn cancel_task(&self, task_id: &str) {
        let found = {
            let list = self.inner.jobs.lock().unwrap();
            list.iter().find(|j| j.view.task_id.as_deref() == Some(task_id)).map(|j| (j.owner.clone(), j.view.id))
        };
        if let Some((owner, id)) = found {
            let _ = self.control(&owner, Control { id, action: "cancel".into(), policy: None, all: false, bps: None, conflict: None, password: None });
        }
    }

    /// `jobs/control`: pause, resume, cancel, answer, limit, start, retry, dismiss.
    pub fn control(&self, user: &str, c: Control) -> Result<Value, ApiError> {
        let journal = self.journal().to_path_buf();
        let mut list = self.inner.jobs.lock().unwrap();
        let Some(index) = list.iter().position(|j| j.view.id == c.id && j.owner == user) else {
            return Err(ApiError::not_found(t!("no such transfer")));
        };
        let job = &mut list[index];
        let ok = match c.action.as_str() {
            "pause" if job.view.state == State::Running => {
                let ok = job.send(&Command::Pause);
                if ok {
                    job.set_state(State::Paused, &journal);
                }
                ok
            }
            "resume" if job.view.state == State::Paused => {
                let ok = job.send(&Command::Resume);
                if ok {
                    let next = if job.view.conflict.is_some() { State::Conflict } else { State::Running };
                    job.set_state(next, &journal);
                }
                ok
            }
            "cancel" if job.view.state == State::Queued => {
                job.view.msg = t!("cancelled");
                job.view.code = CODE_CANCELLED.into();
                job.set_state(State::Cancelled, &journal);
                true
            }
            "cancel" if !job.view.state.finished() => job.send(&Command::Cancel),
            "answer" if job.view.state == State::Conflict || job.view.state == State::Paused => {
                let Some(waiting) = job.view.conflict.as_ref().map(|c| c.id) else {
                    return Err(ApiError::conflict(t!("no conflict waiting")));
                };
                if c.conflict.is_some_and(|x| x != waiting) {
                    return Err(ApiError::conflict(t!("conflict already settled")));
                }
                let action = c.policy.unwrap_or(Policy::Skip);
                let ok = job.send(&Command::Answer { id: waiting, action, all: c.all });
                if ok {
                    job.view.conflict = None;
                    if c.all {
                        job.view.policy = action;
                    }
                    if job.view.state == State::Conflict {
                        job.set_state(State::Running, &journal);
                    }
                }
                ok
            }
            "limit" => {
                let bps = c.bps.unwrap_or(0);
                job.view.limit = bps;
                job.spec.limit = bps;
                job.view.state == State::Queued || job.send(&Command::Limit { bps })
            }
            "start" if job.view.state == State::Queued => {
                job.force = true;
                drop(list);
                self.schedule();
                return Ok(json!({ "ok": true }));
            }
            "retry" | "dismiss" if job.cleaning => {
                return Err(ApiError::new(409, t!("the temporary files are still being cleaned up: try again shortly")).code(error_codes::BUSY));
            }
            "retry" if job.view.state.finished() => {
                // the same job again, with the archive's password
                let mut spec = job.spec.clone();
                if c.password.is_some() {
                    spec.password = c.password;
                }
                let (user, admin, info) = (job.user.clone(), job.admin, job.view.info.clone());
                list.remove(index);
                drop(list);
                // the connections as saved now: a job back from the journal has no
                // secrets, and a password may have changed
                if !spec.remotes.is_empty() {
                    spec.remotes = crate::conns::with_secrets(&user.name, &spec.remotes);
                }
                return self.submit(user, admin, spec, info);
            }
            "dismiss" if job.view.state.finished() => {
                list.remove(index);
                true
            }
            _ => false,
        };
        Ok(json!({ "ok": ok }))
    }

    /// Starts queued jobs while there is room, and forced ones at once.
    fn schedule(&self) {
        let journal = self.journal().to_path_buf();
        let mut list = self.inner.jobs.lock().unwrap();
        let mut running = list.iter().filter(|j| !j.view.state.finished() && j.view.state != State::Queued).count();
        let ids: Vec<u64> = list.iter().filter(|j| j.view.state == State::Queued && !j.cleaning).map(|j| j.view.id).collect();
        for id in ids {
            let job = list.iter_mut().find(|j| j.view.id == id).expect("listed above");
            if running >= MAX_PARALLEL && !job.force {
                continue;
            }
            running += 1;
            match self.spawn(job) {
                Ok(stdout) => {
                    let m = self.clone();
                    std::thread::spawn(move || m.follow(id, stdout));
                }
                Err(e) => {
                    job.view.msg = t!("the copy cannot start: {}", e);
                    job.view.code = error_codes::INTERNAL.into();
                    job.set_state(State::Failed, &journal);
                    running -= 1;
                }
            }
        }
    }

    fn spawn(&self, job: &mut Job) -> io::Result<std::process::ChildStdout> {
        let mut child = start_worker(&self.inner.cfg.worker, &job.user, job.admin, &job.spec)?;
        let stdin = child.stdin.take().ok_or_else(|| io::Error::other("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| io::Error::other("no stdout"))?;
        job.stdin = Some(stdin);
        job.view.started = Some(now_ms());
        job.start_instant = Some(Instant::now());
        job.task = self.inner.cfg.hooks.job_started(kind_of(job.spec.op), &job.owner);
        job.view.task_id = job.task.as_ref().map(|t| t.id());
        if let Some(t) = &job.task {
            t.log(&describe_job(&job.view));
        }
        job.set_state(State::Running, self.journal());
        job.sample(true);
        // reap the child when it ends, whatever happened
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(stdout)
    }

    fn follow(&self, id: u64, stdout: std::process::ChildStdout) {
        let journal = self.journal().to_path_buf();
        let mut done = false;
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let Ok(event) = serde_json::from_str::<Event>(&line) else { continue };
            let mut list = self.inner.jobs.lock().unwrap();
            let Some(job) = list.iter_mut().find(|j| j.view.id == id) else { return };
            match event {
                Event::Progress(p) => {
                    job.view.progress = p;
                    job.sample(false);
                }
                Event::Conflict(c) => {
                    job.view.conflict = Some(c);
                    if job.view.state == State::Running {
                        job.set_state(State::Conflict, &journal);
                    }
                }
                Event::Error { path, msg, code } => {
                    if let Some(t) = &job.task {
                        t.log(&format!("{path}: {msg}"));
                    }
                    if job.view.errors.len() < MAX_ERRORS {
                        job.view.errors.push(ErrorItem { path, msg, code });
                    }
                }
                Event::Done { status, msg, code } => {
                    job.sample(true);
                    job.view.msg = msg;
                    job.view.code = match status {
                        Status::Cancelled => CODE_CANCELLED.into(),
                        _ => code,
                    };
                    job.view.conflict = None;
                    let state = match status {
                        Status::Ok => State::Done,
                        Status::Cancelled => State::Cancelled,
                        Status::Failed => State::Failed,
                    };
                    job.set_state(state, &journal);
                    done = true;
                }
            }
        }
        if !done {
            let mut list = self.inner.jobs.lock().unwrap();
            if let Some(job) = list.iter_mut().find(|j| j.view.id == id) {
                job.sample(true);
                job.view.msg = t!("the copy process stopped unexpectedly");
                job.view.code = error_codes::INTERNAL.into();
                job.set_state(State::Failed, &journal);
            }
        }
        self.schedule();
    }

    // ---- the journal: jobs not finished yet, on disk ----
    //
    // A restart of rynasd (an update, the NAS rebooting) stops the workers. The jobs are
    // not resumed on their own: after an update that could surprise, and half a copy
    // cannot be trusted. Instead their temporary files are cleaned up and they come back
    // as interrupted, with Retry.

    /// The journalled jobs, shown as interrupted; those with a destination to clean.
    fn load_journal(&self) -> Vec<(u64, User, Spec)> {
        let mut entries: Vec<JournalEntry> = std::fs::read_dir(self.journal())
            .map(|d| {
                d.flatten()
                    .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                    .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
                    .collect()
            })
            .unwrap_or_default();
        entries.sort_by_key(|e| e.id);
        let mut cleanups = Vec::new();
        let journal = self.journal().to_path_buf();
        let mut list = self.inner.jobs.lock().unwrap();
        for e in entries.into_iter().rev() {
            tracing::info!("file job {} of {} was interrupted by a restart", e.id, e.user.name);
            let needs_cleanup = e.spec.op != Op::Delete && e.spec.dest.is_some();
            if needs_cleanup {
                let mut clean = e.spec.clone();
                clean.op = Op::Cleanup;
                cleanups.push((e.id, e.user.clone(), clean));
            } else {
                let _ = std::fs::remove_file(journal_path(&journal, e.id));
            }
            list.push_front(interrupted(e, needs_cleanup, &journal));
        }
        cleanups
    }

    fn clean_up(&self, cleanups: Vec<(u64, User, Spec)>) {
        for (id, user, mut clean) in cleanups {
            // remote destinations: the connections as saved, secrets included
            if !clean.remotes.is_empty() {
                clean.remotes = crate::conns::with_secrets(&user.name, &clean.remotes);
            }
            match start_worker(&self.inner.cfg.worker, &user, false, &clean) {
                Ok(mut child) => {
                    drop(child.stdin.take());
                    // the events are not wanted, but the pipe must not fill up
                    if let Some(out) = child.stdout.take() {
                        let _ = io::copy(&mut BufReader::new(out), &mut io::sink());
                    }
                    let _ = child.wait();
                }
                Err(err) => tracing::warn!("clean-up of file job {id} failed: {err}"),
            }
            let mut list = self.inner.jobs.lock().unwrap();
            if let Some(job) = list.iter_mut().find(|j| j.view.id == id) {
                job.cleaning = false;
            }
            let _ = std::fs::remove_file(journal_path(self.journal(), id));
        }
    }
}

/// The first line of a job's task log.
fn describe_job(v: &JobView) -> String {
    let what = match v.op {
        Op::Copy => "copy",
        Op::Move => "move",
        Op::Delete => "delete",
        Op::Compress => "compress",
        Op::Extract => "extract",
        Op::Cleanup => "clean-up",
    };
    let first = v.info.sources.first().map(String::as_str).unwrap_or_default();
    let more = v.info.sources.len().saturating_sub(1);
    let others = if more > 0 { format!(" and {more} more") } else { String::new() };
    match &v.info.dest {
        Some(d) => format!("{what} {first}{others} to {d}"),
        None => format!("{what} {first}{others}"),
    }
}

fn prune(list: &mut VecDeque<Job>) {
    list.retain(|j| j.finished_at.is_none_or(|t| t.elapsed() < KEEP_FINISHED));
    while list.len() >= MAX_JOBS {
        match list.iter().position(|j| j.view.state.finished()) {
            Some(i) => {
                list.remove(i);
            }
            None => break,
        }
    }
}

/// A worker for `spec`, running as `user` (as the user this process is in development,
/// when rynasd is not root), with `spec` already sent.
fn start_worker(exe: &Path, user: &User, admin: bool, spec: &Spec) -> io::Result<std::process::Child> {
    let mut cmd = worker_command(exe, user, admin)?;
    cmd.arg("job").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
    let mut child = cmd.spawn()?;
    let mut line = serde_json::to_vec(spec).map_err(io::Error::other)?;
    line.push(b'\n');
    child.stdin.as_mut().ok_or_else(|| io::Error::other("no stdin"))?.write_all(&line)?;
    Ok(child)
}

/// `rynas-files-worker` as `user`: an empty environment but `PATH`, the user's identity set
/// between fork and exec ([`crate::identity::become_raw`]). A process that is not root
/// (development) cannot switch: the worker then runs as that process's own user, never as
/// root.
pub fn worker_command(exe: &Path, user: &User, admin: bool) -> io::Result<std::process::Command> {
    let mut cmd = std::process::Command::new(exe);
    cmd.env_clear().env("PATH", "/usr/bin:/bin").env("LANG", "C.UTF-8").current_dir("/");
    // SAFETY: plain getter
    if unsafe { libc::geteuid() } == 0 {
        if user.uid == 0 {
            return Err(io::Error::other("the file manager never runs as root"));
        }
        let groups = user.groups.clone();
        let (uid, gid) = (user.uid, user.gid);
        // SAFETY: only system calls between fork and exec (become_raw allocates nothing)
        unsafe {
            cmd.pre_exec(move || crate::identity::become_raw(uid, gid, &groups, admin));
        }
    }
    Ok(cmd)
}

#[derive(Serialize, Deserialize)]
struct JournalEntry {
    id: u64,
    user: User,
    #[serde(default)]
    admin: bool,
    spec: Spec,
    info: Info,
    created: u64,
}

fn journal_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn journal(dir: &Path, entry: &JournalEntry) {
    // no secrets in it (`Spec::without_secrets`), still root's only: it names the user's
    // files
    if let Ok(bytes) = serde_json::to_vec(entry) {
        let _ = crate::store::write_private(&journal_path(dir, entry.id), &bytes);
    }
}

/// Whether one folder is the other or holds it.
fn overlaps(a: &str, b: &str) -> bool {
    let inside = |x: &str, y: &str| x == y || x.strip_prefix(y).is_some_and(|rest| rest.starts_with('/') || y.ends_with('/'));
    inside(a, b) || inside(b, a)
}

/// A job of the journal, as the interface shows it after a restart.
fn interrupted(e: JournalEntry, cleaning: bool, journal: &Path) -> Job {
    let view = JobView {
        id: e.id,
        op: e.spec.op,
        info: e.info,
        state: State::Queued,
        created: e.created,
        started: None,
        finished: None,
        active_ms: 0,
        progress: Progress::default(),
        conflict: None,
        errors: Vec::new(),
        msg: t!("interrupted by a restart"),
        code: CODE_INTERRUPTED.into(),
        verify: e.spec.verify,
        limit: e.spec.limit,
        policy: e.spec.conflict,
        samples: Vec::new(),
        interrupted: true,
        task_id: None,
    };
    let mut job = Job {
        view,
        owner: e.user.name.clone(),
        user: e.user,
        admin: e.admin,
        spec: e.spec,
        stdin: None,
        run_since: None,
        active: Duration::ZERO,
        last_sample: None,
        start_instant: None,
        finished_at: None,
        force: false,
        cleaning,
        task: None,
        last_pct: None,
    };
    job.set_state(State::Failed, journal);
    job
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_overlap() {
        assert!(overlaps("/Film", "/Film"));
        assert!(overlaps("/Film", "/Film/2024"));
        assert!(overlaps("/Film/2024", "/Film"));
        assert!(!overlaps("/Film", "/Filmati"));
        assert!(!overlaps("/Film/a", "/Film/b"));
    }

    #[test]
    fn percent_by_bytes_then_files() {
        let mut p = Progress { bytes_total: 200, bytes_done: 50, ..Default::default() };
        assert_eq!(percent(&p), Some(25));
        p.scanning = true;
        assert_eq!(percent(&p), None);
        let p = Progress { files_total: 4, files_done: 3, ..Default::default() };
        assert_eq!(percent(&p), Some(75));
        assert_eq!(percent(&Progress::default()), None);
    }
}
