//! rynasd's half of the scheduling gate ([`rynas_scheduler::gate`]): the answer to
//! `rynasctl gate <id>` (`ExecCondition=` of every task unit), the pauses, the leases of the
//! starts it let through, and a watch loop for retries, the overlap policy `replace` and
//! pauses that end by themselves.
//!
//! - **Pauses** live in `<state>/scheduler/pause.json` (also read by rynasctl when rynasd
//!   cannot be reached), changed at once (not staged: a pause is an action, like "run now").
//! - **Leases**: one per task, granted with a start (its `$INVOCATION_ID`), released by
//!   `task-finished`. The lease carries what the run holds in rynasd: the rynasd task
//!   started by `rynasctl job run` (its UPID), and the filesystem lock of a run that works in
//!   its own unit (a scrub: deduplication gives way, [`crate::fslock`]). A second start of
//!   the task while that rynasd task still runs is skipped (`running`). After a restart of
//!   rynasd the leases are rebuilt from the task units systemd says are running
//!   ([`Gate::rebuild`]), so nothing is persisted that could go stale.
//! - **Requests**: "run now" and the retries tell the gate what is about to start, so the
//!   start is recorded as such and is not held back by a pause or a deadline (a retry is).
//!   A request never applies to a start of the task's own timer, and is used once.
//! - **Retry chains** (`retries` in `scheduler.db`): a failed scheduled run arms the next
//!   attempt and holds its failure notification back; a newer scheduled failure keeps the
//!   chain's count. The chain ends with a success (nothing is sent), with its last attempt
//!   (that run notifies), or dropped (task deleted or off, a pause at the due time, its start
//!   skipped or lost): then the held notification goes, once ([`drop_retry`]).
//! - **Interval tasks** (`TaskTrigger::Interval`, S3): their timer starts them after boot and
//!   an interval after each run; the gate skips a start whose interval (counted from the
//!   history, so across restarts and reboots) has not passed, without a row, and the watch
//!   loop starts the task once it is due ([`due_intervals`]), never twice within
//!   [`INTERVAL_RESTART`].
//! - **Late answers**: an answer decided after rynasctl's deadline
//!   ([`GateRequest::answer_by_ms`]) says "run" and records no skip, since rynasctl may have
//!   run the start already.
//! - **Queue** (S4, [`rynas_scheduler::queue`]): a start the checks let through whose claims
//!   (from its action: a volume, a disk, the package system) meet what other work holds in
//!   the lock table ([`crate::claims`]), or what a waiter ahead of it needs, is answered
//!   `wait`: rynasctl asks again every 5 s (the start keeps its first answer's origin), until
//!   its turn comes or it has waited past its deadline (`busy`, recorded). Run now confirmed
//!   by the user goes ahead of it. The lease then carries the claims.
//! - **Yield** (S4): a scheduled scrub holding a volume that more urgent work holds or waits
//!   for is cancelled (SIGINT to its unit: `btrfs scrub start -B` cancels and keeps the
//!   position), after an entry in
//!   [`YIELD_FILE`] that tells the unit its end is not a failure; once nothing more urgent
//!   holds or waits for the volume the watch loop starts it again (`btrfs scrub resume`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rynas_api_types::TaskStatus as UpidStatus;
use rynas_api_types::scheduler::{
    Blocker, PauseInfo, QueueWait, Resource, RunTrigger, ScheduledTask, SchedulerPause, SkipReason,
    TaskAction, TaskTrigger,
};
use rynas_notify::event::{code, field, oneshot};
use rynas_notify::{NewEvent, NotifyCategory, NotifySeverity};
use rynas_scheduler::gate::{
    Decision, GateAnswer, GateRequest, Start, WAITER_STALE, YIELD_FILE, YieldEntry, decide,
    Parked, load_parked, load_pause, load_yields, next_retry, save_parked, queue_deadline_ms, replace_due, retries_after, save_pause,
    save_yields, spool_take, start_timeout_lets_wait,
};
use rynas_scheduler::history::{ArmedRetry, GateNote, NewRun};
use rynas_scheduler::maintenance::Defer;
use rynas_scheduler::queue::{Block, Claim, Gives, Holder, Waiter, blocked_by, priority_of, yield_to};
use rynas_scheduler::units::{UNIT_PREFIX, runs_in_unit, service_name, timer_of};
use rynas_tasks::{Cmd, now_ms};

use crate::fslock::HeavyGuard;
use crate::ops::{Error, Res};
use crate::protection::Protection;

/// How long a start rynasd asked for waits for its gate.
const REQUEST_TTL: Duration = Duration::from_secs(120);
/// Period of the watch loop.
const WATCH: Duration = Duration::from_secs(5);

struct Lease {
    invocation_id: String,
    /// The rynasd task the run started (`rynasctl job run`).
    upid: Option<String>,
    /// The filesystem lock of a run working in its own unit.
    _guard: Option<HeavyGuard>,
    /// What the run holds in the lock table (S4: its claims, priority, how it gives way).
    holder: Holder,
    /// The rynas task type of its work in rynasd (its filesystem lock is the lease's).
    kind: Option<&'static str>,
    /// The unit it runs in.
    unit: String,
    /// When it was granted.
    since: Instant,
    /// The task panel's entry of a run working in its own unit ([`crate::jobs::track`]):
    /// told systemd's result when the run ends.
    tracker: Option<tokio::sync::oneshot::Sender<String>>,
}

impl Lease {
    fn new(id: &str, invocation_id: &str, holder: Holder, kind: Option<&'static str>) -> Self {
        Lease {
            invocation_id: invocation_id.to_string(),
            upid: None,
            _guard: None,
            holder,
            kind,
            unit: rynas_scheduler::units::service_of(id),
            since: Instant::now(),
            tracker: None,
        }
    }
}

/// A lease whose unit has not been running for this long is gone (its end was never
/// reported: the lock table must not keep it).
const LEASE_LOST: Duration = Duration::from_secs(60);

/// A lease as the lock table sees it ([`crate::claims`]).
#[derive(Debug, Clone)]
pub struct LeaseView {
    pub holder: Holder,
    pub invocation_id: String,
    /// The id of its filesystem lock.
    pub guard: Option<u64>,
    pub kind: Option<&'static str>,
    pub unit: String,
    /// How long ago it was granted.
    pub age: Duration,
}

/// A start waiting in the queue (S4): what its first answer decided, and when rynasctl last
/// asked.
struct WaitEntry {
    waiter: Waiter,
    invocation_id: String,
    seen: Instant,
    blocker: Option<Blocker>,
    /// It waits for the maintenance window, which opens then.
    opens_ms: Option<u64>,
    trigger: RunTrigger,
    attempt: u8,
    /// Run now (confirmed by the user) came while it waited: it goes at its next question.
    skip: bool,
}

/// A SMART long test that goes on in the drive after its unit ended (`smartctl -t long`
/// returns at once): its disk stays claimed until the drive says it is done, or its
/// estimated time has passed.
#[derive(Debug, Clone)]
pub struct DiskTest {
    pub task: String,
    pub name: String,
    pub disk: String,
    pub until_ms: u64,
    checked: Instant,
}

/// How often the watch loop asks a drive whether its long test still runs.
const DISK_TEST_CHECK: Duration = Duration::from_secs(300);
/// The test time assumed when the drive does not say (minutes).
const DISK_TEST_DEFAULT_MIN: u64 = 120;

/// Asks the drive about its self-test (`smartctl -c -l selftest`).
fn self_test(p: &Protection, disk: &str) -> crate::claims::SelfTest {
    let dev = format!("/dev/{disk}");
    match p.ops.config.system().run(&Cmd::new("smartctl").args(["-c", "-l", "selftest", &dev]).env_clear()) {
        // smartctl's exit code is a bit mask of disk states: read the output whatever it is
        Ok(o) => crate::claims::parse_self_test(&o.stdout),
        Err(e) => {
            tracing::debug!("smartctl -c {dev}: {e}");
            crate::claims::SelfTest::default()
        }
    }
}

/// After the unit of a SMART long test of `disk` ended: keep its disk claimed while the drive
/// runs the test (unless the drive says it is not running).
pub fn hold_disk_test(p: &Protection, task: &ScheduledTask, disk: &str) {
    let st = self_test(p, disk);
    if st.in_progress == Some(false) {
        return;
    }
    let min = st.extended_min.unwrap_or(DISK_TEST_DEFAULT_MIN);
    let until_ms = now_ms() + min * 60_000 + 10 * 60_000;
    tracing::info!("scheduler: task {}: the long SMART test of {disk} runs in the drive: disk {disk} claimed up to {min} min", task.id);
    p.gate.disk_tests.lock().unwrap().insert(
        disk.to_string(),
        DiskTest { task: task.id.clone(), name: task.name.clone(), disk: disk.to_string(), until_ms, checked: Instant::now() },
    );
    p.invalidate();
}

/// The watch loop's turn for the drives' long tests: released when the drive says it is done
/// or its time has passed.
fn disk_tests_tick(p: &Protection, now: u64) {
    let due: Vec<DiskTest> = p
        .gate
        .disk_tests
        .lock()
        .unwrap()
        .values()
        .filter(|d| d.until_ms <= now || d.checked.elapsed() >= DISK_TEST_CHECK)
        .cloned()
        .collect();
    for d in due {
        let done = d.until_ms <= now || self_test(p, &d.disk).in_progress == Some(false);
        let mut m = p.gate.disk_tests.lock().unwrap();
        if done {
            m.remove(&d.disk);
            tracing::info!("scheduler: the long SMART test of {} ended: disk free", d.disk);
        } else if let Some(x) = m.get_mut(&d.disk) {
            x.checked = Instant::now();
        }
    }
}

/// What a task's run holds: its claims, priority and how it gives way.
pub fn holder_of(p: &Protection, t: &ScheduledTask) -> Holder {
    Holder {
        task: Some(t.id.clone()),
        name: t.name.clone(),
        kind: None,
        claims: crate::claims::claims_of(p, t),
        priority: priority_of(&t.action),
        gives: crate::claims::gives_of(&t.action),
    }
}

pub struct Gate {
    state: PathBuf,
    pause: Mutex<SchedulerPause>,
    /// trigger, attempt, asked at (for the TTL), asked at in ms since the epoch, skip the
    /// queue (run now confirmed by the user)
    requested: Mutex<HashMap<String, Request>>,
    leases: Mutex<HashMap<String, Lease>>,
    /// The starts waiting in the queue (S4), by task.
    waiting: Mutex<HashMap<String, WaitEntry>>,
    /// The scrubs that gave way (S4, kept in [`rynas_scheduler::gate::YIELD_FILE`]).
    yields: Mutex<BTreeMap<String, YieldEntry>>,
    /// When a scrub's cancel last failed (tried again a minute later).
    yield_failed: Mutex<HashMap<String, Instant>>,
    /// When the SIGINT of a scrub asked to give way was last sent (sent again until its run
    /// ends: a signal that came before the command started, or a restart of rynasd between
    /// the entry and the signal, loses nothing).
    yield_sent: Mutex<HashMap<String, Instant>>,
    /// Disks whose SMART long test still runs in the drive after its unit ended (S4): task,
    /// name, disk, until when at the latest, when last checked.
    disk_tests: Mutex<HashMap<String, DiskTest>>,
    /// The applied task ids at the last watch tick (a task gone since was deleted).
    known: Mutex<Option<HashSet<String>>>,
    /// When the watch loop last started each interval task that was due (it never starts
    /// one twice within [`INTERVAL_RESTART`], whatever the history says).
    interval_started: Mutex<HashMap<String, IntervalStart>>,
    /// The waiting starts kept on disk ([`rynas_scheduler::gate::PARKED_FILE`]): a reboot
    /// that stops one is recorded and, when it may still run, started again.
    parked: Mutex<BTreeMap<String, Parked>>,
    /// This boot's id.
    boot: String,
}

/// The watch loop's last start of an interval task: when, the last run's end it saw then, how
/// many starts in a row left that end where it was.
type IntervalStart = (Instant, Option<u64>, u32);

/// The history of a task could not be read.
#[derive(Debug)]
pub struct HistoryUnreadable;

/// The watch loop starts a due interval task at most this often.
const INTERVAL_RESTART: Duration = Duration::from_secs(600);

impl Gate {
    /// `state`: rynasd's state directory (the pauses are below it).
    pub fn new(state: PathBuf) -> Self {
        let pause = load_pause(&state);
        let yields = load_yields(&state);
        let parked = load_parked(&state);
        Self {
            parked: Mutex::new(parked),
            boot: rynas_scheduler::gate::boot_id(),
            state,
            pause: Mutex::new(pause),
            requested: Mutex::new(HashMap::new()),
            leases: Mutex::new(HashMap::new()),
            waiting: Mutex::new(HashMap::new()),
            yields: Mutex::new(yields),
            yield_failed: Mutex::new(HashMap::new()),
            yield_sent: Mutex::new(HashMap::new()),
            disk_tests: Mutex::new(HashMap::new()),
            known: Mutex::new(None),
            interval_started: Mutex::new(HashMap::new()),
        }
    }

    /// The pauses in force (ended ones dropped, and the file rewritten then).
    pub fn pause(&self) -> SchedulerPause {
        let mut p = self.pause.lock().unwrap();
        if p.expire(now_ms())
            && let Err(e) = save_pause(&self.state, &p)
        {
            tracing::warn!("scheduler pause: {e}");
        }
        p.clone()
    }

    fn change_pause(&self, f: impl FnOnce(&mut SchedulerPause)) -> Res<SchedulerPause> {
        let mut p = self.pause.lock().unwrap();
        let mut next = p.clone();
        f(&mut next);
        next.expire(now_ms());
        save_pause(&self.state, &next).map_err(Error::internal)?;
        *p = next.clone();
        Ok(next)
    }

    /// Pauses every task (`Some`) or resumes them (`None`).
    pub fn set_pause_all(&self, info: Option<PauseInfo>) -> Res<SchedulerPause> {
        self.change_pause(|p| p.all = info)
    }

    /// Pauses (`Some`) or resumes (`None`) one task.
    pub fn set_pause_task(&self, id: &str, info: Option<PauseInfo>) -> Res<SchedulerPause> {
        self.change_pause(|p| match info {
            Some(i) => {
                p.tasks.insert(id.to_string(), i);
            }
            None => {
                p.tasks.remove(id);
            }
        })
    }

    /// The next start of `id` is one rynasd asked for.
    pub fn request(&self, id: &str, trigger: RunTrigger, attempt: u8) {
        self.request_with(id, trigger, attempt, false);
    }

    /// [`Gate::request`]; `skip_queue`: the start does not wait in the queue (run now, the
    /// user confirmed what it slows down).
    pub fn request_with(&self, id: &str, trigger: RunTrigger, attempt: u8, skip_queue: bool) {
        self.requested.lock().unwrap().insert(
            id.to_string(),
            Request { trigger, attempt, at: Instant::now(), at_ms: now_ms(), skip_queue },
        );
    }

    /// Takes the request of `id` (used once, whoever started the unit).
    fn take_request(&self, id: &str) -> Option<Request> {
        let mut r = self.requested.lock().unwrap();
        r.retain(|_, q| q.at.elapsed() < REQUEST_TTL);
        r.remove(id)
    }

    /// A start of `id` that rynasd asked for is still waiting for its gate.
    fn requested_now(&self, id: &str) -> bool {
        self.requested
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|q| q.at.elapsed() < REQUEST_TTL)
    }

    /// The start `invocation_id` of `id` waits in the queue already: how its first answer
    /// said it came about.
    fn waiting_as(&self, id: &str, invocation_id: &str) -> Option<(RunTrigger, u8, bool)> {
        self.waiting
            .lock()
            .unwrap()
            .get(id)
            .filter(|w| w.invocation_id == invocation_id && w.seen.elapsed() < WAITER_STALE)
            .map(|w| (w.trigger, w.attempt, w.skip))
    }

    /// Run now on a task whose start waits in the queue: `true` when it waits (then, with
    /// `skip_queue`, it goes at its next question; without, it keeps its place).
    pub fn let_through(&self, id: &str, skip_queue: bool) -> bool {
        let mut m = self.waiting.lock().unwrap();
        match m.get_mut(id).filter(|w| w.seen.elapsed() < WAITER_STALE) {
            Some(w) => {
                w.skip |= skip_queue;
                true
            }
            None => false,
        }
    }

    /// The start of `id` waits (again) for `blocker`, or for the maintenance window that opens
    /// at `opens_ms`; `true` when that changed what is shown.
    #[allow(clippy::too_many_arguments)]
    fn wait(
        &self,
        id: &str,
        invocation_id: &str,
        waiter: Waiter,
        blocker: Option<Blocker>,
        opens_ms: Option<u64>,
        trigger: RunTrigger,
        attempt: u8,
        deadline_ms: u64,
    ) -> bool {
        let since_ms = waiter.since_ms;
        let changed = {
            let mut m = self.waiting.lock().unwrap();
            let changed = m
                .get(id)
                .is_none_or(|w| w.invocation_id != invocation_id || w.blocker != blocker || w.opens_ms != opens_ms);
            m.insert(
                id.to_string(),
                WaitEntry { waiter, invocation_id: invocation_id.to_string(), seen: Instant::now(), blocker, opens_ms, trigger, attempt, skip: false },
            );
            changed
        };
        // kept on disk once per start (and when what it waits for changes kind)
        let want = Parked {
            invocation_id: invocation_id.to_string(),
            since_ms,
            deadline_ms,
            trigger,
            attempt,
            boot_id: self.boot.clone(),
            window: opens_ms.is_some(),
        };
        if !invocation_id.is_empty() {
            self.change_parked(|p| match p.get(id) {
                Some(cur) if *cur == want => false,
                _ => {
                    p.insert(id.to_string(), want);
                    true
                }
            });
        }
        changed
    }

    /// Changes the waiting starts kept on disk (`f` says whether it changed anything).
    fn change_parked(&self, f: impl FnOnce(&mut BTreeMap<String, Parked>) -> bool) {
        let mut m = self.parked.lock().unwrap();
        if f(&mut m)
            && let Err(e) = save_parked(&self.state, &m)
        {
            tracing::warn!("scheduler: {}: {e}", rynas_scheduler::gate::PARKED_FILE);
        }
    }

    /// The start `invocation_id` of `id` was waiting in the queue when its unit ended (stopped
    /// while queued: nothing ran); its place goes. `true` when it was. `keep`: the NAS is
    /// shutting down, the start stays on disk (recorded and started again after the boot).
    pub fn ended_waiting(&self, id: &str, invocation_id: &str, keep: bool) -> bool {
        let was = {
            let mut m = self.waiting.lock().unwrap();
            if m.get(id).is_some_and(|w| w.invocation_id == invocation_id) {
                m.remove(id);
                true
            } else {
                false
            }
        };
        let parked = self.parked.lock().unwrap().get(id).is_some_and(|p| p.invocation_id == invocation_id);
        if parked && !keep {
            self.change_parked(|p| p.remove(id).is_some());
        }
        was || parked
    }

    /// The waiting starts kept on disk.
    pub fn parked(&self) -> BTreeMap<String, Parked> {
        self.parked.lock().unwrap().clone()
    }

    /// Forgets a waiting start kept on disk.
    pub fn unpark(&self, id: &str) {
        self.change_parked(|p| p.remove(id).is_some());
    }

    /// This boot's id.
    pub fn boot(&self) -> &str {
        &self.boot
    }

    /// A start of `id` waits in the queue now.
    pub fn is_waiting(&self, id: &str) -> bool {
        self.waiting.lock().unwrap().get(id).is_some_and(|w| w.seen.elapsed() < WAITER_STALE)
    }

    /// The start of `id` no longer waits; returns since when it had waited.
    fn unwait(&self, id: &str) -> Option<u64> {
        let w = self.waiting.lock().unwrap().remove(id).map(|w| w.waiter.since_ms);
        self.change_parked(|p| p.remove(id).is_some());
        w
    }

    /// The fresh waiters but `except` (whose rynasctl asked within [`WAITER_STALE`]).
    pub fn waiters(&self, except: &str) -> Vec<Waiter> {
        self.waiting
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, w)| id.as_str() != except && w.seen.elapsed() < WAITER_STALE)
            .map(|(_, w)| w.waiter.clone())
            .collect()
    }

    /// Drops the waiters gone silent (their unit was stopped); `true` when one went.
    fn expire_waiters(&self) -> bool {
        let mut m = self.waiting.lock().unwrap();
        let n = m.len();
        m.retain(|_, w| w.seen.elapsed() < WAITER_STALE);
        m.len() != n
    }

    /// The starts waiting in the queue: task, since, what they wait for.
    pub fn queued(&self) -> Vec<(String, QueueWait)> {
        self.waiting
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, w)| w.seen.elapsed() < WAITER_STALE)
            .map(|(id, w)| {
                (id.clone(), QueueWait { since_ms: w.waiter.since_ms, blocker: w.blocker.clone(), window_opens_ms: w.opens_ms })
            })
            .collect()
    }

    /// The scrubs that gave way.
    pub fn yields(&self) -> BTreeMap<String, YieldEntry> {
        self.yields.lock().unwrap().clone()
    }

    fn change_yields(&self, f: impl FnOnce(&mut BTreeMap<String, YieldEntry>)) -> std::io::Result<()> {
        let mut m = self.yields.lock().unwrap();
        let mut next = m.clone();
        f(&mut next);
        // kept in memory only once on disk: the unit reads the file to tell a scrub that gave
        // way from one that failed
        if let Err(e) = save_yields(&self.state, &next) {
            tracing::warn!("scheduler: {}: {e}", YIELD_FILE);
            return Err(e);
        }
        *m = next;
        Ok(())
    }

    /// The run `invocation_id` of `id` ended: `true` when it is the run that gave way (it
    /// waits to be resumed now). Any other run of the task ends the scrub's yield (it
    /// resumed, or a run since went to the end).
    pub fn run_ended(&self, id: &str, invocation_id: &str) -> bool {
        let mut gave_way = false;
        let known = self.yields.lock().unwrap().contains_key(id);
        if !known {
            return false;
        }
        let _ = self.change_yields(|m| match m.get_mut(id) {
            Some(e) if e.invocation_id == invocation_id => {
                e.stopped = true;
                gave_way = true;
            }
            Some(e) if e.stopped => {
                m.remove(id);
            }
            _ => {}
        });
        gave_way
    }

    /// Forgets the yield of `id` (the task is gone or off, or its run resumes it now).
    pub fn drop_yield(&self, id: &str) {
        if self.yields.lock().unwrap().contains_key(id) {
            let _ = self.change_yields(|m| {
                m.remove(id);
            });
            self.yield_sent.lock().unwrap().remove(id);
        }
    }

    /// The run of `id` that is asked to give way, if it is `invocation_id` (it has not stopped
    /// yet): such a run that has not begun its work gives way at once ([`crate::jobs::plan`]).
    pub fn yield_pending(&self, id: &str, invocation_id: &str) -> bool {
        !invocation_id.is_empty()
            && self
                .yields
                .lock()
                .unwrap()
                .get(id)
                .is_some_and(|e| e.invocation_id == invocation_id && !e.stopped)
    }

    /// The long SMART tests still running in their drive.
    pub fn disk_tests(&self) -> Vec<DiskTest> {
        let mut v: Vec<DiskTest> = self.disk_tests.lock().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.disk.cmp(&b.disk));
        v
    }

    /// The invocation of the current run of `id` (its lease), empty without one.
    pub fn lease_invocation(&self, id: &str) -> String {
        self.leases.lock().unwrap().get(id).map(|l| l.invocation_id.clone()).unwrap_or_default()
    }

    /// The watch loop may start the interval task `id` now; records the start when it may. A
    /// start that left the task's last run where it was (the gate skipped it, the start was
    /// lost) is tried again after [`INTERVAL_RESTART`], then twice as long each time, at most
    /// the task's interval: never a start every few minutes.
    fn interval_may_start(&self, id: &str, base: Option<u64>, t: &ScheduledTask) -> bool {
        let mut m = self.interval_started.lock().unwrap();
        let n = match m.get(id) {
            Some((at, b, n)) if *b == base => {
                let hours = match t.trigger {
                    TaskTrigger::Interval { hours, .. } => u64::from(hours.max(1)),
                    _ => 24,
                };
                let wait = INTERVAL_RESTART.saturating_mul(1u32 << (*n).min(10)).min(Duration::from_secs(hours * 3600));
                if at.elapsed() < wait {
                    return false;
                }
                n + 1
            }
            _ => 0,
        };
        m.insert(id.to_string(), (Instant::now(), base, n));
        true
    }

    fn grant(&self, id: &str, invocation_id: &str, holder: Holder, kind: Option<&'static str>) {
        self.leases
            .lock()
            .unwrap()
            .insert(id.to_string(), Lease::new(id, invocation_id, holder, kind));
    }

    /// Drops the leases whose unit no longer runs (its end report never came: rynasctl and
    /// the spool both lost it), granted more than [`LEASE_LOST`] ago. Their lock and task
    /// panel entry go with them.
    fn sweep(&self, running: &[String]) {
        let gone: Vec<(String, Lease)> = {
            let mut l = self.leases.lock().unwrap();
            let ids: Vec<String> = l
                .iter()
                .filter(|(_, x)| x.since.elapsed() > LEASE_LOST && !running.contains(&x.unit))
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter().filter_map(|id| l.remove(&id).map(|x| (id, x))).collect()
        };
        for (id, mut lease) in gone {
            tracing::warn!("scheduler: task {id}: its unit no longer runs and its end never came: lease dropped");
            if let Some(t) = lease.tracker.take() {
                let _ = t.send("unknown".into());
            }
        }
    }

    fn has_leases(&self) -> bool {
        !self.leases.lock().unwrap().is_empty()
    }

    /// The leases as the lock table sees them.
    pub fn lease_views(&self) -> Vec<LeaseView> {
        let mut v: Vec<LeaseView> = self
            .leases
            .lock()
            .unwrap()
            .values()
            .map(|l| LeaseView {
                holder: l.holder.clone(),
                invocation_id: l.invocation_id.clone(),
                guard: l._guard.as_ref().map(HeavyGuard::id),
                kind: l.kind,
                unit: l.unit.clone(),
                age: l.since.elapsed(),
            })
            .collect();
        v.sort_by(|a, b| a.holder.task.cmp(&b.holder.task));
        v
    }

    /// The run of `id` started the rynasd task `upid`.
    pub fn attach_upid(&self, id: &str, upid: &str) {
        if let Some(l) = self.leases.lock().unwrap().get_mut(id) {
            l.upid = Some(upid.to_string());
        }
    }

    /// The run of `id` holds a filesystem lock until it ends (`holder`: what it holds, for a
    /// start without the gate: rynasd was down then, the run gets a lease of its own).
    pub fn attach_guard(&self, id: &str, holder: impl FnOnce() -> Holder, guard: HeavyGuard) {
        let mut l = self.leases.lock().unwrap();
        l.entry(id.to_string())
            .or_insert_with(|| Lease::new(id, "", holder(), None))
            ._guard = Some(guard);
    }

    /// The run of `id` has a task panel entry waiting for its end; returns the run's
    /// invocation id (empty when the gate did not see its start).
    pub fn attach_tracker(
        &self,
        id: &str,
        holder: impl FnOnce() -> Holder,
        tracker: tokio::sync::oneshot::Sender<String>,
    ) -> String {
        let mut l = self.leases.lock().unwrap();
        let lease = l.entry(id.to_string()).or_insert_with(|| Lease::new(id, "", holder(), None));
        lease.tracker = Some(tracker);
        lease.invocation_id.clone()
    }

    /// The run `invocation_id` of `id` ended with systemd's `result`: its lease goes (the
    /// lock with it), its task panel entry learns the result.
    pub fn release(&self, id: &str, invocation_id: &str, result: &str) {
        let lease = {
            let mut l = self.leases.lock().unwrap();
            if l.get(id).is_some_and(|x| x.invocation_id == invocation_id || x.invocation_id.is_empty()) {
                l.remove(id)
            } else {
                None
            }
        };
        if let Some(t) = lease.and_then(|mut x| x.tracker.take()) {
            let _ = t.send(result.to_string());
        }
    }

    /// An earlier start of `id` still runs in rynasd (its rynasd task).
    fn running_elsewhere(&self, id: &str, invocation_id: &str, tasks: &rynas_tasks::TaskManager) -> bool {
        let l = self.leases.lock().unwrap();
        let Some(lease) = l.get(id).filter(|x| x.invocation_id != invocation_id) else {
            return false;
        };
        lease.upid.as_deref().and_then(|u| tasks.get(u)).is_some_and(|t| {
            matches!(t.status, UpidStatus::Running | UpidStatus::Queued)
        })
    }

    /// After a start of rynasd: a lease for every task unit that runs now, the gated system
    /// units too (with the lock of those that work in their own unit, and their task panel
    /// entry). A start still waiting in its gate is not running: its rynasctl asks again and
    /// takes its place in the queue.
    pub async fn rebuild(p: &Arc<Protection>) {
        let p2 = p.clone();
        let found = tokio::task::spawn_blocking(move || -> Vec<(ScheduledTask, String, Holder)> {
            let running = p2.probe.running_tasks().unwrap_or_default();
            running
                .iter()
                .filter_map(|unit| {
                    let id = match unit.strip_prefix(UNIT_PREFIX).and_then(|u| u.strip_suffix(".service")) {
                        Some(id) => id.to_string(),
                        None => rynas_api_types::scheduler::system::GATED_UNITS
                            .iter()
                            .find(|(_, s, _)| s == unit)
                            .map(|(i, ..)| i.to_string())?,
                    };
                    let run = p2.probe.current_run(unit).ok().flatten()?;
                    let t = crate::scheduler::applied_task(&p2, &id).ok().flatten()?;
                    let h = holder_of(&p2, &t);
                    Some((t, run.invocation_id, h))
                })
                .collect()
        })
        .await
        .unwrap_or_default();
        for (t, inv, h) in found {
            p.gate.grant(&t.id, &inv, h, crate::claims::work_kind(&t.action));
            if let TaskAction::Scrub { filesystem } = &t.action {
                let g = crate::fslock::FsLocks::global().heavy_paths(&[filesystem], "btrfs.scrub").await;
                p.gate.attach_guard(&t.id, Holder::default, g);
            }
            if runs_in_unit(&t.action) {
                crate::jobs::track(p, &t);
            }
            tracing::info!("scheduler: task {} runs ({inv}): lease rebuilt", t.id);
        }
        // long SMART tests still running in their drive keep their disk
        let p3 = p.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let applied = p3
                .ops
                .config
                .applied(rynas_scheduler::SCHEDULER_KEY)
                .map(|v| rynas_scheduler::domain::tasks_of(v.as_ref()))
                .unwrap_or_default();
            for t in applied {
                if let TaskAction::SmartTest { disk, test: rynas_api_types::scheduler::SmartTestKind::Long } = &t.action
                    && self_test(&p3, disk).in_progress == Some(true)
                {
                    hold_disk_test(&p3, &t, disk);
                }
            }
        })
        .await;
    }
}

/// A start rynasd asked for (run now, a retry, an interval due, a resume).
#[derive(Debug, Clone, Copy)]
struct Request {
    trigger: RunTrigger,
    attempt: u8,
    at: Instant,
    at_ms: u64,
    skip_queue: bool,
}

/// The gate of this rynasd once its watch loop runs (for the busy checks of other features).
static RUNNING: std::sync::OnceLock<Arc<Gate>> = std::sync::OnceLock::new();

/// The gate of this rynasd, once its watch loop runs: the lock table's leases for other
/// features' questions ([`crate::claims`]).
pub fn running() -> Option<Arc<Gate>> {
    RUNNING.get().cloned()
}

fn reason_text(r: SkipReason) -> &'static str {
    match r {
        SkipReason::PausedAll => "every task is paused",
        SkipReason::Paused => "the task is paused",
        SkipReason::Missed => "a missed run past its catch-up deadline",
        SkipReason::Overlap => "its time came while the previous run was going (overlap policy skip)",
        SkipReason::Running => "the previous run is still going",
        SkipReason::Window => "past the start window of its scheduled time",
        SkipReason::NotDue => "not due yet",
        SkipReason::Busy => "it waited in the queue past its deadline",
        SkipReason::Maintenance => "outside the maintenance window, which opens only after its deadline",
        SkipReason::Interrupted => "it was waiting when the NAS restarted",
    }
}

/// Records what rynasctl spooled while rynasd could not be reached: skipped starts, starts
/// that ran without the gate, ends of runs (with their output from the journal). Each entry
/// is recorded once however often it is imported.
pub fn import_spool(p: &Protection) {
    for e in spool_take(&p.state_dir) {
        let trigger = if e.by_timer { RunTrigger::Timer } else { RunTrigger::Other };
        if let Some(f) = &e.finished {
            let end = crate::scheduler::TaskFinished {
                invocation_id: e.invocation_id.clone(),
                result: f.result.clone(),
                exit_code: f.exit_code.clone(),
                exit_status: f.exit_status.clone(),
            };
            match crate::scheduler::record_end(p, &e.task_id, &end, None, e.at_ms) {
                Ok(Some((run, notify))) => crate::scheduler::collect_output(p, &e.task_id, run, notify),
                Ok(None) => {}
                Err(err) => tracing::warn!("scheduler: spooled end of {}: {}", e.task_id, err.2),
            }
            continue;
        }
        let r = match e.skipped {
            Some(reason) => p.history.record(&NewRun::skipped(
                &e.task_id,
                &e.invocation_id,
                e.at_ms,
                trigger,
                reason,
                Some("rynas was not reachable: the pause file decided".into()),
            )),
            None => p
                .history
                .note_gate(&GateNote {
                    invocation_id: e.invocation_id.clone(),
                    task_id: e.task_id.clone(),
                    at_ms: e.at_ms,
                    trigger: Some(trigger),
                    attempt: 0,
                    ungated: true,
                    queued_ms: 0,
                })
                .map(|_| 0),
        };
        if let Err(err) = r {
            tracing::warn!("scheduler: spooled gate outcome of {}: {err}", e.task_id);
        }
    }
}

pub fn answer(p: &Protection, id: &str, req: &GateRequest) -> Res<GateAnswer> {
    let now = now_ms();
    // (a task an apply in progress adds is found: [`crate::scheduler::applied_task`] reads the
    // value being applied; the gated system units are tasks too)
    let Some(task) = crate::scheduler::applied_task(p, id)? else {
        // rynasd answers, and the task is not rynas's: a unit left behind, nothing to run
        // (skipped, not failed; only an unreachable rynasd runs the start as before the gate)
        return Ok(GateAnswer {
            run: false,
            trigger: RunTrigger::Other,
            skipped: Some(SkipReason::NotDue),
            message: format!("rynas: task {id} is not in the configuration: start skipped"),
            wait: false,
            attempt: 0, deadline_ms: 0
        });
    };
    let pause = p.gate.pause();
    let late = req.answer_by_ms > 0 && now_ms() > req.answer_by_ms;
    // a start waiting in the queue already: its first answer decided how it came about (kept
    // by rynasd, or sent back by rynasctl after a restart of rynasd); a pause set meanwhile
    // still skips it (unless the user asked for it), and so does turning the task off
    let waiting = p.gate.waiting_as(id, &req.invocation_id).or_else(|| {
        req.queued_trigger.filter(|_| req.queued_since_ms > 0).map(|t| (t, req.queued_attempt, false))
    });
    if let Some((trigger, attempt, skip)) = waiting {
        if !task.enabled {
            p.gate.unwait(id);
            return Ok(GateAnswer {
                run: false,
                trigger,
                skipped: Some(SkipReason::NotDue),
                message: "rynas: the task was turned off while it waited in the queue: start skipped".into(),
                wait: false,
                attempt, deadline_ms: 0
            });
        }
        if skip {
            return queue_or_run(p, &task, req, trigger, attempt, true, late, now);
        }
        if !matches!(trigger, RunTrigger::Manual | RunTrigger::Other)
            && let Some(reason) = pause.blocks(id, now)
            && !late
        {
            p.gate.unwait(id);
            let p_info = match reason {
                SkipReason::PausedAll => pause.all.as_ref(),
                _ => pause.tasks.get(id),
            };
            let detail = p_info.map(|i| i.reason.clone()).filter(|r| !r.is_empty());
            return skipped(p, &task, req, now, trigger, reason, detail);
        }
        return queue_or_run(p, &task, req, trigger, attempt, false, late, now);
    }
    // used once, and dropped by a start of the timer (run now on a running task left it);
    // a start rynasd asked for may still name the timer (systemd keeps activation details)
    let asked = p.gate.take_request(id);
    let by_timer = rynas_scheduler::gate::started_by_timer(
        req.trigger_unit == timer_of(id),
        req.trigger_timer_ms,
        asked.map(|q| q.at_ms),
        now,
    );
    let requested = asked.map(|q| (q.trigger, q.attempt));
    let skip_queue = !by_timer && asked.is_some_and(|q| q.skip_queue);
    let tz = rynas_power::system_tz();
    let interval = matches!(task.trigger, TaskTrigger::Interval { .. });
    let last_run = if interval {
        // a history that cannot be read: an interval start is not due (no extra runs)
        match interval_last(p, &task) {
            Ok(l) => l,
            Err(HistoryUnreadable) => Some((now, now)),
        }
    } else {
        p.history.last_ran(id).map_err(Error::internal)?
    };
    let start = Start {
        task: &task,
        now_ms: now,
        by_timer,
        requested,
        last_run,
        not_before_ms: if interval && last_run.is_none() { interval_not_before(p, &task, now) } else { 0 },
        pause: &pause,
        running_elsewhere: p.gate.running_elsewhere(id, &req.invocation_id, &p.ops.tasks),
        tz: &tz,
    };
    let decision = decide(&start);
    let (trigger, attempt) = match &decision {
        Decision::Run { trigger, attempt } => (*trigger, *attempt),
        Decision::Skip { trigger, .. } if late => {
            let attempt = if by_timer { 0 } else { requested.map_or(0, |(_, a)| a) };
            tracing::warn!(
                "scheduler: task {id}: the gate decided after rynasctl's deadline: running the start, no skip recorded"
            );
            (*trigger, attempt)
        }
        // nothing due (an interval that has not passed, a window block that had its start):
        // not an event worth a row; the watch loop starts an interval task when it is due
        Decision::Skip { trigger, reason: SkipReason::NotDue, .. } => {
            return Ok(GateAnswer {
                run: false,
                trigger: *trigger,
                skipped: Some(SkipReason::NotDue),
                message: String::new(),
                wait: false,
                attempt: 0, deadline_ms: 0
            });
        }
        Decision::Skip { trigger, reason, detail } => {
            return skipped(p, &task, req, now, *trigger, *reason, detail.clone());
        }
    };
    queue_or_run(p, &task, req, trigger, attempt, skip_queue, late, now)
}

/// Records a skipped start (an interval task's same skip as its latest: one row) and answers
/// it.
fn skipped(
    p: &Protection,
    task: &ScheduledTask,
    req: &GateRequest,
    now: u64,
    trigger: RunTrigger,
    reason: SkipReason,
    detail: Option<String>,
) -> Res<GateAnswer> {
    let id = task.id.as_str();
    let interval = matches!(task.trigger, TaskTrigger::Interval { .. });
    let repeated = interval
        && p.history.list(id, 1, None).ok().and_then(|v| v.into_iter().next()).is_some_and(|r| {
            r.skipped == Some(reason) && r.trigger == Some(trigger) && r.detail == detail
        });
    if !repeated {
        let run = NewRun::skipped(id, &req.invocation_id, now, trigger, reason, detail.clone());
        p.history.record(&run).map_err(Error::internal)?;
    }
    p.invalidate();
    if trigger == RunTrigger::Retry {
        drop_retry(p, id, "its start was skipped");
    }
    if reason == SkipReason::Missed && task.effective_policy().rules.on_missed {
        p.notifier.emit(missed_event(task));
    }
    let mut message = format!("rynas: start skipped: {}", reason_text(reason));
    if let Some(d) = detail {
        message.push_str(&format!(" ({d})"));
    }
    Ok(GateAnswer { run: false, trigger, skipped: Some(reason), message, wait: false, attempt: 0, deadline_ms: 0 })
}

/// The lock table now, for the gate's questions.
pub fn table_now(p: &Protection) -> crate::claims::Table {
    let running = p.probe.running_tasks().unwrap_or_default();
    let mdstat = std::fs::read_to_string("/proc/mdstat").unwrap_or_default();
    crate::claims::table(Some(&p.gate), &p.ops.tasks, &mdstat, &running)
}

/// A blocker in the words of the unit's journal.
fn blocker_text(b: &Blocker) -> String {
    let who = match (&b.task, b.name.is_empty() || b.task.as_deref() == Some(b.name.as_str()), &b.kind) {
        (Some(t), true, _) => format!("task {t}"),
        (Some(t), false, _) => format!("{} (task {t})", b.name),
        (None, false, _) => b.name.clone(),
        (None, true, Some(k)) => format!("rynas task {k}"),
        (None, true, None) => "other work".into(),
    };
    let what = match &b.resource {
        Resource::Volume { path } if path == rynas_scheduler::queue::EVERY => "every volume".to_string(),
        Resource::Volume { path } => path.clone(),
        Resource::Disk { disk } => format!("disk {disk}"),
        Resource::Packages => "the package system".into(),
        Resource::Power => "a scheduled shutdown".into(),
        Resource::Maintenance => "maintenance".into(),
    };
    format!("{who} on {what}")
}

/// The name of a task (its id when it has none or is unknown).
fn task_name(p: &Protection, id: &str) -> String {
    crate::scheduler::applied_task(p, id)
        .ok()
        .flatten()
        .map(|t| t.name)
        .filter(|n| !n.is_empty())
        .unwrap_or_default()
}

/// The task's unit may wait in its gate: its start timeout is infinite (systemd's own value,
/// with drop-ins; unknown counts as yes, see [`start_timeout_lets_wait`]).
fn unit_can_wait(p: &Protection, id: &str) -> bool {
    let unit = rynas_scheduler::units::service_of(id);
    match p.ops.config.system().run(
        &Cmd::new("systemctl").args(["show", "--no-pager", "--value", "-p", "TimeoutStartUSec", "--", &unit]),
    ) {
        Ok(o) if o.success() => start_timeout_lets_wait(&o.stdout),
        _ => true,
    }
}

/// A start the gate lets through, unless what it claims is busy: then it waits in the queue
/// (S4), or is skipped once it has waited past its deadline. `skip_queue`: run now that the
/// user confirmed. An older rynasctl (`can_wait` false) and a late answer never wait.
#[allow(clippy::too_many_arguments)]
fn queue_or_run(
    p: &Protection,
    task: &ScheduledTask,
    req: &GateRequest,
    trigger: RunTrigger,
    attempt: u8,
    skip_queue: bool,
    late: bool,
    now: u64,
) -> Res<GateAnswer> {
    let id = task.id.as_str();
    let holder = holder_of(p, task);
    // a shutdown waits by its own policy (it holds what it claims at once); a late answer
    // runs a first start (rynasctl may have run it already) but never takes a start that
    // already waits out of the queue (rynasctl keeps waiting until its turn)
    let queued = req.queued_since_ms > 0 || p.gate.is_waiting(id);
    let can_queue = req.can_wait
        && (!late || queued)
        && !skip_queue
        && holder.claims.iter().any(|c| !matches!(c, Claim::Power));
    let since = if req.queued_since_ms > 0 { req.queued_since_ms.min(now) } else { now };
    // the global maintenance window (S5): a scheduled start of heavy work outside it waits
    // for the window (never blocking anyone else meanwhile: it waits with no claims), or is
    // skipped when the window opens only after the start's deadline
    if req.can_wait && (!late || queued) && !skip_queue {
        let window = crate::scheduler::maintenance_window(p);
        let tz = rynas_power::system_tz();
        match rynas_scheduler::maintenance::defer(window.as_deref(), task, trigger, since, now, &tz) {
            Defer::Go => {}
            Defer::Skip(opens) => {
                p.gate.unwait(id);
                let detail = match opens {
                    Some(o) => format!(
                        "the maintenance window opens {}, after the start's deadline",
                        rynas_scheduler::maintenance::wall_clock(&tz, o)
                    ),
                    None => "the maintenance window has no open hour".to_string(),
                };
                return skipped(p, task, req, now, trigger, SkipReason::Maintenance, Some(detail));
            }
            Defer::Until(opens) if queued || unit_can_wait(p, id) => {
                let message = format!(
                    "rynas: deferred: outside the maintenance window, waiting until it opens ({})",
                    rynas_scheduler::maintenance::wall_clock(&tz, opens)
                );
                let me = Waiter { task: id.to_string(), claims: Vec::new(), priority: holder.priority, since_ms: since };
                let deadline_ms = queue_deadline_ms(task, since, &tz);
                if p.gate.wait(id, &req.invocation_id, me, None, Some(opens), trigger, attempt, deadline_ms) {
                    tracing::info!("scheduler: task {id}: {}", message.trim_start_matches("rynas: "));
                    p.invalidate();
                }
                return Ok(GateAnswer { run: false, trigger, skipped: None, message, wait: true, attempt, deadline_ms });
            }
            Defer::Until(_) => {
                tracing::warn!("scheduler: task {id}: its unit has a start timeout (written before rynas 0.8.18?): not deferred to the maintenance window");
            }
        }
    }
    if can_queue {
        let me = Waiter { task: id.to_string(), claims: holder.claims.clone(), priority: holder.priority, since_ms: since };
        let table = table_now(p);
        let others = p.gate.waiters(id);
        let disks = |k: &str| table.disks(k);
        if let Some(block) = blocked_by(&me, &table.holders, &others, &disks) {
            let blocker = match &block {
                Block::Holder(i, c) => table.blocker(&table.holders[*i], c),
                Block::Waiter(i, c) => Blocker {
                    task: Some(others[*i].task.clone()),
                    name: task_name(p, &others[*i].task),
                    kind: None,
                    resource: table.resource(c),
                },
            };
            let deadline_ms = queue_deadline_ms(task, since, &rynas_power::system_tz());
            if now > deadline_ms {
                p.gate.unwait(id);
                return skipped(p, task, req, now, trigger, SkipReason::Busy, Some(blocker_text(&blocker)));
            }
            // a unit kept from before S4 (a start timeout of 90 s) would be killed while it
            // waits: it runs as before the queue
            if !queued && !unit_can_wait(p, id) {
                tracing::warn!("scheduler: task {id}: its unit has a start timeout (written before rynas 0.8.18?): not queued");
            } else {
                let message = format!("rynas: queued: waiting for {}", blocker_text(&blocker));
                if p.gate.wait(id, &req.invocation_id, me, Some(blocker), None, trigger, attempt, deadline_ms) {
                    tracing::info!("scheduler: task {id}: {}", message.trim_start_matches("rynas: "));
                    p.invalidate();
                }
                return Ok(GateAnswer { run: false, trigger, skipped: None, message, wait: true, attempt, deadline_ms });
            }
        }
    }
    let waited = p.gate.unwait(id);
    // rynasctl's own count wins (it survives a restart of rynasd)
    let queued_ms = match (req.queued_since_ms, waited) {
        (s, _) | (0, Some(s)) if s > 0 => now.saturating_sub(s),
        _ => 0,
    };
    p.gate.grant(id, &req.invocation_id, holder, crate::claims::work_kind(&task.action));
    p.history
        .note_gate(&GateNote {
            invocation_id: req.invocation_id.clone(),
            task_id: id.to_string(),
            at_ms: now,
            trigger: Some(trigger),
            attempt,
            ungated: false,
            queued_ms,
        })
        .map_err(Error::internal)?;
    if queued_ms > 0 {
        p.invalidate();
    }
    let message = match trigger {
        _ if late => "rynas: the scheduling gate answered late; running the start".into(),
        _ if queued_ms > 0 => format!("rynas: its turn in the queue came after {} s: starting", queued_ms / 1000),
        RunTrigger::Retry => format!(
            "rynas: retry {attempt} of {} after a failed run",
            task.effective_policy().retry.attempts
        ),
        RunTrigger::Queued => {
            "rynas: its time came while the previous run was going: starting now (overlap policy queue)".into()
        }
        RunTrigger::CatchUp => "rynas: catching up a run missed while the NAS was off".into(),
        RunTrigger::Resume => "rynas: the volume is free again: resuming".into(),
        _ if skip_queue => "rynas: run now, ahead of the queue".into(),
        _ => String::new(),
    };
    Ok(GateAnswer { run: true, trigger, skipped: None, message, wait: false, attempt, deadline_ms: 0 })
}

fn missed_event(task: &ScheduledTask) -> NewEvent {
    let name = if task.name.is_empty() { task.id.clone() } else { task.name.clone() };
    let mut ev = oneshot(
        NotifyCategory::Task,
        NotifySeverity::Warning,
        "scheduler",
        format!("scheduler:task:{}:missed", task.id),
        format!("Task {name}: missed run not caught up"),
    );
    ev.body = "The NAS was off at its scheduled time and came back after the catch-up deadline.".into();
    let ev = code(ev, "sched.task_missed");
    field(field(ev, "task", &task.id), "name", name)
}

/// After a run ended: arms its retry, keeps the chain going or ends it. `true`: the run
/// notifies now; `false`: an armed retry holds its failure notification back ([`hold`]).
///
/// - a success ends the chain (the held failure is dropped: the task works again);
/// - the run of a started retry is that retry, whatever the gate saw (a request lost to a
///   restart of rynasd); the chain moves on, or ends with it (then it notifies);
/// - a newer scheduled run that fails while the chain waits keeps the chain's count and time;
/// - a run started by hand notifies as it always did and leaves the chain alone.
pub fn after_run(p: &Protection, task: &ScheduledTask, run: &NewRun) -> bool {
    if run.ok() {
        let _ = p.history.clear_retry(&task.id);
        return true;
    }
    let armed = p.history.retry(&task.id).ok().flatten();
    let done = match armed {
        Some(a) if a.started_ms.is_some() => a.retry.attempt,
        Some(a) if retries_after(run.trigger) => {
            if let Err(e) = p.history.set_retry(&task.id, &ArmedRetry { after_ms: run.end_ms, ..a }) {
                tracing::warn!("scheduler: retry of {}: {e}", task.id);
            }
            return false;
        }
        _ if !retries_after(run.trigger) => return true,
        _ if run.trigger == Some(RunTrigger::Retry) => run.attempt,
        _ => 0,
    };
    let policy = task.effective_policy();
    match next_retry(&policy.retry, done, run.end_ms) {
        Some(retry) => {
            tracing::info!(
                "scheduler: task {} failed; retry {} of {} at {}",
                task.id,
                retry.attempt,
                retry.attempts,
                retry.due_ms
            );
            let armed = ArmedRetry { retry, after_ms: run.end_ms, started_ms: None };
            match p.history.set_retry(&task.id, &armed) {
                Ok(()) => false,
                Err(e) => {
                    tracing::warn!("scheduler: retry of {}: {e}", task.id);
                    true
                }
            }
        }
        // the last attempt failed: its own notification is the chain's
        None => {
            let _ = p.history.clear_retry(&task.id);
            true
        }
    }
}

/// Holds the failure notification of a run in the task's armed retry ([`after_run`] said
/// so); sent at once when the chain ended meanwhile.
pub fn hold(p: &Protection, id: &str, ev: NewEvent) {
    let held = serde_json::to_string(&ev)
        .ok()
        .is_some_and(|j| p.history.hold(id, &j).unwrap_or(false));
    if !held {
        p.notifier.emit(ev);
    }
}

/// Ends the retry chain of `id` without a success: the failure notification it held back is
/// sent now. Exactly once: only the caller that takes the chain's row sends it.
pub fn drop_retry(p: &Protection, id: &str, why: &str) {
    match p.history.take_retry(id) {
        Ok(Some((armed, held))) => {
            tracing::info!("scheduler: task {id}: retry {} dropped: {why}", armed.retry.attempt);
            if let Some(ev) = held.and_then(|j| serde_json::from_str::<NewEvent>(&j).ok()) {
                p.notifier.emit(ev);
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!("scheduler: retry of {id}: {e}"),
    }
}

/// The watch loop: due retries, the overlap policy `replace`, pauses that end by themselves.
pub async fn watch(p: Arc<Protection>) {
    let _ = started_ms();
    let _ = RUNNING.set(p.gate.clone());
    Gate::rebuild(&p).await;
    loop {
        tokio::time::sleep(WATCH).await;
        let p2 = p.clone();
        let _ = tokio::task::spawn_blocking(move || tick(&p2)).await;
    }
}

/// Tasks gone from the applied configuration since the last tick (deleted): their pause and
/// their retry chain go.
fn forget_deleted(p: &Protection, applied: &[ScheduledTask]) {
    let now: HashSet<String> = applied.iter().map(|t| t.id.clone()).collect();
    let gone: Vec<String> = {
        let mut known = p.gate.known.lock().unwrap();
        let gone = known
            .as_ref()
            .map(|old| old.difference(&now).cloned().collect())
            .unwrap_or_default();
        *known = Some(now);
        gone
    };
    for id in gone {
        if p.gate.pause().tasks.contains_key(&id) {
            match p.gate.set_pause_task(&id, None) {
                Ok(_) => tracing::info!("scheduler: task {id} deleted: its pause goes"),
                Err(e) => tracing::warn!("scheduler: pause of deleted task {id}: {}", e.2),
            }
        }
        drop_retry(p, &id, "the task was deleted");
        p.gate.drop_yield(&id);
    }
}

/// Milliseconds since the epoch when the machine booted (`/proc/uptime`).
fn boot_ms(now: u64) -> u64 {
    let up = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|t| t.split_whitespace().next()?.parse::<f64>().ok())
        .unwrap_or(0.0);
    now.saturating_sub((up * 1000.0) as u64)
}

/// The interval tasks that are due now: their interval (and random part) has passed since
/// their last run (or the feature's own last check), or, never run, their delay after the
/// boot or rynasd's start has. Not while paused, a retry chain waits, or a start rynasd asked
/// for is on its way. Their timer's own starts go through the gate, which skips those that
/// are not due; this is what starts them at the right time after such a skip. Also keeps the
/// Docker page's next check.
fn due_intervals<'a>(
    p: &Protection,
    applied: &'a [ScheduledTask],
    retries: &[(String, ArmedRetry)],
    pause: &SchedulerPause,
    now: u64,
) -> Vec<&'a ScheduledTask> {
    let mut out = Vec::new();
    for t in applied.iter().filter(|t| matches!(t.trigger, TaskTrigger::Interval { .. })) {
        // a history that cannot be read: nothing is started (no extra runs)
        let Ok(last) = interval_last(p, t) else { continue };
        let due = interval_due(p, t, last, now);
        // the Docker page shows the next check: the same time the loop starts it at
        if matches!(t.action, TaskAction::DockerImageCheck)
            && let Some(d) = p.ops.docker.get()
        {
            d.next_check_ms.store(if t.enabled { due } else { 0 }, std::sync::atomic::Ordering::Relaxed);
        }
        if !t.enabled
            || due > now
            || pause.blocks(&t.id, now).is_some()
            || retries.iter().any(|(id, _)| *id == t.id)
            || p.gate.requested_now(&t.id)
            || !p.gate.interval_may_start(&t.id, last.map(|l| l.1), t)
        {
            continue;
        }
        out.push(t);
    }
    out
}

/// When rynasd started (ms since the epoch): an interval task that never ran counts its
/// delay from the later of the boot and this, as the loops it replaces did.
static STARTED_MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

pub fn started_ms() -> u64 {
    *STARTED_MS.get_or_init(now_ms)
}

/// The job's own record of its last check, kept by its feature (before the task existed, or a
/// "Check now"): the next automatic one counts from it too.
fn feature_last_ms(p: &Protection, t: &ScheduledTask) -> Option<u64> {
    let docker = p.ops.docker.get()?;
    match t.action {
        TaskAction::DockerImageCheck => Some(docker.report.lock().unwrap().checked_at_ms).filter(|m| *m > 0),
        TaskAction::AppsCatalog => docker.apps.get()?.upgrade()?.online.get()?.last_check_ms(),
        _ => None,
    }
}

/// The last run of an interval task: its latest recorded run, or the feature's own last
/// check when that is later (as `(start, end)`). `Err` when the history cannot be read.
pub fn interval_last(p: &Protection, t: &ScheduledTask) -> Result<Option<(u64, u64)>, HistoryUnreadable> {
    let ran = p.history.last_ran(&t.id).map_err(|e| {
        tracing::warn!("scheduler: history of {}: {e}", t.id);
        HistoryUnreadable
    })?;
    Ok(match (ran, feature_last_ms(p, t)) {
        (Some(r), Some(f)) if f > r.1 => Some((f, f)),
        (r, None) | (r @ Some(_), Some(_)) => r,
        (None, Some(f)) => Some((f, f)),
    })
}

/// An interval task that never ran is not due before its delay after the later of the boot
/// and the start of rynasd that first saw it (as the loops counted after a start). That time
/// is kept in `<state>/scheduler/first-due.json`, so a later restart of rynasd does not push
/// it back again; it no longer matters once the task has run.
pub fn interval_not_before(p: &Protection, t: &ScheduledTask, now: u64) -> u64 {
    let TaskTrigger::Interval { after_boot_min, .. } = t.trigger else {
        return 0;
    };
    first_due(&p.state_dir.join(FIRST_DUE_FILE), &t.id, || {
        boot_ms(now).max(started_ms()) + u64::from(after_boot_min) * 60_000
    })
}

/// The time stored for `id` in the file `path`, or `compute()` stored there now.
fn first_due(path: &std::path::Path, id: &str, compute: impl FnOnce() -> u64) -> u64 {
    static LOCK: Mutex<()> = Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut map: std::collections::BTreeMap<String, u64> = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if let Some(at) = map.get(id) {
        return *at;
    }
    let at = compute();
    map.insert(id.to_string(), at);
    let tmp = path.with_extension("json.tmp");
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|_| std::fs::write(&tmp, serde_json::to_vec(&map).unwrap_or_default()))
        .and_then(|_| std::fs::rename(&tmp, path));
    if let Err(e) = written {
        tracing::warn!("scheduler: {}: {e}", path.display());
    }
    at
}

/// The first due time of the interval tasks that never ran ([`interval_not_before`]).
pub const FIRST_DUE_FILE: &str = "scheduler/first-due.json";

/// When an interval task is due ([`rynas_scheduler::gate::interval_due_ms`] from its last
/// run, else [`interval_not_before`]).
fn interval_due(p: &Protection, t: &ScheduledTask, last: Option<(u64, u64)>, now: u64) -> u64 {
    match last {
        Some((_, end)) => rynas_scheduler::gate::interval_due_ms(t, end).unwrap_or(u64::MAX),
        None => interval_not_before(p, t, now),
    }
}

/// A scrub whose cancel failed is asked again after this long.
const YIELD_RETRY: Duration = Duration::from_secs(60);
/// A scrub asked to give way whose end was never reported counts as stopped after this long
/// (rynasd was away when it ended).
const YIELD_LOST_MS: u64 = 120_000;

/// A run is asked to give way only this long after its lease was granted.
const YIELD_SETTLE: Duration = Duration::from_secs(3);

/// A scrub asked to give way gets its signal again after this long while its run goes on.
const YIELD_RESEND: Duration = Duration::from_secs(30);

/// SIGINT to the processes of the unit of `id`: `btrfs scrub start -B` cancels its scrub and
/// records the position for `btrfs scrub resume` itself (its own SIGINT handler), inside the
/// unit, which sees the volume as it is; rynasctl, the main process, waits for it (and gives
/// way at once when the command has not started yet). `false` when systemd refused.
fn send_yield(p: &Protection, id: &str) -> bool {
    let unit = service_name(id);
    let r = p
        .ops
        .config
        .system()
        .run(&Cmd::new("systemctl").args(["kill", "--kill-whom=all", "--signal=SIGINT", &unit]));
    p.gate.yield_sent.lock().unwrap().insert(id.to_string(), Instant::now());
    match r {
        Ok(o) if o.success() => true,
        Ok(o) => {
            tracing::warn!("scheduler: task {id}: stopping the scrub: exit code {:?}: {}", o.code, o.stderr.trim());
            false
        }
        Err(e) => {
            tracing::warn!("scheduler: task {id}: stopping the scrub: {e}");
            false
        }
    }
}

/// The queue's turn of the watch loop (S4): a running scrub gives way to more urgent work on
/// its volume (SIGINT to its unit: btrfs-progs cancels and keeps its position), and a scrub
/// that gave way resumes
/// once nothing more urgent holds or waits for its volume.
fn queue_tick(p: &Protection, applied: &[ScheduledTask], now: u64) {
    let leases = p.gate.lease_views();
    let yields = p.gate.yields();
    if yields.is_empty() && !leases.iter().any(|l| l.holder.gives == Gives::Yields) {
        return;
    }
    let table = table_now(p);
    let waiters = p.gate.waiters("");
    let disks = |k: &str| table.disks(k);
    for l in leases.iter().filter(|l| l.holder.gives == Gives::Yields) {
        let Some(id) = l.holder.task.as_deref() else { continue };
        // (a run granted moments ago: its rynasctl may not handle signals yet; the plan it
        // asks for says "give way" when asked meanwhile)
        if l.invocation_id.is_empty() || l.age < YIELD_SETTLE {
            continue;
        }
        match yields.get(id) {
            // asked to give way and still running: the signal again, now and then (one that
            // came before btrfs started, or a restart of rynasd between the entry and the
            // signal, is not lost; btrfs takes a second SIGINT as the first)
            Some(e) if e.invocation_id == l.invocation_id && !e.stopped => {
                let resend = p.gate.yield_sent.lock().unwrap().get(id).is_none_or(|at| at.elapsed() >= YIELD_RESEND);
                if resend {
                    send_yield(p, id);
                }
                continue;
            }
            // this run gave way and is ending
            Some(e) if e.invocation_id == l.invocation_id => continue,
            // none, or an earlier run's (this run resumes it): this run may be asked
            _ => {}
        }
        if p.gate.yield_failed.lock().unwrap().get(id).is_some_and(|at| at.elapsed() < YIELD_RETRY) {
            continue;
        }
        let Some(block) = yield_to(&l.holder, &table.holders, &waiters, &disks) else { continue };
        let Some(TaskAction::Scrub { filesystem }) = applied.iter().find(|t| t.id == id).map(|t| &t.action) else {
            continue;
        };
        let to = match &block {
            Block::Holder(i, c) => table.blocker(&table.holders[*i], c),
            Block::Waiter(i, c) => Blocker {
                task: Some(waiters[*i].task.clone()),
                name: task_name(p, &waiters[*i].task),
                kind: None,
                resource: table.resource(c),
            },
        };
        tracing::info!("scheduler: task {id}: the scrub gives way to {}", blocker_text(&to));
        // written before the cancel: the unit reads it to tell this end from a failure
        let entry = YieldEntry {
            filesystem: filesystem.clone(),
            invocation_id: l.invocation_id.clone(),
            since_ms: now,
            to: Some(to),
            stopped: false,
        };
        // no entry on disk, no signal: the unit could not tell its end from a failure
        if p.gate.change_yields(|m| {
            m.insert(id.to_string(), entry);
        })
        .is_err()
        {
            p.gate.yield_failed.lock().unwrap().insert(id.to_string(), Instant::now());
            continue;
        }
        if !send_yield(p, id) {
            p.gate.yield_failed.lock().unwrap().insert(id.to_string(), Instant::now());
            p.gate.drop_yield(id);
        }
        p.invalidate();
    }
    // resumes
    let pause = p.gate.pause();
    let running = p.probe.running_tasks().unwrap_or_default();
    for (id, e) in p.gate.yields() {
        let Some(task) = applied.iter().find(|t| t.id == id) else {
            tracing::info!("scheduler: task {id} is gone: its scrub is not resumed");
            p.gate.drop_yield(&id);
            continue;
        };
        if !task.enabled {
            p.gate.drop_yield(&id);
            continue;
        }
        let unit = service_name(&id);
        let leased = leases.iter().any(|l| l.holder.task.as_deref() == Some(id.as_str()));
        if !e.stopped {
            // asked to give way, its end never reported (rynasd was away): stopped
            if !leased && !running.contains(&unit) && now.saturating_sub(e.since_ms) > YIELD_LOST_MS {
                let _ = p.gate.change_yields(|m| {
                    if let Some(x) = m.get_mut(&id) {
                        x.stopped = true;
                    }
                });
            }
            continue;
        }
        if leased
            || running.contains(&unit)
            || pause.blocks(&id, now).is_some()
            || p.gate.requested_now(&id)
            || p.gate.waiting.lock().unwrap().contains_key(&id)
        {
            continue;
        }
        let holder = holder_of(p, task);
        let me = Waiter { task: id.clone(), claims: holder.claims, priority: holder.priority, since_ms: e.since_ms };
        if blocked_by(&me, &table.holders, &waiters, &disks).is_some() {
            continue;
        }
        p.gate.request(&id, RunTrigger::Resume, 0);
        match p.ops.config.system().start_unit(&unit) {
            Ok(()) => tracing::info!("scheduler: task {id}: its volume is free: the scrub resumes"),
            Err(err) => tracing::warn!("scheduler: task {id}: resume: {err}"),
        }
        p.invalidate();
    }
}

/// The system is shutting down or rebooting (`systemctl is-system-running` says `stopping`).
pub fn shutting_down(p: &Protection) -> bool {
    p.ops
        .config
        .system()
        .run(&Cmd::new("systemctl").arg("is-system-running"))
        .is_ok_and(|o| o.stdout.trim() == "stopping")
}

/// The waiting starts kept on disk: one from an earlier boot was stopped by the restart (its
/// timer has moved on and `Persistent=` will not catch it up): recorded as interrupted, and
/// started again when it may still run ([`rynas_scheduler::gate::rearm_parked`]); one of this
/// boot whose unit no longer waits (stopped while rynasd was away) is forgotten.
fn parked_tick(p: &Protection, applied: &[ScheduledTask], now: u64) {
    let parked = p.gate.parked();
    if parked.is_empty() {
        return;
    }
    for (id, w) in parked {
        let unit = rynas_scheduler::units::service_of(&id);
        if w.boot_id == p.gate.boot() {
            if p.gate.is_waiting(&id) {
                continue;
            }
            // the unit may still wait (rynasd restarted: its rynasctl asks again soon)
            match p.probe.current_run(&unit) {
                Ok(Some(r)) if r.invocation_id == w.invocation_id => {}
                Ok(_) if now.saturating_sub(started_ms()) > WAITER_STALE.as_millis() as u64 => p.gate.unpark(&id),
                _ => {}
            }
            continue;
        }
        p.gate.unpark(&id);
        let task = applied
            .iter()
            .find(|t| t.id == id)
            .cloned()
            .map(|mut t| {
                t.migrate();
                t
            })
            .or_else(|| crate::scheduler::gated_system_task(p, &id));
        let Some(task) = task else { continue };
        let again = rynas_scheduler::gate::rearm_parked(&w, &task, now);
        let what = if w.window { "the maintenance window" } else { "its turn in the queue" };
        let detail = if again {
            format!("waiting for {what} when the NAS restarted: started again")
        } else {
            format!("waiting for {what} when the NAS restarted: not started again")
        };
        let mut r = NewRun::skipped(&id, &w.invocation_id, now, w.trigger, SkipReason::Interrupted, Some(detail));
        r.attempt = w.attempt;
        if let Err(e) = p.history.record(&r) {
            tracing::warn!("scheduler: task {id}: {e}");
        }
        p.invalidate();
        if again {
            p.gate.request(&id, RunTrigger::CatchUp, w.attempt);
            match p.ops.config.system().start_unit(&unit) {
                Ok(()) => tracing::info!("scheduler: task {id}: its wait was cut by a restart of the NAS: started again"),
                Err(e) => tracing::warn!("scheduler: task {id}: start again after the restart: {e}"),
            }
        } else {
            tracing::info!("scheduler: task {id}: its wait was cut by a restart of the NAS: recorded, not started again");
        }
    }
}

/// One turn of the watch loop (public for the tests).
pub fn tick(p: &Protection) {
    import_spool(p);
    let now = now_ms();
    let applied: Vec<ScheduledTask> = match p.ops.config.applied(rynas_scheduler::SCHEDULER_KEY) {
        Ok(v) => rynas_scheduler::domain::tasks_of(v.as_ref()),
        Err(e) => {
            tracing::warn!("scheduler watch: {e}");
            return;
        }
    };
    forget_deleted(p, &applied);
    if p.gate.expire_waiters() {
        p.invalidate();
    }
    if p.gate.has_leases()
        && let Ok(running) = p.probe.running_tasks()
    {
        p.gate.sweep(&running);
    }
    disk_tests_tick(p, now);
    queue_tick(p, &applied, now);
    parked_tick(p, &applied, now);
    let retries = p.history.retries().unwrap_or_default();
    let replacing: Vec<&ScheduledTask> = applied
        .iter()
        .filter(|t| t.effective_policy().overlap == rynas_api_types::scheduler::Overlap::Replace)
        .collect();
    let pause = p.gate.pause();
    let due = due_intervals(p, &applied, &retries, &pause, now);
    // nothing to do: no systemd question
    if retries.is_empty() && replacing.is_empty() && due.is_empty() {
        return;
    }
    let running = p.probe.running_tasks().unwrap_or_default();
    for task in due {
        let unit = service_name(&task.id);
        if running.contains(&unit) {
            continue;
        }
        p.gate.request(&task.id, RunTrigger::Timer, 0);
        match p.ops.config.system().start_unit(&unit) {
            Ok(()) => tracing::info!("scheduler: task {}: its interval has passed: started", task.id),
            Err(e) => tracing::warn!("scheduler: task {}: start: {e}", task.id),
        }
    }
    for (id, armed) in retries {
        let unit = service_name(&id);
        let Some(task) = applied.iter().find(|t| t.id == id) else {
            drop_retry(p, &id, "the task is gone");
            continue;
        };
        if !task.enabled {
            drop_retry(p, &id, "the task is off");
            continue;
        }
        // a run goes on: its end moves the chain on (the retry itself, or a run before it);
        // a retry waiting in the queue is a start on its way, like any other (S4)
        if running.contains(&unit) || p.gate.is_waiting(&id) {
            continue;
        }
        if let Some(at) = armed.started_ms {
            // started, nothing runs and no end came: the start was lost
            if now.saturating_sub(at) > REQUEST_TTL.as_millis() as u64 {
                drop_retry(p, &id, "its start did not happen");
            }
            continue;
        }
        if armed.retry.due_ms > now {
            continue;
        }
        if let Some(reason) = pause.blocks(&id, now) {
            // a pause drops the chain (the next scheduled run comes after the pause)
            let inv = format!("retry{:x}{}", now, armed.retry.attempt);
            let mut r = NewRun::skipped(&id, &inv, now, RunTrigger::Retry, reason, None);
            r.attempt = armed.retry.attempt;
            let _ = p.history.record(&r);
            p.invalidate();
            drop_retry(p, &id, "the task is paused");
            continue;
        }
        let _ = p.history.retry_started(&id, now);
        p.gate.request(&id, RunTrigger::Retry, armed.retry.attempt);
        match p.ops.config.system().start_unit(&unit) {
            Ok(()) => tracing::info!("scheduler: task {id}: retry {} started", armed.retry.attempt),
            Err(e) => {
                tracing::warn!("scheduler: task {id}: retry: {e}");
                drop_retry(p, &id, "its unit could not be started");
            }
        }
    }
    // overlap policy replace: a time came after the running one started
    let tz = rynas_power::system_tz();
    for task in replacing {
        let unit = service_name(&task.id);
        if !running.contains(&unit) || pause.blocks(&task.id, now).is_some() {
            continue;
        }
        let Some(start) = p.probe.start_ms(&unit) else { continue };
        if replace_due(task, start, now, &tz) {
            tracing::info!(
                "scheduler: task {}: its time came while it ran: restarting it (overlap policy replace)",
                task.id
            );
            p.gate.request(&task.id, RunTrigger::Timer, 0);
            let _ = p
                .ops
                .config
                .system()
                .run(&Cmd::new("systemctl").args(["restart", "--no-block", &unit]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A start of a due interval task that leaves its last run where it was (the gate skipped
    /// it) is tried again later and later, never every few minutes; a new last run resets it.
    #[test]
    fn interval_restarts_back_off() {
        let dir = tempfile::tempdir().unwrap();
        let g = Gate::new(dir.path().to_path_buf());
        let t = rynas_api_types::scheduler::system::pc_retention();
        assert!(g.interval_may_start(&t.id, Some(1), &t));
        assert!(!g.interval_may_start(&t.id, Some(1), &t), "not again at once");
        // pretend the wait passed: the next wait doubles
        let back = |g: &Gate, by: Duration| {
            let mut m = g.interval_started.lock().unwrap();
            let e = m.get_mut(&t.id).unwrap();
            e.0 = Instant::now().checked_sub(by).unwrap();
        };
        back(&g, INTERVAL_RESTART + Duration::from_secs(1));
        assert!(g.interval_may_start(&t.id, Some(1), &t));
        back(&g, INTERVAL_RESTART + Duration::from_secs(1));
        assert!(!g.interval_may_start(&t.id, Some(1), &t), "the second wait is twice as long");
        // at most the interval (one hour for the retention)
        back(&g, Duration::from_secs(3601));
        assert!(g.interval_may_start(&t.id, Some(1), &t));
        // a run happened: at once again
        assert!(g.interval_may_start(&t.id, Some(2), &t));
    }

    /// A start waiting in its gate is kept on disk until it runs or is skipped; a stop while
    /// the NAS shuts down keeps it (started again after the boot), a stop by hand forgets it;
    /// and a scheduled shutdown never waits for it.
    #[test]
    fn waiting_starts_are_parked_and_never_busy() {
        let dir = tempfile::tempdir().unwrap();
        let g = Gate::new(dir.path().to_path_buf());
        let me = Waiter { task: "scrub-a".into(), claims: Vec::new(), priority: 30, since_ms: 1000 };
        assert!(g.wait("scrub-a", "inv1", me.clone(), None, Some(5000), RunTrigger::Timer, 0, 9000));
        let on_disk = load_parked(dir.path());
        assert_eq!(on_disk["scrub-a"].deadline_ms, 9000);
        assert!(on_disk["scrub-a"].window);
        // a new rynasd reads it back
        assert_eq!(Gate::new(dir.path().to_path_buf()).parked().len(), 1);
        // the shutdown: not busy (the unit is in its gate, holds nothing)
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _e = rt.enter();
        let tasks = rynas_tasks::TaskManager::new(dir.path().join("t"), "nas", Default::default()).unwrap();
        let t = crate::claims::table_in(Some(&g), &tasks, "", &["rynas-task-scrub-a.service".into()], "", std::path::Path::new("/nonexistent"));
        assert!(crate::claims::power_busy(&t, true, true, "", "rynas-task-off.service").is_empty());
        // stopped by a shutdown: kept
        assert!(g.ended_waiting("scrub-a", "inv1", true));
        assert_eq!(load_parked(dir.path()).len(), 1);
        // stopped by hand: forgotten
        assert!(g.wait("scrub-a", "inv2", me, None, None, RunTrigger::Timer, 0, 9000));
        assert!(g.ended_waiting("scrub-a", "inv2", false));
        assert!(load_parked(dir.path()).is_empty());
    }

    /// The first due time of a never-run interval task is kept: a restart of rynasd (another
    /// computed time) does not push it back.
    #[test]
    fn first_due_survives_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FIRST_DUE_FILE);
        assert_eq!(first_due(&path, "updates-check", || 1000), 1000);
        assert_eq!(first_due(&path, "updates-check", || 9000), 1000, "after a restart");
        assert_eq!(first_due(&path, "pcbackup-retention", || 5000), 5000);
        assert_eq!(first_due(&path, "updates-check", || 9000), 1000);
    }
}
