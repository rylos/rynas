//! Live output of a running scheduled task (issue #12):
//! `GET /v1/scheduler/tasks/{id}/live` (public `/api/v1/scheduler/tasks/{id}/live`),
//! server-sent events. Read rights are those of the run history (any signed-in user).
//!
//! The stream follows the invocation of `rynas-task-<id>.service` that runs when it opens
//! (`systemctl show -p InvocationID`), whatever started it (a timer, "Run now", `systemctl`):
//! `journalctl -o json --follow _SYSTEMD_INVOCATION_ID=<id>` from the first entry (the last
//! [`rynas_scheduler::live::BACKLOG_LINES`] of a run that already printed more) until the
//! unit stops or runs another invocation. Events (names in
//! [`rynas_api_types::scheduler::live_event`]):
//!
//! - `run` ([`LiveRun`]): the invocation followed;
//! - `lines`: a JSON array of strings, one journal entry each, batched (64 KiB, 500 entries
//!   or 150 ms, whichever comes first);
//! - `end` ([`LiveEnd`]): the unit stopped (after the last entries journald still had), or
//!   nothing was running (`ran: false`); the stream closes;
//! - `failed` (`{"message"}`): journalctl could not run or stopped early; the stream closes.
//!
//! Comments every [`KEEPALIVE`] keep proxies from closing an idle stream and make a client
//! that went away show up (the write fails, the body is dropped). One task per stream owns
//! the `journalctl` child (`kill_on_drop`): when the browser disconnects, the session ends,
//! or the run ends, the task returns and the child is killed and reaped. Streams per user
//! and in all are capped ([`PER_USER`], [`TOTAL`]); over the cap: 429 `rate_limited`.
//!
//! `--dry-system` replaces systemd and the journal with [`Demo`]: "Run now" starts a scripted
//! run (progress lines with carriage returns, a long line, a byte that is not UTF-8) that is
//! followed like a real one and recorded in the history when it ends.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::{Stream, stream};
use rynas_api_types::scheduler::{LiveEnd, LiveRun, live_event};
use rynas_scheduler::history::NewRun;
use rynas_scheduler::live::{Batch, MAX_ENTRY_JSON, follow_args};
use rynas_scheduler::probe::CurrentRun;
use rynas_scheduler::units::service_name;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::protection::Protection;

/// Live output streams one user may hold at the same time.
pub const PER_USER: usize = 4;
/// Live output streams of all users together (each one a `journalctl` process).
pub const TOTAL: usize = 32;
/// Comment sent on an idle stream.
pub const KEEPALIVE: Duration = Duration::from_secs(5);
/// How often the unit's state is read while following.
const STATE_EVERY: Duration = Duration::from_secs(1);
/// Entries waiting in a batch are sent at least this often.
const FLUSH_EVERY: Duration = Duration::from_millis(150);
/// After the unit stopped: entries are still read until none came for [`QUIET`], at most
/// [`DRAIN`] (journald may still be writing the last lines of the stream).
const QUIET: Duration = Duration::from_millis(400);
const DRAIN: Duration = Duration::from_millis(2000);
/// Before `end`: entries still queued (read from the pipe, not sent yet) are sent while they
/// come at most this far apart …
const DRAIN_GAP: Duration = Duration::from_millis(50);
/// … and at most this many.
const DRAIN_MAX: usize = 20_000;

/// The stream table and, on a dry system, the scripted runs.
pub struct Live {
    per_user: usize,
    total: usize,
    counts: Mutex<(usize, HashMap<String, usize>)>,
    /// `--dry-system`: runs come from here instead of systemd and the journal.
    pub demo: Option<Arc<Demo>>,
}

impl Default for Live {
    fn default() -> Self {
        Self::new(PER_USER, TOTAL)
    }
}

/// A place in the stream table, given back on drop.
pub struct Slot {
    live: Arc<Live>,
    user: String,
}

impl Drop for Slot {
    fn drop(&mut self) {
        // never panic in drop: a poisoned table still counts
        let mut c = self.live.counts.lock().unwrap_or_else(|e| e.into_inner());
        c.0 = c.0.saturating_sub(1);
        if let Some(n) = c.1.get_mut(&self.user) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                c.1.remove(&self.user);
            }
        }
    }
}

impl Live {
    pub fn new(per_user: usize, total: usize) -> Self {
        Self {
            per_user,
            total,
            counts: Mutex::new((0, HashMap::new())),
            demo: None,
        }
    }

    pub fn with_demo(mut self, demo: Arc<Demo>) -> Self {
        self.demo = Some(demo);
        self
    }

    /// A place for one more stream of `user`; `None` over a cap.
    pub fn take(self: &Arc<Self>, user: &str) -> Option<Slot> {
        let mut c = self.counts.lock().unwrap();
        let mine = c.1.get(user).copied().unwrap_or(0);
        if c.0 >= self.total || mine >= self.per_user {
            return None;
        }
        c.0 += 1;
        c.1.insert(user.to_string(), mine + 1);
        Some(Slot {
            live: self.clone(),
            user: user.to_string(),
        })
    }

    /// Streams open now (tests).
    pub fn open_streams(&self) -> usize {
        self.counts.lock().unwrap().0
    }
}

pub async fn current_run(p: Arc<Protection>, unit: String) -> Result<Option<CurrentRun>, String> {
    if let Some(d) = &p.live.demo {
        return Ok(d.current(&unit));
    }
    tokio::task::spawn_blocking(move || p.probe.current_run(&unit))
        .await
        .map_err(|e| e.to_string())?
}

fn json_event(name: &str, v: &impl serde::Serialize) -> Event {
    Event::default()
        .event(name)
        .data(serde_json::to_string(v).unwrap_or_else(|_| "{}".into()))
}

fn failed_event(msg: &str) -> Event {
    json_event(live_event::FAILED, &serde_json::json!({ "message": msg }))
}

/// The response of `GET .../live`: follows the current run of `unit`, or ends at once
/// (`end`, `ran: false`) when nothing runs. `slot` is held as long as the stream lives.
pub async fn stream(
    p: Arc<Protection>,
    unit: String,
    slot: Slot,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = mpsc::channel::<Event>(16);
    match current_run(p.clone(), unit.clone()).await {
        Ok(Some(run)) => {
            tokio::spawn(pump(p, unit, run, tx, slot));
        }
        Ok(None) => {
            let _ = tx.try_send(json_event(live_event::END, &LiveEnd { ran: false }));
            drop(slot);
        }
        Err(e) => {
            tracing::warn!("live output of {unit}: {e}");
            let _ = tx.try_send(failed_event(&e));
            drop(slot);
        }
    }
    let events = stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|e| (Ok(e), rx))
    });
    Sse::new(events).keep_alive(KeepAlive::new().interval(KEEPALIVE))
}

/// Aborts a helper task when dropped.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Where the entries come from, and what must go away with the stream: the `journalctl`
/// child (killed on drop) and the task that reads it.
struct Feed {
    entries: mpsc::Receiver<Vec<u8>>,
    _child: Option<tokio::process::Child>,
    _reader: AbortOnDrop,
}

/// Starts `journalctl` following `invocation_id`.
fn journal_feed(invocation_id: &str) -> Result<Feed, String> {
    let args = follow_args(invocation_id)?;
    let mut child = tokio::process::Command::new("journalctl")
        .args(args)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("journalctl: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "journalctl: no output pipe".to_string())?;
    let (tx, rx) = mpsc::channel(256);
    let reader = tokio::spawn(async move {
        let mut r = BufReader::with_capacity(64 * 1024, stdout);
        let mut buf = Vec::new();
        loop {
            match next_line(&mut r, &mut buf, MAX_ENTRY_JSON).await {
                Ok(Some(false)) => {
                    if let Some(m) = rynas_scheduler::live::entry_message(&buf)
                        && tx.send(m).await.is_err()
                    {
                        return;
                    }
                }
                // an entry over the cap: skipped
                Ok(Some(true)) => {}
                Ok(None) | Err(_) => return,
            }
        }
    });
    Ok(Feed {
        entries: rx,
        _child: Some(child),
        _reader: AbortOnDrop(reader),
    })
}

/// Reads one line (without its newline) into `buf`; `Some(true)` when the line was longer
/// than `max` (then `buf` is empty and the rest of the line is skipped), `None` at the end.
pub async fn next_line<R: AsyncBufRead + Unpin>(
    r: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<Option<bool>> {
    buf.clear();
    let mut over = false;
    let mut any = false;
    loop {
        let avail = r.fill_buf().await?;
        if avail.is_empty() {
            return Ok(any.then_some(over));
        }
        any = true;
        let (take, found) = match avail.iter().position(|&b| b == b'\n') {
            Some(i) => (i, true),
            None => (avail.len(), false),
        };
        if !over {
            if buf.len() + take > max {
                over = true;
                buf.clear();
            } else {
                buf.extend_from_slice(&avail[..take]);
            }
        }
        r.consume(if found { take + 1 } else { take });
        if found {
            return Ok(Some(over));
        }
    }
}

async fn pump(
    p: Arc<Protection>,
    unit: String,
    run: CurrentRun,
    tx: mpsc::Sender<Event>,
    _slot: Slot,
) {
    let head = LiveRun {
        invocation_id: run.invocation_id.clone(),
        start_ms: run.start_ms,
    };
    if tx.send(json_event(live_event::RUN, &head)).await.is_err() {
        return;
    }
    let feed = match &p.live.demo {
        Some(d) => Ok(d.feed(&unit)),
        None => journal_feed(&run.invocation_id),
    };
    let mut feed = match feed {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("live output of {unit}: {e}");
            let _ = tx.send(failed_event(&e)).await;
            return;
        }
    };
    let mut batch = Batch::default();
    let mut flush = tokio::time::interval(FLUSH_EVERY);
    flush.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut state = tokio::time::interval(STATE_EVERY);
    state.set_missed_tick_behavior(MissedTickBehavior::Skip);
    state.tick().await;
    let mut ending: Option<Instant> = None;
    let mut last_entry = Instant::now();
    // the unit's state is read in a task of its own: a slow systemd never holds up the output
    let mut checking: Option<JoinHandle<Result<Option<CurrentRun>, String>>> = None;
    let same = |r: &Result<Option<CurrentRun>, String>| match r {
        Ok(Some(r)) => Some(r.invocation_id == run.invocation_id),
        Ok(None) => Some(false),
        // no answer: unknown, asked again at the next tick
        Err(_) => None,
    };
    loop {
        tokio::select! {
            // the client went away (browser closed, session ended, evicted): the feed and its
            // child go with this task
            _ = tx.closed() => return,
            m = feed.entries.recv() => match m {
                Some(entry) => {
                    last_entry = Instant::now();
                    if batch.push(entry) && tx.send(lines_event(&mut batch)).await.is_err() {
                        return;
                    }
                }
                None => {
                    if !batch.is_empty() && tx.send(lines_event(&mut batch)).await.is_err() {
                        return;
                    }
                    // the source ended: normal once the unit stopped, early otherwise
                    let stopped = ending.is_some()
                        || same(&current_run(p.clone(), unit.clone()).await) == Some(false);
                    let last = if stopped {
                        json_event(live_event::END, &LiveEnd { ran: true })
                    } else {
                        tracing::warn!("live output of {unit}: journalctl stopped early");
                        failed_event("journalctl stopped")
                    };
                    let _ = tx.send(last).await;
                    return;
                }
            },
            _ = flush.tick() => {
                if !batch.is_empty() && tx.send(lines_event(&mut batch)).await.is_err() {
                    return;
                }
                if let Some(at) = ending
                    && (at.elapsed() >= DRAIN || (at.elapsed() >= QUIET && last_entry.elapsed() >= QUIET))
                {
                    if drain(&mut feed, &mut batch, &tx).await.is_err() {
                        return;
                    }
                    let _ = tx.send(json_event(live_event::END, &LiveEnd { ran: true })).await;
                    return;
                }
            }
            r = async { checking.as_mut().expect("guarded").await }, if checking.is_some() => {
                checking = None;
                let r = r.unwrap_or_else(|e| Err(e.to_string()));
                match same(&r) {
                    Some(true) => {}
                    Some(false) => {
                        // journald writes what it still has; not awaited (the drain below
                        // waits for the last entries anyway)
                        if p.live.demo.is_none() {
                            let p = p.clone();
                            drop(tokio::task::spawn_blocking(move || p.probe.sync_journal()));
                        }
                        ending = Some(Instant::now());
                    }
                    None => tracing::debug!("live output of {unit}: state not read: {:?}", r.err()),
                }
            }
            _ = state.tick(), if ending.is_none() && checking.is_none() => {
                checking = Some(tokio::spawn(current_run(p.clone(), unit.clone())));
            }
        }
    }
}

fn lines_event(b: &mut Batch) -> Event {
    Event::default().event(live_event::LINES).data(b.take())
}

/// The last entries already read or arriving within [`DRAIN_GAP`] of each other (at most
/// [`DRAIN_MAX`]), sent before `end`. `Err`: the client went away.
async fn drain(feed: &mut Feed, batch: &mut Batch, tx: &mpsc::Sender<Event>) -> Result<(), ()> {
    for _ in 0..DRAIN_MAX {
        match tokio::time::timeout(DRAIN_GAP, feed.entries.recv()).await {
            Ok(Some(entry)) => {
                if batch.push(entry) {
                    tx.send(lines_event(batch)).await.map_err(|_| ())?;
                }
            }
            _ => break,
        }
    }
    if !batch.is_empty() {
        tx.send(lines_event(batch)).await.map_err(|_| ())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// --dry-system
// ---------------------------------------------------------------------------------------------

/// Scripted runs of a dry system: a run starts with "Run now", prints its script at the
/// given offsets (ms from the start) and is recorded in the history when the script ends.
pub struct Demo {
    script: Vec<(u64, Vec<u8>)>,
    runs: Mutex<HashMap<String, DemoRun>>,
}

#[derive(Clone)]
struct DemoRun {
    invocation_id: String,
    start: Instant,
    start_ms: u64,
}

impl Default for Demo {
    fn default() -> Self {
        Self::with_script(demo_script())
    }
}

/// The default script: an rsync copy of about 45 s with `--progress` output (carriage
/// returns inside each entry, as journald stores them), a long path, a name with a byte
/// that is not UTF-8, and rsync's summary.
pub fn demo_script() -> Vec<(u64, Vec<u8>)> {
    let mut s: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut at = 0;
    let mut push = |dt: u64, line: Vec<u8>| {
        at += dt;
        s.push((at, line));
    };
    push(200, b"rsync -a --progress /srv/rynas/volume1/photos/ /srv/rynas/usb1/photos-copy/".to_vec());
    push(400, b"sending incremental file list".to_vec());
    for i in 1..=40u64 {
        let size = 1_048_576 * (3 + i % 7);
        push(250, format!("2026/11/IMG_{:04}.jpg", 1000 + i).into_bytes());
        let mut progress = Vec::new();
        for pct in [25u64, 50, 75, 100] {
            if pct > 25 {
                progress.push(b'\r');
            }
            let done = size * pct / 100;
            progress.extend_from_slice(format!("{done:>14}  {pct:>3}%   41.20MB/s    0:00:0{}", 3 - pct / 34).as_bytes());
        }
        progress.extend_from_slice(format!(" (xfr#{i}, to-chk={}/52)", 52 - i).as_bytes());
        push(700, progress);
        if i == 12 {
            push(150, b"2026/11/an-unusually-long-folder-name-from-a-phone-backup/another-level-with-a-long-name/IMG_20261103_174512_HDR_portrait_mode_night_sight_edited_final_v2_copy.jpg".to_vec());
        }
        if i == 25 {
            push(150, b"2026/11/caf\xe9 menu (Latin-1 name).png".to_vec());
        }
    }
    push(300, b"".to_vec());
    push(200, b"sent 251,723,331 bytes  received 1,062 bytes  5,593,875.40 bytes/sec".to_vec());
    push(100, b"total size is 251,658,240  speedup is 1.00".to_vec());
    s
}

impl Demo {
    pub fn with_script(script: Vec<(u64, Vec<u8>)>) -> Self {
        Self {
            script,
            runs: Mutex::new(HashMap::new()),
        }
    }

    /// How long a run lasts: its last line, then half a second.
    fn duration(&self) -> Duration {
        Duration::from_millis(self.script.last().map_or(0, |(at, _)| *at) + 500)
    }

    /// Units running now.
    pub fn running(&self) -> Vec<String> {
        self.runs.lock().unwrap().keys().cloned().collect()
    }

    fn current(&self, unit: &str) -> Option<CurrentRun> {
        self.runs.lock().unwrap().get(unit).map(|r| CurrentRun {
            invocation_id: r.invocation_id.clone(),
            start_ms: Some(r.start_ms),
        })
    }

    /// "Run now" of task `id`: a scripted run, unless one runs already (systemd does not
    /// start a running unit again either). Recorded in the history when it ends.
    pub fn start(self: &Arc<Self>, p: Arc<Protection>, id: &str) {
        let unit = service_name(id);
        let run = {
            let mut runs = self.runs.lock().unwrap();
            if runs.contains_key(&unit) {
                return;
            }
            let mut b = [0u8; 16];
            let _ = getrandom::fill(&mut b);
            let run = DemoRun {
                invocation_id: b.iter().map(|x| format!("{x:02x}")).collect(),
                start: Instant::now(),
                start_ms: rynas_tasks::now_ms(),
            };
            runs.insert(unit.clone(), run.clone());
            run
        };
        let (me, id, wait) = (self.clone(), id.to_string(), self.duration());
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            me.runs.lock().unwrap().remove(&unit);
            let text: Vec<String> = me
                .script
                .iter()
                .map(|(_, l)| String::from_utf8_lossy(l).into_owned())
                .collect();
            let p2 = p.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let new = NewRun {
                    task_id: id.clone(),
                    invocation_id: run.invocation_id,
                    start_ms: Some(run.start_ms),
                    end_ms: rynas_tasks::now_ms(),
                    result: "success".into(),
                    exit_code: Some(0),
                    signal: None,
                };
                match p2.history.record(&new) {
                    Ok(n) => {
                        let mut out = text.join("\n");
                        out.push('\n');
                        if let Err(e) = p2.history.set_output(n, &out, false) {
                            tracing::warn!("demo run of {id}: {e}");
                        }
                    }
                    Err(e) => tracing::warn!("demo run of {id}: {e}"),
                }
            })
            .await;
            p.invalidate();
        });
    }

    /// The script of the run of `unit`: what is already due at once, the rest when due.
    fn feed(self: &Arc<Self>, unit: &str) -> Feed {
        let (tx, rx) = mpsc::channel(256);
        let start = self.runs.lock().unwrap().get(unit).map(|r| r.start);
        let me = self.clone();
        let reader = tokio::spawn(async move {
            let Some(start) = start else { return };
            for (at, line) in &me.script {
                let due = start + Duration::from_millis(*at);
                tokio::time::sleep_until(due.into()).await;
                if tx.send(line.clone()).await.is_err() {
                    return;
                }
            }
            // like journalctl --follow: no end of its own, the unit's state ends the stream
            // (this task is aborted with the feed)
            std::future::pending::<()>().await;
        });
        Feed {
            entries: rx,
            _child: None,
            _reader: AbortOnDrop(reader),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_lines_with_a_cap() {
        let mut r: &[u8] = b"one\ntwo-is-too-long\nthree\nlast";
        let mut buf = Vec::new();
        assert_eq!(next_line(&mut r, &mut buf, 8).await.unwrap(), Some(false));
        assert_eq!(buf, b"one");
        assert_eq!(next_line(&mut r, &mut buf, 8).await.unwrap(), Some(true));
        assert!(buf.is_empty());
        assert_eq!(next_line(&mut r, &mut buf, 8).await.unwrap(), Some(false));
        assert_eq!(buf, b"three");
        assert_eq!(next_line(&mut r, &mut buf, 8).await.unwrap(), Some(false));
        assert_eq!(buf, b"last");
        assert_eq!(next_line(&mut r, &mut buf, 8).await.unwrap(), None);
    }

    #[test]
    fn caps_streams_per_user_and_in_all() {
        let live = Arc::new(Live::new(2, 3));
        let a1 = live.take("alice").unwrap();
        let _a2 = live.take("alice").unwrap();
        assert!(live.take("alice").is_none());
        let _b1 = live.take("bob").unwrap();
        assert!(live.take("carol").is_none());
        drop(a1);
        assert_eq!(live.open_streams(), 2);
        assert!(live.take("carol").is_some());
    }

    #[test]
    fn demo_script_has_progress_and_odd_bytes() {
        let s = demo_script();
        assert!(s.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(s.iter().any(|(_, l)| l.contains(&b'\r')));
        assert!(s.iter().any(|(_, l)| std::str::from_utf8(l).is_err()));
        assert!(s.iter().any(|(_, l)| l.len() > 120));
    }
}
