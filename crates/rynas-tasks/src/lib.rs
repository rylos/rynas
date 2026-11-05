//! rynas task engine, modelled on the Proxmox UPID tasks.
//!
//! A task is an async job with an id (`UPID:<node>:<pid>:<seq>:<start>:<type>:<user>:`), a
//! type, the user who started it, start/end time, a status, an optional progress and a log
//! file under the state directory (`<dir>/<id>.log`, capped). Tasks beyond the concurrency
//! limit wait in a queue. Cancellation is cooperative: the job sees its
//! [`CancellationToken`] fire, and external commands started with [`TaskCtx::run`] are killed
//! (whole process group). History is kept in `<dir>/index.json` with a retention by count
//! and age; tasks that were running when the daemon stopped are marked as failed on restart.
//! Every change is broadcast as a [`TaskEvent`] (the UI task panel listens over SSE).

pub mod cmd;

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use rynas_api_types::{TaskEvent, TaskEventKind, TaskInfo, TaskStatus};
use tokio::sync::{Semaphore, broadcast};
pub use tokio_util::sync::CancellationToken;

pub use cmd::{Cmd, CmdError, CmdOutput};

/// Limits and retention.
#[derive(Debug, Clone)]
pub struct TaskConfig {
    /// Tasks running at the same time; the others wait in the queue.
    pub max_concurrent: usize,
    /// Tasks waiting in the queue; beyond it [`TaskManager::spawn`] fails with `Busy`.
    pub max_queued: usize,
    /// Size cap of a task log; the end marker is always written.
    pub log_cap: u64,
    /// Finished tasks kept in the history...
    pub keep: usize,
    /// ...and for at most this long.
    pub keep_age: Duration,
}

impl Default for TaskConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 4,
            max_queued: 32,
            log_cap: 1 << 20,
            keep: 500,
            keep_age: Duration::from_secs(30 * 86400),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskError {
    NotFound,
    /// The task has already ended.
    Finished,
    /// Too many tasks queued.
    Busy,
    Io(String),
}

impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskError::NotFound => write!(f, "no such task"),
            TaskError::Finished => write!(f, "the task has already ended"),
            TaskError::Busy => write!(f, "too many tasks queued, retry later"),
            TaskError::Io(e) => write!(f, "task storage: {e}"),
        }
    }
}

impl std::error::Error for TaskError {}

/// A piece of a task log, read from `offset`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogChunk {
    pub text: String,
    /// Offset to ask for next.
    pub next: u64,
    /// The task has ended and the log was read to the end.
    pub complete: bool,
}

const INDEX: &str = "index.json";
/// Capacity of the event channels; a slower subscriber lags and must re-read the list.
const EVENTS: usize = 256;

/// A task event already serialized to JSON (once, whatever the number of subscribers).
#[derive(Debug, Clone)]
pub struct SerializedEvent {
    pub kind: TaskEventKind,
    pub json: Arc<str>,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

struct Entry {
    info: TaskInfo,
    log: PathBuf,
    cancel: Option<CancellationToken>,
}

struct Inner {
    dir: PathBuf,
    node: String,
    cfg: TaskConfig,
    tasks: Mutex<VecDeque<Entry>>,
    events: broadcast::Sender<TaskEvent>,
    json_events: broadcast::Sender<SerializedEvent>,
    slots: Arc<Semaphore>,
    seq: AtomicU32,
}

/// The task engine. Cheap to clone.
#[derive(Clone)]
pub struct TaskManager {
    inner: Arc<Inner>,
}

/// Keeps only `[A-Za-z0-9._@-]` so that the UPID stays a single path segment and file name.
fn sanitize(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if s.is_empty() { "_".into() } else { s }
}

fn log_name(id: &str) -> String {
    format!("{}.log", id.replace(':', "_"))
}

impl TaskManager {
    /// Opens (creates) the task directory and loads the history.
    pub fn new(dir: impl Into<PathBuf>, node: &str, cfg: TaskConfig) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        let mut tasks = VecDeque::new();
        if let Ok(data) = std::fs::read(dir.join(INDEX)) {
            let list: Vec<TaskInfo> = serde_json::from_slice(&data).unwrap_or_else(|e| {
                tracing::warn!("{}: {e}; task history reset", dir.join(INDEX).display());
                Vec::new()
            });
            for mut info in list {
                let log = dir.join(log_name(&info.id));
                if !info.status.is_finished() {
                    info.status = TaskStatus::Error;
                    info.error = Some("interrupted: rynasd stopped while it was running".into());
                    info.end_ms = Some(now_ms());
                    append_raw(&log, "TASK ERROR: interrupted\n");
                }
                tasks.push_back(Entry {
                    info,
                    log,
                    cancel: None,
                });
            }
        }
        let (events, _) = broadcast::channel(EVENTS);
        let (json_events, _) = broadcast::channel(EVENTS);
        let m = Self {
            inner: Arc::new(Inner {
                slots: Arc::new(Semaphore::new(cfg.max_concurrent.max(1))),
                dir,
                node: sanitize(node),
                cfg,
                tasks: Mutex::new(tasks),
                events,
                json_events,
                seq: AtomicU32::new(0),
            }),
        };
        {
            let mut t = m.inner.tasks.lock().unwrap();
            m.prune(&mut t);
            m.persist(&t);
        }
        Ok(m)
    }

    /// Starts `job` as a task and returns its id at once. The job gets a [`TaskCtx`] to log,
    /// report progress, run commands and watch for cancellation; `Err(message)` marks the
    /// task failed (cancelled if it was cancelled).
    pub fn spawn<F, Fut>(&self, kind: &str, user: &str, job: F) -> Result<String, TaskError>
    where
        F: FnOnce(TaskCtx) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        let start = now_ms();
        let seq = self.inner.seq.fetch_add(1, Ordering::Relaxed);
        let (kind, user) = (sanitize(kind), sanitize(user));
        let id = format!(
            "UPID:{}:{:08X}:{:08X}:{:08X}:{kind}:{user}:",
            self.inner.node,
            std::process::id(),
            seq,
            start / 1000
        );
        let log = self.inner.dir.join(log_name(&id));
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&log)
            .map_err(|e| TaskError::Io(e.to_string()))?;
        let token = CancellationToken::new();
        let info = TaskInfo {
            id: id.clone(),
            kind,
            user,
            start_ms: start,
            status: TaskStatus::Queued,
            ..Default::default()
        };
        {
            let mut t = self.inner.tasks.lock().unwrap();
            let queued = t
                .iter()
                .filter(|e| e.info.status == TaskStatus::Queued)
                .count();
            if queued >= self.inner.cfg.max_queued {
                drop(t);
                let _ = std::fs::remove_file(&log);
                return Err(TaskError::Busy);
            }
            t.push_back(Entry {
                info: info.clone(),
                log,
                cancel: Some(token.clone()),
            });
            self.persist(&t);
        }
        self.emit(TaskEventKind::Started, info);

        let ctx = TaskCtx {
            id: id.clone(),
            manager: self.clone(),
            cancel: token.clone(),
            log: Arc::new(Mutex::new(LogWriter {
                file,
                written: 0,
                cap: self.inner.cfg.log_cap,
                truncated: false,
            })),
        };
        let m = self.clone();
        tokio::spawn(async move {
            let slots = m.inner.slots.clone();
            let permit = tokio::select! {
                p = slots.acquire_owned() => p.ok(),
                _ = token.cancelled() => None,
            };
            let Some(_permit) = permit else {
                m.finish(&ctx, Err("cancelled before it started".into()));
                return;
            };
            m.update(&ctx.id, |i| i.status = TaskStatus::Running);
            // a panic in the job must still end the task
            let result = match tokio::spawn(job(ctx.clone())).await {
                Ok(r) => r,
                Err(e) if e.is_panic() => Err("internal error: the task panicked".into()),
                Err(_) => Err("aborted".into()),
            };
            m.finish(&ctx, result);
        });
        Ok(id)
    }

    /// Tasks, newest first.
    pub fn list(&self, limit: usize) -> Vec<TaskInfo> {
        let t = self.inner.tasks.lock().unwrap();
        t.iter().rev().take(limit).map(|e| e.info.clone()).collect()
    }

    pub fn get(&self, id: &str) -> Option<TaskInfo> {
        let t = self.inner.tasks.lock().unwrap();
        t.iter().find(|e| e.info.id == id).map(|e| e.info.clone())
    }

    /// Requests cancellation. The task ends when its job notices (commands are killed).
    pub fn cancel(&self, id: &str) -> Result<(), TaskError> {
        let t = self.inner.tasks.lock().unwrap();
        let e = t
            .iter()
            .find(|e| e.info.id == id)
            .ok_or(TaskError::NotFound)?;
        match &e.cancel {
            Some(c) if !e.info.status.is_finished() => {
                c.cancel();
                Ok(())
            }
            _ => Err(TaskError::Finished),
        }
    }

    /// Up to `max` bytes of the log from `offset`. While the task runs, the chunk ends at a
    /// line boundary so that pollers never see half a line.
    pub fn read_log(&self, id: &str, offset: u64, max: usize) -> Result<LogChunk, TaskError> {
        let (path, finished) = {
            let t = self.inner.tasks.lock().unwrap();
            let e = t
                .iter()
                .find(|e| e.info.id == id)
                .ok_or(TaskError::NotFound)?;
            (e.log.clone(), e.info.status.is_finished())
        };
        let io = |e: std::io::Error| TaskError::Io(e.to_string());
        let mut f = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LogChunk {
                    text: String::new(),
                    next: offset,
                    complete: finished,
                });
            }
            Err(e) => return Err(io(e)),
        };
        let len = f.metadata().map_err(io)?.len();
        let offset = offset.min(len);
        f.seek(SeekFrom::Start(offset)).map_err(io)?;
        let mut buf = Vec::new();
        f.take(max as u64).read_to_end(&mut buf).map_err(io)?;
        let at_end = offset + buf.len() as u64 >= len;
        if !(finished && at_end)
            && let Some(nl) = buf.iter().rposition(|&b| b == b'\n')
        {
            buf.truncate(nl + 1);
        }
        let next = offset + buf.len() as u64;
        Ok(LogChunk {
            text: String::from_utf8_lossy(&buf).into_owned(),
            next,
            complete: finished && next >= len,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TaskEvent> {
        self.inner.events.subscribe()
    }

    /// Like [`TaskManager::subscribe`], with every event serialized once for all the
    /// subscribers (event streams).
    pub fn subscribe_json(&self) -> broadcast::Receiver<SerializedEvent> {
        self.inner.json_events.subscribe()
    }

    /// Waits until the task has ended.
    pub async fn wait(&self, id: &str) -> Option<TaskInfo> {
        let mut rx = self.subscribe();
        loop {
            let info = self.get(id)?;
            if info.status.is_finished() {
                return Some(info);
            }
            match rx.recv().await {
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return self.get(id),
            }
        }
    }

    fn emit(&self, kind: TaskEventKind, task: TaskInfo) {
        let ev = TaskEvent { kind, task };
        if self.inner.json_events.receiver_count() > 0
            && let Ok(json) = serde_json::to_string(&ev)
        {
            let _ = self.inner.json_events.send(SerializedEvent {
                kind,
                json: json.into(),
            });
        }
        let _ = self.inner.events.send(ev);
    }

    fn update(&self, id: &str, f: impl FnOnce(&mut TaskInfo)) {
        let info = {
            let mut t = self.inner.tasks.lock().unwrap();
            let Some(e) = t.iter_mut().find(|e| e.info.id == id) else {
                return;
            };
            let before = e.info.clone();
            f(&mut e.info);
            if e.info == before {
                return;
            }
            e.info.clone()
        };
        self.emit(TaskEventKind::Updated, info);
    }

    fn finish(&self, ctx: &TaskCtx, result: Result<(), String>) {
        let (status, error, marker) = match result {
            Ok(()) => (TaskStatus::Ok, None, "TASK OK".to_string()),
            Err(_) if ctx.cancel.is_cancelled() => (
                TaskStatus::Cancelled,
                Some("cancelled".to_string()),
                "TASK CANCELLED".to_string(),
            ),
            Err(e) => {
                let m = format!("TASK ERROR: {e}");
                (TaskStatus::Error, Some(e), m)
            }
        };
        ctx.log.lock().unwrap().write_marker(&marker);
        let info = {
            let mut t = self.inner.tasks.lock().unwrap();
            let Some(e) = t.iter_mut().find(|e| e.info.id == ctx.id) else {
                return;
            };
            e.info.status = status;
            e.info.error = error;
            e.info.end_ms = Some(now_ms());
            e.cancel = None;
            let info = e.info.clone();
            self.prune(&mut t);
            self.persist(&t);
            info
        };
        self.emit(TaskEventKind::Finished, info);
    }

    /// Drops the oldest finished tasks beyond the retention, with their logs.
    fn prune(&self, t: &mut VecDeque<Entry>) {
        let cutoff = now_ms().saturating_sub(self.inner.cfg.keep_age.as_millis() as u64);
        let finished = t.iter().filter(|e| e.info.status.is_finished()).count();
        let mut excess = finished.saturating_sub(self.inner.cfg.keep);
        t.retain(|e| {
            if !e.info.status.is_finished() {
                return true;
            }
            let old = e.info.end_ms.unwrap_or(e.info.start_ms) < cutoff;
            let drop = excess > 0 || old;
            if drop {
                excess = excess.saturating_sub(1);
                let _ = std::fs::remove_file(&e.log);
            }
            !drop
        });
    }

    fn persist(&self, t: &VecDeque<Entry>) {
        let list: Vec<&TaskInfo> = t.iter().map(|e| &e.info).collect();
        let path = self.inner.dir.join(INDEX);
        let tmp = self.inner.dir.join(format!("{INDEX}.tmp"));
        let r = serde_json::to_vec(&list)
            .map_err(std::io::Error::other)
            .and_then(|data| std::fs::write(&tmp, data))
            .and_then(|_| std::fs::rename(&tmp, &path));
        if let Err(e) = r {
            tracing::error!("{}: {e}", path.display());
        }
    }
}

fn append_raw(path: &Path, text: &str) {
    if let Ok(mut f) = OpenOptions::new().append(true).open(path) {
        let _ = f.write_all(text.as_bytes());
    }
}

struct LogWriter {
    file: File,
    written: u64,
    cap: u64,
    truncated: bool,
}

impl LogWriter {
    fn write_line(&mut self, line: &str) {
        if self.truncated {
            return;
        }
        let len = line.len() as u64 + 1;
        if self.written + len > self.cap {
            self.truncated = true;
            let _ = writeln!(
                self.file,
                "[log truncated: limit of {} bytes reached]",
                self.cap
            );
            return;
        }
        self.written += len;
        let _ = writeln!(self.file, "{line}");
    }

    fn write_marker(&mut self, line: &str) {
        let _ = writeln!(self.file, "{line}");
        let _ = self.file.flush();
    }
}

/// Handle given to a running job.
#[derive(Clone)]
pub struct TaskCtx {
    id: String,
    manager: TaskManager,
    cancel: CancellationToken,
    log: Arc<Mutex<LogWriter>>,
}

impl TaskCtx {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Appends text to the task log, one line per line.
    pub fn log(&self, text: &str) {
        let mut w = self.log.lock().unwrap();
        for l in text.lines() {
            w.write_line(l);
        }
    }

    /// Reports progress (clamped to 100).
    pub fn progress(&self, pct: u8) {
        self.manager
            .update(&self.id, |i| i.progress = Some(pct.min(100)));
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn token(&self) -> &CancellationToken {
        &self.cancel
    }

    /// Runs an external command, logging its command line and output; killed on cancel.
    pub async fn run(&self, cmd: &Cmd) -> Result<CmdOutput, CmdError> {
        self.log(&format!("$ {}", cmd.display()));
        let sink = |l: &str| self.log(l);
        cmd.run_checked(Some(&self.cancel), Some(&sink)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager(dir: &Path, cfg: TaskConfig) -> TaskManager {
        TaskManager::new(dir.join("tasks"), "nas", cfg).unwrap()
    }

    fn full_log(m: &TaskManager, id: &str) -> String {
        m.read_log(id, 0, usize::MAX).unwrap().text
    }

    #[tokio::test]
    async fn runs_logs_and_reports() {
        let dir = tempfile::tempdir().unwrap();
        let m = manager(dir.path(), TaskConfig::default());
        let mut rx = m.subscribe();
        let id = m
            .spawn("test.ok", "alice", |ctx| async move {
                ctx.log("hello\nworld");
                ctx.progress(50);
                ctx.run(&Cmd::new("echo").arg("from a command"))
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(())
            })
            .unwrap();
        assert!(id.starts_with("UPID:nas:"), "{id}");
        assert!(id.ends_with(":test.ok:alice:"), "{id}");
        let info = m.wait(&id).await.unwrap();
        assert_eq!(info.status, TaskStatus::Ok);
        assert_eq!(info.progress, Some(50));
        assert!(info.end_ms.is_some());
        assert_eq!(
            full_log(&m, &id),
            "hello\nworld\n$ echo from a command\nfrom a command\nTASK OK\n"
        );
        let mut kinds = Vec::new();
        while let Ok(e) = rx.try_recv() {
            kinds.push(e.kind);
        }
        assert_eq!(kinds.first(), Some(&TaskEventKind::Started));
        assert_eq!(kinds.last(), Some(&TaskEventKind::Finished));
        assert!(kinds.contains(&TaskEventKind::Updated));
    }

    #[tokio::test]
    async fn errors_and_panics_end_the_task() {
        let dir = tempfile::tempdir().unwrap();
        let m = manager(dir.path(), TaskConfig::default());
        let id = m
            .spawn("t", "u", |_| async { Err("boom".to_string()) })
            .unwrap();
        let info = m.wait(&id).await.unwrap();
        assert_eq!(info.status, TaskStatus::Error);
        assert_eq!(info.error.as_deref(), Some("boom"));
        assert!(full_log(&m, &id).ends_with("TASK ERROR: boom\n"));
        let id = m.spawn("t", "u", |_| async { panic!("bug") }).unwrap();
        assert_eq!(m.wait(&id).await.unwrap().status, TaskStatus::Error);
    }

    #[tokio::test]
    async fn cancel_kills_the_running_command() {
        let dir = tempfile::tempdir().unwrap();
        let m = manager(dir.path(), TaskConfig::default());
        let id = m
            .spawn("t", "u", |ctx| async move {
                ctx.run(&Cmd::new("sleep").arg("30"))
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        m.cancel(&id).unwrap();
        let t = std::time::Instant::now();
        let info = m.wait(&id).await.unwrap();
        assert!(t.elapsed() < Duration::from_secs(5));
        assert_eq!(info.status, TaskStatus::Cancelled);
        assert!(full_log(&m, &id).ends_with("TASK CANCELLED\n"));
        assert_eq!(m.cancel(&id), Err(TaskError::Finished));
        assert_eq!(m.cancel("UPID:nope"), Err(TaskError::NotFound));
    }

    #[tokio::test]
    async fn command_timeout_fails_the_task() {
        let dir = tempfile::tempdir().unwrap();
        let m = manager(dir.path(), TaskConfig::default());
        let id = m
            .spawn("t", "u", |ctx| async move {
                ctx.run(
                    &Cmd::new("sleep")
                        .arg("30")
                        .timeout(Duration::from_millis(200)),
                )
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
            })
            .unwrap();
        let info = m.wait(&id).await.unwrap();
        assert_eq!(info.status, TaskStatus::Error);
        assert!(info.error.unwrap().contains("timed out"));
    }

    #[tokio::test]
    async fn concurrency_limit_queues_and_queue_limit_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = TaskConfig {
            max_concurrent: 1,
            max_queued: 1,
            ..Default::default()
        };
        let m = manager(dir.path(), cfg);
        let gate = CancellationToken::new();
        let g = gate.clone();
        let first = m
            .spawn("t", "u", move |_| async move {
                g.cancelled().await;
                Ok(())
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let second = m.spawn("t", "u", |_| async { Ok(()) }).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(m.get(&first).unwrap().status, TaskStatus::Running);
        assert_eq!(m.get(&second).unwrap().status, TaskStatus::Queued);
        assert_eq!(
            m.spawn("t", "u", |_| async { Ok(()) }).unwrap_err(),
            TaskError::Busy
        );
        gate.cancel();
        assert_eq!(m.wait(&second).await.unwrap().status, TaskStatus::Ok);
        // a queued task can be cancelled before it starts
        let gate = CancellationToken::new();
        let g = gate.clone();
        let blocker = m
            .spawn("t", "u", move |_| async move {
                g.cancelled().await;
                Ok(())
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let queued = m.spawn("t", "u", |_| async { Ok(()) }).unwrap();
        m.cancel(&queued).unwrap();
        assert_eq!(m.wait(&queued).await.unwrap().status, TaskStatus::Cancelled);
        gate.cancel();
        m.wait(&blocker).await;
    }

    #[tokio::test]
    async fn log_cap_and_polling() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = TaskConfig {
            log_cap: 100,
            ..Default::default()
        };
        let m = manager(dir.path(), cfg);
        let id = m
            .spawn("t", "u", |ctx| async move {
                for i in 0..100 {
                    ctx.log(&format!("line {i:03}"));
                }
                Ok(())
            })
            .unwrap();
        m.wait(&id).await;
        let log = full_log(&m, &id);
        assert!(log.starts_with("line 000\nline 001\n"));
        assert!(log.contains("[log truncated: limit of 100 bytes reached]\n"));
        assert!(log.ends_with("TASK OK\n"));
        assert!(log.len() < 200, "{}", log.len());
        // chunked reads: stitched together they give the whole log
        let (mut off, mut all) = (0, String::new());
        loop {
            let c = m.read_log(&id, off, 7).unwrap();
            all.push_str(&c.text);
            off = c.next;
            if c.complete {
                break;
            }
        }
        assert_eq!(all, log);
    }

    #[tokio::test]
    async fn history_survives_restart_and_is_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = TaskConfig {
            keep: 2,
            ..Default::default()
        };
        let m = manager(dir.path(), cfg.clone());
        let mut ids = Vec::new();
        for _ in 0..3 {
            let id = m.spawn("t", "u", |_| async { Ok(()) }).unwrap();
            m.wait(&id).await;
            ids.push(id);
        }
        assert_eq!(m.list(10).len(), 2);
        assert!(m.get(&ids[0]).is_none(), "oldest pruned");
        let logs = std::fs::read_dir(dir.path().join("tasks"))
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "log")
            })
            .count();
        assert_eq!(logs, 2);
        // a task left running by a crash is reported as interrupted
        let gate = CancellationToken::new();
        let g = gate.clone();
        let running = m
            .spawn("t", "u", move |_| async move {
                g.cancelled().await;
                Ok(())
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let m2 = manager(dir.path(), cfg);
        let list = m2.list(10);
        assert_eq!(list[0].id, running);
        assert_eq!(list[0].status, TaskStatus::Error);
        assert!(list[0].error.as_deref().unwrap().contains("interrupted"));
        assert_eq!(m2.get(&ids[2]).unwrap().status, TaskStatus::Ok);
        gate.cancel();
    }

    #[test]
    fn upid_parts_are_sanitized() {
        assert_eq!(sanitize("a b/../c"), "a_b_.._c");
        assert_eq!(sanitize(""), "_");
        assert_eq!(log_name("UPID:n:1:"), "UPID_n_1_.log");
    }
}
