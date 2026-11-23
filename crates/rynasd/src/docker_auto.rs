//! Automatic image updates (D-UPDATES channel C, `docs/phase2/docker-apps.md` §5.8).
//!
//! Both are scheduler tasks owned by Docker (S3 of `docs/research/scheduling.md`; before,
//! a loop of rynasd with the same times):
//!
//! - **Daily check** (`docker-update-check`): five minutes after boot at the earliest, then
//!   24 hours after the last one plus up to an hour at random. It is the
//!   `docker.update-check` task with the registry requests spread out; notifications only
//!   change with the state ([`crate::docker::check_updates`]).
//! - **Window** (`docker-auto-update`): once a day, inside `docker.auto_update` (start,
//!   length; the start delayed at random within its first third), the stacks
//!   that take automatic updates and whose update the policy allows are updated one at a
//!   time, each as a task (`docker.auto-update`, or `apps.auto-update` for a catalogue app):
//!   pull → (version label check for floating tags) → snapshot → `up -d` → health check
//!   ([`rynas_docker::health`]) → on failure the rollback, and a notification either way.
//!   Nothing starts while rynas maintenance runs (a scrub, a RAID resync, a system update,
//!   an engine move…) or after the window has ended.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use rynas_api_types::TaskStatus;
use rynas_api_types::apps::{AppUpdatePlan, AppUpdateRequest, ImageChangeKind};
use rynas_api_types::docker::*;
use rynas_api_types::notify::{NewEvent, NotifyCategory, NotifySeverity};
use rynas_docker::policy::{self, VERSION_LABEL, Verdict};
use rynas_docker::stacks::ComposeAction;
use rynas_docker::{Layout, health, updates};
use rynas_notify::event::{code, field, oneshot};
use rynas_tasks::{Cmd, TaskCtx};
use serde::{Deserialize, Serialize};

use crate::apps::AppCandidate;
use crate::docker::{DockerSvc, audit, compose, compose_for, current_images, stack_services, do_rollback, mark_busy, pre_snapshot};

const MIN: u64 = 60_000;
/// How often the scheduler looks at the clock.
const TICK: Duration = Duration::from_secs(60);
/// A report older than this is refreshed before a window.
const STALE_REPORT: u64 = 26 * 60 * MIN;
/// Interval of the health readings after `up`.
const HEALTH_POLL: Duration = Duration::from_secs(10);
/// The most one automatic stack update may take (pull, snapshot, up, health check,
/// rollback) before it is cancelled and recorded as failed, so a hung command cannot stop
/// the window and the daily check.
pub const STACK_DEADLINE: Duration = Duration::from_secs(45 * 60);
/// The same for the scheduled update check (requests spread out, rate-limit pauses).
const CHECK_DEADLINE: Duration = Duration::from_secs(2 * 60 * 60);
/// After a cancellation, how long the task gets to end through its own failure path.
const CANCEL_GRACE: Duration = Duration::from_secs(2 * 60);

// ---------------------------------------------------------------------------------------------
// state
// ---------------------------------------------------------------------------------------------

/// Held after a download: the version labels did not allow it. The same digests are not
/// downloaded again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Held {
    pub digests: BTreeSet<String>,
    pub reason: Option<HoldReason>,
}

/// `<state>/docker/auto.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AutoState {
    #[serde(default)]
    pub results: BTreeMap<String, AutoResult>,
    #[serde(default)]
    pub held: BTreeMap<String, Held>,
    /// The day (`YYYY-MM-DD`, local) of the last window that ran.
    #[serde(default)]
    pub last_window: Option<String>,
    /// Installed apps with a newer catalogue entry, from the last check.
    #[serde(default)]
    pub apps: Vec<AppCandidate>,
}

impl AutoState {
    pub fn load(dir: &std::path::Path) -> Self {
        std::fs::read_to_string(dir.join("auto.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }
}

impl DockerSvc {
    pub(crate) fn auto_change(&self, f: impl FnOnce(&mut AutoState)) {
        let text = {
            let mut a = self.auto.lock().unwrap();
            f(&mut a);
            serde_json::to_string(&*a).unwrap_or_default()
        };
        let _ = std::fs::create_dir_all(&self.state_dir);
        let tmp = self.state_dir.join("auto.json.new");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, self.state_dir.join("auto.json"));
        }
    }

    fn record_auto(&self, r: AutoResult) {
        self.auto_change(|a| {
            a.results.insert(r.stack.clone(), r);
        });
        self.changed();
    }

    /// A rynas stack whose files rynas can change (in its folder, or adopted read-write).
    pub(crate) fn updatable_stack(&self, name: &str) -> bool {
        if let Some(l) = self.layout()
            && rynas_docker::stacks::find_compose(&rynas_docker::under(&self.root, &l.stack(name)))
                .is_some()
        {
            return true;
        }
        self.adopted_get(name).is_some_and(|a| !a.read_only)
    }
}

// ---------------------------------------------------------------------------------------------
// what the policy does with each stack's update
// ---------------------------------------------------------------------------------------------

/// The tag of an image reference, without a pinned digest.
pub fn tag_of(image: &str) -> Option<String> {
    let bare = image.split('@').next().unwrap_or(image);
    updates::split_ref(bare).1
}

/// The verdict of a catalogue app update.
pub fn judge_app(c: &AppCandidate, own: Option<UpdatePolicy>) -> (Verdict, Option<String>) {
    let p = &c.plan;
    let major = |v: &str| v.split('.').next().map(str::to_string);
    let change = if p.from_app_version != p.to_app_version {
        Some(format!("{} → {}", p.from_app_version, p.to_app_version))
    } else {
        Some(format!("{} → {}", p.from_version, p.to_version))
    };
    if !p.missing_values.is_empty() || major(&p.from_version) != major(&p.to_version) {
        return (Verdict::Hold(HoldReason::NeedsReview), change);
    }
    let mut v = Verdict::Auto;
    for ch in &p.image_changes {
        let step = match ch.kind {
            ImageChangeKind::Added | ImageChangeKind::Removed => Verdict::Hold(HoldReason::NeedsReview),
            ImageChangeKind::Digest => match own {
                Some(UpdatePolicy::Notify) => Verdict::Hold(HoldReason::Policy),
                _ => Verdict::Auto,
            },
            ImageChangeKind::Tag => {
                let (Some(from), Some(to)) = (
                    ch.from.as_deref().and_then(tag_of),
                    ch.to.as_deref().and_then(tag_of),
                ) else {
                    return (Verdict::Hold(HoldReason::UnknownVersion), change);
                };
                let pol = own
                    .or(if ch.service == c.main_service { c.policy } else { None })
                    .unwrap_or_else(|| policy::default_policy(&from));
                policy::judge_versions(pol, &from, &to)
            }
        };
        v = v.and(step);
    }
    if let Some(UpdatePolicy::Notify) = own.or(c.policy) {
        v = Verdict::Hold(HoldReason::Policy);
    }
    (v, change)
}

/// What [`Held`] records of a catalogue app's update that failed: its version change.
fn app_hold_key(change: &str) -> String {
    format!("app:{change}")
}

/// Every stack with an update and what its policy does with it. `managed`: rynas can change
/// the stack's files.
pub fn stack_updates(
    report: &UpdateReport,
    set: &AutoUpdateSettings,
    apps: &[AppCandidate],
    held: &BTreeMap<String, Held>,
    managed: &dyn Fn(&str) -> bool,
) -> Vec<StackUpdate> {
    let mut by_stack: BTreeMap<String, Vec<&ImageUpdate>> = BTreeMap::new();
    for i in report.images.iter().filter(|i| i.update_available) {
        for s in &i.stacks {
            by_stack.entry(s.clone()).or_default().push(i);
        }
    }
    let mut out = Vec::new();
    for (stack, imgs) in by_stack {
        // a catalogue app's own images are pinned: its updates come from the catalogue
        if apps.iter().any(|a| a.stack == stack) {
            continue;
        }
        let own = set.of(&stack);
        let mut v = Verdict::Auto;
        let mut pols = BTreeSet::new();
        for i in &imgs {
            let tag = tag_of(&i.image).unwrap_or_default();
            let pol = own.policy.unwrap_or_else(|| policy::default_policy(&tag));
            pols.insert(pol);
            let held_now = held.get(&stack).filter(|h| {
                i.remote_digest.as_ref().is_some_and(|d| h.digests.contains(d))
            });
            v = v.and(match held_now {
                Some(h) => Verdict::Hold(h.reason.unwrap_or(HoldReason::UnknownVersion)),
                None => policy::judge_digest(pol, &tag),
            });
        }
        if !managed(&stack) {
            v = Verdict::Hold(HoldReason::NotManaged);
        }
        let (verdict, reason) = v.api();
        out.push(StackUpdate {
            stack: stack.clone(),
            app: None,
            images: imgs.iter().map(|i| i.image.clone()).collect(),
            policy: own.policy.or((pols.len() == 1).then(|| *pols.iter().next().unwrap())),
            enabled: own.enabled,
            verdict,
            reason,
            change: None,
        });
    }
    for c in apps {
        let own = set.of(&c.stack);
        let (mut v, change) = judge_app(c, own.policy);
        // the same entry failed at its last automatic update
        if let Some(ch) = &change
            && held.get(&c.stack).is_some_and(|h| h.digests.contains(&app_hold_key(ch)))
        {
            v = Verdict::Hold(HoldReason::Failed);
        }
        let (verdict, reason) = v.api();
        out.push(StackUpdate {
            stack: c.stack.clone(),
            app: Some(c.plan.app.clone()),
            images: c
                .plan
                .image_changes
                .iter()
                .map(|ch| {
                    format!(
                        "{}: {} → {}",
                        ch.service,
                        ch.from.as_deref().and_then(tag_of).unwrap_or_else(|| "-".into()),
                        ch.to.as_deref().and_then(tag_of).unwrap_or_else(|| "-".into())
                    )
                })
                .collect(),
            policy: own
                .policy
                .or(c.policy)
                .or_else(|| {
                    c.plan
                        .image_changes
                        .iter()
                        .find(|ch| ch.service == c.main_service)
                        .and_then(|ch| ch.from.as_deref().and_then(tag_of))
                        .map(|t| policy::default_policy(&t))
                }),
            enabled: own.enabled,
            verdict,
            reason,
            change,
        });
    }
    out.sort_by(|a, b| a.stack.cmp(&b.stack));
    out
}

/// The report with the stacks' verdicts, the last automatic results and the schedule.
pub fn enrich(s: &DockerSvc, mut r: UpdateReport) -> UpdateReport {
    let set = s.settings().auto_update;
    let a = s.auto.lock().unwrap().clone();
    r.stacks = stack_updates(&r, &set, &a.apps, &a.held, &|n| s.updatable_stack(n));
    r.auto = a.results.into_values().collect();
    // when the scheduler starts the check (its watch loop keeps it up to date)
    r.next_check_ms = s.next_check_ms.load(std::sync::atomic::Ordering::Relaxed);
    r.next_window_ms = if set.stacks.values().any(|x| x.enabled) {
        next_window_ms(&set, a.last_window.as_deref())
    } else {
        0
    };
    r
}

/// Forgets the automatic results and the held downloads of stacks rynas no longer has
/// (removed, released). Nothing is forgotten while the stacks folder is not there: an
/// unmounted volume hides every stack.
pub(crate) fn prune_gone(s: &DockerSvc) {
    let Some(l) = s.layout() else {
        return;
    };
    if !rynas_docker::under(&s.root, &l.stacks()).is_dir() {
        return;
    }
    let names: BTreeSet<String> = {
        let a = s.auto.lock().unwrap();
        a.results.keys().chain(a.held.keys()).cloned().collect()
    };
    let gone: Vec<String> = names
        .into_iter()
        .filter(|n| !s.updatable_stack(n))
        .collect();
    if !gone.is_empty() {
        s.auto_change(|a| {
            for n in &gone {
                a.results.remove(n);
                a.held.remove(n);
            }
        });
    }
}

// ---------------------------------------------------------------------------------------------
// the schedule
// ---------------------------------------------------------------------------------------------

/// Minutes since the window started (`0..1440`), the window starting at `start` (minute of
/// the day) and `now` a minute of the day.
pub fn minutes_into(now: u32, start: u32) -> u32 {
    (now % 1440 + 1440 - start % 1440) % 1440
}

pub fn in_window(now: u32, start: u32, len: u32) -> bool {
    minutes_into(now, start) < len
}


fn local_now() -> jiff::Zoned {
    jiff::Zoned::now()
}

fn minute_of(z: &jiff::Zoned) -> u32 {
    z.hour() as u32 * 60 + z.minute() as u32
}

/// The local day a window containing `z` started on.
pub fn window_day(z: &jiff::Zoned, start: u32) -> String {
    let into = minutes_into(minute_of(z), start);
    let began = z
        .checked_sub(jiff::SignedDuration::from_mins(i64::from(into)))
        .unwrap_or_else(|_| z.clone());
    began.date().to_string()
}

/// Milliseconds of the next window start (today's when it has not run yet).
pub fn next_window_ms(set: &AutoUpdateSettings, last: Option<&str>) -> u64 {
    let z = local_now();
    let start = u32::from(set.hour) * 60 + u32::from(set.minute);
    let now = minute_of(&z);
    let into = minutes_into(now, start);
    let ran = last == Some(window_day(&z, start).as_str());
    let wait_min = if in_window(now, start, u32::from(set.window_min)) && !ran {
        0
    } else if into == 0 {
        1440
    } else {
        1440 - into
    };
    let at = z
        .checked_add(jiff::SignedDuration::from_mins(i64::from(wait_min)))
        .unwrap_or(z);
    at.timestamp().as_millisecond().max(0) as u64 - u64::from(at.second() as u8) * 1000
}

/// Task kinds (prefixes) that keep automatic updates from starting: storage work (the manual
/// scrub is `storage.scrub`, the scheduled one `btrfs.scrub`), rollbacks and restores,
/// replication, the NAS backups (`backup.run`, `backup.check`, `backup.restore`), the
/// maintenance of the PC backup repositories, system updates, moves and config applies.
const MAINTENANCE_KINDS: &[&str] = &[
    "storage.", "btrfs.scrub", "snapshot.rollback", "snapshot.restore", "replication.", "backup.",
    "pcbackup.maintenance", "pcbackup.prune", "updates.install", "docker.install", "docker.migrate",
    "ids.remap", "homes.move", "power.", "config.apply", "crypt.",
];

fn is_maintenance(kind: &str) -> bool {
    MAINTENANCE_KINDS.iter().any(|k| kind.starts_with(k))
}

/// What keeps automatic updates from starting now: a question to the scheduler's lock table
/// ([`crate::claims::docker_busy`]: other package work, maintenance, heavy storage work
/// scheduled or started by hand, an md operation, a shutdown), plus the maintenance task
/// types of [`MAINTENANCE_KINDS`] the table does not know by name.
pub fn maintenance(s: &DockerSvc) -> Option<String> {
    let mdstat = if s.dry { String::new() } else { std::fs::read_to_string("/proc/mdstat").unwrap_or_default() };
    let gate = crate::gate::running();
    let table = crate::claims::table(gate.as_deref(), &s.ops.tasks, &mdstat, &[]);
    if let Some(why) = crate::claims::docker_busy(&table) {
        return Some(why);
    }
    s.ops
        .tasks
        .list(200)
        .into_iter()
        .find(|t| matches!(t.status, TaskStatus::Running | TaskStatus::Queued) && is_maintenance(&t.kind))
        .map(|t| format!("task {} is running", t.kind))
}

async fn wait_finished(s: &DockerSvc, id: &str, deadline: Duration) -> Waited {
    wait_task(
        || s.ops.tasks.get(id),
        || {
            let _ = s.ops.tasks.cancel(id);
        },
        deadline,
        CANCEL_GRACE,
        Duration::from_secs(2),
    )
    .await
}

/// How a waited task ended.
#[derive(Debug)]
enum Waited {
    Finished(rynas_api_types::TaskInfo),
    /// No longer in the task list.
    Gone,
    /// Past its deadline, cancelled, and still not finished after the grace time.
    TimedOut,
}

/// Waits for a task (`get`) until `deadline`; then `cancel`s it and gives it `grace` to end
/// (its own failure path records the outcome) before giving up on it.
async fn wait_task(
    get: impl Fn() -> Option<rynas_api_types::TaskInfo>,
    cancel: impl Fn(),
    deadline: Duration,
    grace: Duration,
    poll: Duration,
) -> Waited {
    let t0 = tokio::time::Instant::now();
    let mut until = t0 + deadline;
    let mut cancelled = false;
    loop {
        match get() {
            Some(t) if t.status.is_finished() => return Waited::Finished(t),
            None => return Waited::Gone,
            _ => {}
        }
        if tokio::time::Instant::now() >= until {
            if cancelled {
                return Waited::TimedOut;
            }
            cancel();
            cancelled = true;
            until = tokio::time::Instant::now() + grace;
        }
        tokio::time::sleep(poll).await;
    }
}

/// Starts the scheduled check (task `docker.update-check` by `rynas`, requests spread out)
/// and waits for it.
pub async fn scheduled_check(s: &Arc<DockerSvc>) -> bool {
    if s.engine().await.is_err() {
        return false;
    }
    let s2 = s.clone();
    match s.ops.tasks.spawn("docker.update-check", "rynas", move |ctx| async move {
        crate::docker::check_updates(&s2, &ctx, true).await
    }) {
        Ok(id) => match wait_finished(s, &id, CHECK_DEADLINE).await {
            Waited::Finished(t) => t.status == TaskStatus::Ok,
            Waited::Gone => false,
            Waited::TimedOut => {
                tracing::warn!("docker: the scheduled update check did not end in time");
                false
            }
        },
        Err(e) => {
            tracing::warn!("docker: scheduled update check not started: {e}");
            false
        }
    }
}

/// The Docker service of this rynasd, for the scheduled jobs.
fn docker_of(p: &crate::protection::Protection) -> Result<Arc<DockerSvc>, String> {
    p.ops
        .docker
        .get()
        .cloned()
        .ok_or_else(|| "Docker is not enabled in this rynasd".to_string())
}

/// The scheduled image check (scheduler task `docker-update-check`: five minutes after boot
/// at the earliest, then every 24 hours plus up to an hour; before S3 a loop of rynasd): the
/// task `docker.update-check` with the registry requests spread out. Without Docker there is
/// nothing to check (a success); an engine that does not answer fails the run, which is
/// tried again every hour (the task's retry).
pub fn spawn_image_check(
    p: &Arc<crate::protection::Protection>,
    t: &rynas_api_types::scheduler::ScheduledTask,
    who: &str,
) -> Result<String, String> {
    let s = docker_of(p)?;
    let _ = t;
    let s2 = s.clone();
    s.ops
        .tasks
        .spawn("docker.update-check", who, move |ctx| async move {
            if let Err(e) = s2.engine().await {
                if e.1 == rynas_api_types::error_codes::DOCKER_NOT_INSTALLED {
                    ctx.log("Docker is not installed: nothing to check");
                    return Ok(());
                }
                return Err(format!("Docker does not answer: {}", e.2));
            }
            // the next check is set by the scheduler's watch loop (the time it starts it at)
            let r = crate::docker::check_updates(&s2, &ctx, true).await;
            s2.changed();
            r
        })
        .map_err(|e| e.to_string())
}

/// The scheduled window of automatic updates (scheduler task `docker-auto-update`, at the
/// window's start plus a random delay in its first third; before S3 a loop of rynasd). The
/// task `docker.auto-window` waits while rynas maintenance runs, then updates the allowed
/// stacks one at a time (each its own task) until the window ends. A start after the window
/// (a catch-up after boot) is skipped by the gate; one inside it runs what is left of it.
pub fn spawn_auto_window(
    p: &Arc<crate::protection::Protection>,
    t: &rynas_api_types::scheduler::ScheduledTask,
    who: &str,
) -> Result<String, String> {
    let s = docker_of(p)?;
    let task = t.clone();
    let s2 = s.clone();
    s.ops
        .tasks
        .spawn("docker.auto-window", who, move |ctx| async move {
            let set = s2.settings().auto_update;
            if !set.stacks.values().any(|x| x.enabled) {
                ctx.log("no stack takes automatic updates");
                return Ok(());
            }
            let now = rynas_tasks::now_ms();
            let Some(end) = window_end(&task, now) else {
                ctx.log("outside the update window: nothing started");
                return Ok(());
            };
            let day = window_day(&local_now(), u32::from(set.hour) * 60 + u32::from(set.minute));
            if s2.auto.lock().unwrap().last_window.as_deref() == Some(day.as_str()) {
                ctx.log(&format!("the window of {day} already ran"));
                return Ok(());
            }
            loop {
                if ctx.is_cancelled() {
                    return Err("cancelled".into());
                }
                match maintenance(&s2) {
                    None => break,
                    Some(why) => {
                        if rynas_tasks::now_ms() >= end {
                            ctx.log(&format!("the window ended while rynas maintenance ran ({why}): nothing started"));
                            return Ok(());
                        }
                        ctx.log(&format!("automatic updates wait: {why}"));
                        tokio::time::sleep(TICK).await;
                    }
                }
            }
            s2.auto_change(|a| a.last_window = Some(day.clone()));
            ctx.log(&format!(
                "update window until {}",
                jiff::Timestamp::from_millisecond(end as i64).map(|t| t.to_string()).unwrap_or_default()
            ));
            run_window(&s2, &ctx, end).await;
            s2.changed();
            if ctx.is_cancelled() {
                return Err("cancelled".into());
            }
            Ok(())
        })
        .map_err(|e| e.to_string())
}

/// The end of the window the latest scheduled time of `t` opened, when `now_ms` is inside it.
pub fn window_end(t: &rynas_api_types::scheduler::ScheduledTask, now_ms: u64) -> Option<u64> {
    let min = u64::from(t.effective_policy().window_min);
    let expr = t.trigger.on_calendar()?;
    let tz = jiff::tz::TimeZone::system();
    let spec = rynas_schedule::CalendarSpec::parse(&expr).ok()?;
    let from = jiff::Timestamp::from_millisecond(now_ms.saturating_sub(min * MIN) as i64).ok()?;
    let mut slot = spec.next_after(&tz, from)?;
    let mut last = None;
    while slot.as_millisecond() as u64 <= now_ms {
        last = Some(slot.as_millisecond() as u64);
        slot = spec.next_after(&tz, slot)?;
    }
    if let Some(e) = last.map(|s| s + min * MIN).filter(|e| *e > now_ms) {
        return Some(e);
    }
    // the window's time did not exist today (spring forward): its civil window, as the loop
    // read the clock (02:30 for 90 minutes: from the jump to 04:00)
    let (h, m) = rynas_scheduler::window::daily_time(&expr)?;
    if !rynas_scheduler::window::gap_on_day(&tz, now_ms, h, m) {
        return None;
    }
    let local = jiff::Timestamp::from_millisecond(now_ms as i64).ok()?.to_zoned(tz.clone());
    if u32::from(local.hour() as u8) * 60 + u32::from(local.minute() as u8) < u32::from(h) * 60 + u32::from(m) {
        return None;
    }
    let today = local.date();
    let end = today
        .at(h as i8, m as i8, 0, 0)
        .checked_add(jiff::SignedDuration::from_mins(min as i64))
        .ok()?;
    let end = tz.to_ambiguous_zoned(end).compatible().ok()?.timestamp().as_millisecond() as u64;
    (end > now_ms).then_some(end)
}

/// The window: the allowed updates of the stacks that take them, one at a time. Cancelling
/// the window's task (`ctx`) lets the update in hand end and starts no other.
pub async fn run_window(s: &Arc<DockerSvc>, ctx: &TaskCtx, end_ms: u64) {
    let stale = rynas_tasks::now_ms().saturating_sub(s.report.lock().unwrap().checked_at_ms) > STALE_REPORT;
    if stale && !scheduled_check(s).await {
        tracing::warn!("docker: automatic updates skipped: the update check failed");
        return;
    }
    prune_gone(s);
    let report = enrich(s, s.report.lock().unwrap().clone());
    let todo: Vec<StackUpdate> = report
        .stacks
        .into_iter()
        .filter(|u| u.enabled && matches!(u.verdict, UpdateVerdict::Auto | UpdateVerdict::AfterPull))
        .collect();
    for u in todo {
        if ctx.is_cancelled() {
            tracing::info!("docker: the update window was cancelled before {}", u.stack);
            break;
        }
        if rynas_tasks::now_ms() >= end_ms {
            tracing::info!("docker: the update window ended before {}", u.stack);
            break;
        }
        if let Some(why) = maintenance(s) {
            tracing::info!("docker: automatic updates stop: {why}");
            break;
        }
        match start_auto(s, &u, "rynas") {
            Ok(id) => {
                if let Waited::TimedOut = wait_finished(s, &id, STACK_DEADLINE).await {
                    // the task hangs past its cancellation: record it here and go on
                    let detail = format!(
                        "the update did not end within {} minutes and was canceled",
                        STACK_DEADLINE.as_secs() / 60
                    );
                    tracing::warn!("docker: automatic update of {}: {detail}", u.stack);
                    s.notifier.emit(auto_event(AutoOutcome::Failed, &u.stack, Some(&detail), u.change.as_deref()));
                    s.record_auto(AutoResult {
                        stack: u.stack.clone(),
                        at_ms: rynas_tasks::now_ms(),
                        outcome: AutoOutcome::Failed,
                        task_id: Some(id),
                        detail: Some(detail),
                    });
                }
            }
            Err(e) => tracing::warn!("docker: automatic update of {} not started: {e}", u.stack),
        }
    }
}

/// Starts the automatic update of one stack as a task.
pub fn start_auto(s: &Arc<DockerSvc>, u: &StackUpdate, who: &str) -> Result<String, String> {
    let guard = mark_busy(s, &u.stack).map_err(|e| e.2)?;
    let (s2, u2) = (s.clone(), u.clone());
    let kind = if u.app.is_some() { "apps.auto-update" } else { "docker.auto-update" };
    audit(who, kind, &u.stack);
    s.ops
        .tasks
        .spawn(kind, who, move |ctx| async move {
            let _guard = guard;
            let stack = u2.stack.clone();
            let (outcome, detail) = if u2.app.is_some() {
                app_update(&s2, &ctx, &u2).await
            } else {
                stack_update(&s2, &ctx, &u2).await
            };
            ctx.log(&format!("result: {outcome:?}{}", detail.as_deref().map(|d| format!(": {d}")).unwrap_or_default()));
            s2.notifier.emit(auto_event(outcome, &stack, detail.as_deref(), u2.change.as_deref()));
            s2.record_auto(AutoResult {
                stack,
                at_ms: rynas_tasks::now_ms(),
                outcome,
                task_id: Some(ctx.id().to_string()),
                detail: detail.clone(),
            });
            match outcome {
                AutoOutcome::Updated | AutoOutcome::Held => Ok(()),
                _ => Err(detail.unwrap_or_else(|| "the update failed".into())),
            }
        })
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------------------------
// one stack
// ---------------------------------------------------------------------------------------------

/// The readings of the stack's containers (by name); `None` when the engine does not answer.
async fn readings(s: &DockerSvc, stack: &str) -> Option<BTreeMap<String, health::Sample>> {
    let e = s.engine().await.ok()?;
    let cs = e.containers().await.ok()?;
    let mut out = BTreeMap::new();
    for c in cs.iter().filter(|c| c.c.stack.as_deref() == Some(stack)) {
        if let Ok(x) = e.container_health(&c.c.id).await {
            out.insert(c.c.name.clone(), x);
        }
    }
    Some(out)
}

/// Waits until the stack is healthy or fails (or the task is cancelled).
pub async fn health_check(s: &DockerSvc, ctx: &TaskCtx, stack: &str) -> Result<(), String> {
    ctx.log(&format!(
        "health check: healthy containers, or running without a restart for {} s",
        health::STABLE_S
    ));
    let t0 = std::time::Instant::now();
    let first = readings(s, stack)
        .await
        .ok_or("Docker does not answer: the health of the stack cannot be checked")?;
    loop {
        if ctx.is_cancelled() {
            return Err("cancelled".into());
        }
        let now = readings(s, stack)
            .await
            .ok_or("Docker stopped answering during the health check")?;
        match health::judge(&first, &now, t0.elapsed().as_secs()) {
            health::Judgement::Healthy => {
                ctx.log("the stack is healthy");
                return Ok(());
            }
            health::Judgement::Failed(why) => return Err(why),
            health::Judgement::Wait => tokio::time::sleep(HEALTH_POLL).await,
        }
    }
}

/// Puts the images the stack ran back on their references (no snapshot to roll back to).
async fn retag(s: &DockerSvc, ctx: &TaskCtx, before: &[ServiceImage]) -> Result<(), String> {
    for i in before.iter().filter(|i| !i.image_id.is_empty()) {
        let c = Cmd::new("docker")
            .args(["tag", i.image_id.as_str(), i.image.as_str()])
            .env_clear()
            .timeout(Duration::from_secs(60));
        let out = s.run(ctx, &c).await?;
        if !out.success() {
            return Err(format!("docker tag {} {}: {}", i.image_id, i.image, out.stderr.trim()));
        }
    }
    Ok(())
}

async fn label_of(s: &DockerSvc, image: &str) -> Option<String> {
    let e = s.engine().await.ok()?;
    e.image_label(image, VERSION_LABEL).await.ok().flatten()
}

/// A compose stack: pull, label check where needed, snapshot, up, health, rollback.
pub async fn stack_update(s: &Arc<DockerSvc>, ctx: &TaskCtx, u: &StackUpdate) -> (AutoOutcome, Option<String>) {
    let l: Layout = match s.managed(&u.stack).await {
        Ok(l) => l,
        Err(e) => return (AutoOutcome::Failed, Some(e.2)),
    };
    let set = s.settings();
    let own = set.auto_update.of(&u.stack);
    ctx.log(&format!(
        "automatic update of {} ({}): {}",
        u.stack,
        own.policy.map(|p| p.as_str()).unwrap_or("policy by tag"),
        u.images.join(", ")
    ));
    let before = current_images(s, &u.stack).await;
    let mut old_labels = BTreeMap::new();
    for i in &before {
        old_labels.insert(i.image.clone(), label_of(s, &i.image_id).await);
    }
    // only the services whose images the policy judged: another floating image of the stack
    // (its check failed: rate limit, private registry) is neither pulled nor recreated
    let config = match stack_services(s, ctx, &u.stack, &l).await {
        Ok(c) => c,
        Err(e) => return (AutoOutcome::Failed, Some(e)),
    };
    let services = services_of(&config, &before, &u.images);
    if services.is_empty() {
        return (AutoOutcome::Failed, Some("no service of the stack uses the updated images".into()));
    }
    ctx.log(&format!("services to update: {}", services.join(", ")));
    ctx.log("step 1/4: download the new images");
    if let Err(e) = compose_for(s, ctx, &u.stack, &l, ComposeAction::Pull, &services).await {
        // the stack still runs the old images: nothing to roll back
        return (AutoOutcome::Failed, Some(format!("download failed: {e}")));
    }
    ctx.progress(25);
    // floating and short tags: the version labels decide
    let report = s.report.lock().unwrap().clone();
    let mut verdict = Verdict::Auto;
    for img in &u.images {
        let tag = tag_of(img).unwrap_or_default();
        let pol = own.policy.unwrap_or_else(|| policy::default_policy(&tag));
        if policy::judge_digest(pol, &tag) == Verdict::AfterPull {
            let old = old_labels.get(img).cloned().flatten();
            let new = label_of(s, img).await;
            let v = policy::judge_labels(pol, old.as_deref(), new.as_deref());
            ctx.log(&format!(
                "{img}: version {} → {}: {}",
                old.as_deref().unwrap_or("unknown"),
                new.as_deref().unwrap_or("unknown"),
                match v {
                    Verdict::Auto => "allowed".to_string(),
                    Verdict::Hold(r) => format!("not allowed ({r:?})"),
                    Verdict::AfterPull => "undecided".to_string(),
                }
            ));
            verdict = verdict.and(v);
        }
    }
    if let Verdict::Hold(reason) = verdict {
        ctx.log("the new version is outside the policy: the previous images are put back, nothing else changes");
        if let Err(e) = retag(s, ctx, &before).await {
            ctx.log(&format!("warning: {e}"));
        }
        let digests: BTreeSet<String> = report
            .images
            .iter()
            .filter(|i| u.images.contains(&i.image))
            .filter_map(|i| i.remote_digest.clone())
            .collect();
        s.auto_change(|a| {
            a.held.insert(
                u.stack.clone(),
                Held {
                    digests,
                    reason: Some(reason),
                },
            );
        });
        return (AutoOutcome::Held, Some(format!("the new version is outside the policy ({reason:?})")));
    }
    ctx.log("step 2/4: snapshot");
    let snap = if set.snapshot_before_update {
        match pre_snapshot(s, ctx, &u.stack, &l, "update", set.stop_for_snapshot).await {
            Ok(sn) => sn,
            Err(e) => {
                let _ = retag(s, ctx, &before).await;
                return (AutoOutcome::Failed, Some(format!("snapshot failed: {e}")));
            }
        }
    } else {
        ctx.log("no snapshot (turned off in the Docker settings): a failure puts the previous images back");
        None
    };
    ctx.progress(45);
    ctx.log("step 3/4: start the new version");
    let mut r = compose_for(s, ctx, &u.stack, &l, ComposeAction::Up, &services).await;
    ctx.progress(60);
    if r.is_ok() {
        ctx.log("step 4/4: health check");
        r = health_check(s, ctx, &u.stack).await;
    }
    let Err(e) = r else {
        s.auto_change(|a| {
            a.held.remove(&u.stack);
        });
        s.resolve_updates(&u.images);
        return (AutoOutcome::Updated, None);
    };
    ctx.log(&format!("the update failed ({e}): rolling back"));
    let back = match &snap {
        Some(sn) => do_rollback(s, ctx, &u.stack, &l, &sn.id).await,
        None => match retag(s, ctx, &before).await {
            Ok(()) => compose_for(s, ctx, &u.stack, &l, ComposeAction::Up, &services).await,
            Err(e2) => Err(e2),
        },
    };
    // the version that failed is not tried again every night: the stack waits for a newer
    // one (other digests), or for an update by hand
    let digests: BTreeSet<String> = report
        .images
        .iter()
        .filter(|i| u.images.contains(&i.image))
        .filter_map(|i| i.remote_digest.clone())
        .collect();
    if !digests.is_empty() {
        s.auto_change(|a| {
            a.held.insert(
                u.stack.clone(),
                Held {
                    digests,
                    reason: Some(HoldReason::Failed),
                },
            );
        });
    }
    match back {
        Ok(()) => (AutoOutcome::RolledBack, Some(e)),
        Err(e2) => (AutoOutcome::Failed, Some(format!("{e}; the rollback failed too: {e2}"))),
    }
}

/// The services of a stack that use one of `images` (the ones the policy judged), sorted:
/// from the compose model, and from the running containers (the reference they run).
fn services_of(config: &[(String, Option<String>)], before: &[ServiceImage], images: &[String]) -> Vec<String> {
    let set: BTreeSet<String> = config
        .iter()
        .filter(|(_, image)| image.as_ref().is_some_and(|i| images.contains(i)))
        .map(|(service, _)| service.clone())
        .chain(before.iter().filter(|i| images.contains(&i.image)).map(|i| i.service.clone()))
        .collect();
    set.into_iter().collect()
}

/// A catalogue app: the app update flow (snapshot, new files, pull, up; it rolls back by
/// itself when pull or up fails), then the health check and a rollback when it fails.
pub async fn app_update(s: &Arc<DockerSvc>, ctx: &TaskCtx, u: &StackUpdate) -> (AutoOutcome, Option<String>) {
    let Some(apps) = s.apps.get().and_then(|w| w.upgrade()) else {
        return (AutoOutcome::Failed, Some("the app catalog is not available".into()));
    };
    ctx.log(&format!(
        "automatic update of app {} ({})",
        u.stack,
        u.change.as_deref().unwrap_or("new catalog entry")
    ));
    let (job, plan): (crate::apps::UpdateJob, AppUpdatePlan) =
        match apps.prepare_update_job(&u.stack, &AppUpdateRequest::default()).await {
            Ok(x) => x,
            Err(e) => return (AutoOutcome::Failed, Some(e.2)),
        };
    // the plan may have moved since the check: judge it again
    let cand = AppCandidate {
        stack: u.stack.clone(),
        policy: s.auto.lock().unwrap().apps.iter().find(|a| a.stack == u.stack).and_then(|a| a.policy),
        main_service: s
            .auto
            .lock()
            .unwrap()
            .apps
            .iter()
            .find(|a| a.stack == u.stack)
            .map(|a| a.main_service.clone())
            .unwrap_or_default(),
        plan,
    };
    let (v, _) = judge_app(&cand, s.settings().auto_update.of(&u.stack).policy);
    if let Verdict::Hold(r) = v {
        return (AutoOutcome::Held, Some(format!("the catalog update is outside the policy ({r:?})")));
    }
    let (layout, old) = (job.layout.clone(), job.old.clone());
    let snap = match crate::apps::update_task(s, ctx, job).await {
        Ok(sn) => sn,
        Err(e) if e.contains("rolled back") || e.contains("restored") => {
            return (AutoOutcome::RolledBack, Some(e));
        }
        Err(e) => return (AutoOutcome::Failed, Some(e)),
    };
    let Err(e) = health_check(s, ctx, &u.stack).await else {
        s.auto_change(|a| a.apps.retain(|x| x.stack != u.stack));
        return (AutoOutcome::Updated, None);
    };
    ctx.log(&format!("the new version is not healthy ({e}): rolling back"));
    let back = match &snap {
        Some(id) => do_rollback(s, ctx, &u.stack, &layout, id).await,
        None => match crate::apps::write_stack(s, &layout, &u.stack, &old).await {
            Ok(()) => compose(s, ctx, &u.stack, &layout, ComposeAction::Up).await,
            Err(e2) => Err(e2),
        },
    };
    // the catalogue entry that failed is not tried again every night (see `stack_updates`)
    if let Some(change) = &u.change {
        s.auto_change(|a| {
            a.held.insert(
                u.stack.clone(),
                Held {
                    digests: [app_hold_key(change)].into(),
                    reason: Some(HoldReason::Failed),
                },
            );
        });
    }
    match back {
        Ok(()) => (AutoOutcome::RolledBack, Some(e)),
        Err(e2) => (AutoOutcome::Failed, Some(format!("{e}; the rollback failed too: {e2}"))),
    }
}

/// The notification of an automatic update.
pub fn auto_event(o: AutoOutcome, stack: &str, detail: Option<&str>, change: Option<&str>) -> NewEvent {
    let key = format!("docker:auto:{stack}");
    let (sev, c, title, body) = match o {
        AutoOutcome::Updated => (
            NotifySeverity::Info,
            "docker.auto_updated",
            format!("Stack {stack} updated automatically"),
            change.map(|c| format!("New version: {c}.")).unwrap_or_else(|| "The new images run and the stack is healthy.".into()),
        ),
        AutoOutcome::RolledBack => (
            NotifySeverity::Warning,
            "docker.auto_rolled_back",
            format!("Automatic update of {stack} rolled back"),
            format!("The new version failed: {}. The stack runs the previous version again.", detail.unwrap_or("unknown reason")),
        ),
        AutoOutcome::Held => (
            NotifySeverity::Info,
            "docker.auto_held",
            format!("Update of {stack} needs you"),
            format!("{}. Update it by hand from the Docker page.", detail.unwrap_or("The new version is outside the policy")),
        ),
        AutoOutcome::Failed => (
            NotifySeverity::Error,
            "docker.auto_failed",
            format!("Automatic update of {stack} failed"),
            format!("{}. Look at the task log on the Docker page.", detail.unwrap_or("unknown reason")),
        ),
    };
    let mut ev = oneshot(NotifyCategory::Update, sev, "docker", key, title);
    ev.body = body;
    let ev = field(code(ev, c), "stack", stack);
    let ev = field(ev, "reason", detail.unwrap_or(""));
    field(ev, "change", change.unwrap_or(""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::apps::ImageChange;

    fn img(image: &str, stack: &str, remote: &str) -> ImageUpdate {
        ImageUpdate {
            image: image.into(),
            local_digest: Some("sha256:old".into()),
            remote_digest: Some(remote.into()),
            update_available: true,
            stacks: vec![stack.into()],
            ..Default::default()
        }
    }

    #[test]
    fn maintenance_kinds() {
        // the scheduled scrub, the NAS backups and the PC repositories' maintenance (I-6)
        for k in ["btrfs.scrub", "storage.scrub", "backup.run", "backup.check", "backup.restore", "pcbackup.maintenance", "pcbackup.prune", "replication.run", "updates.install"] {
            assert!(is_maintenance(k), "{k}");
        }
        for k in ["snapshot.run", "docker.update-check", "pcbackup.usage", "smb.recycle-cleanup", "apps.install"] {
            assert!(!is_maintenance(k), "{k}");
        }
    }

    fn app(stack: &str, policy: Option<UpdatePolicy>, from: &str, to: &str) -> AppCandidate {
        AppCandidate {
            stack: stack.into(),
            policy,
            main_service: "homeassistant".into(),
            plan: AppUpdatePlan {
                stack: stack.into(),
                app: "home-assistant".into(),
                from_version: "1.0.0".into(),
                to_version: "1.0.1".into(),
                from_app_version: from.into(),
                to_app_version: to.into(),
                image_changes: vec![ImageChange {
                    service: "homeassistant".into(),
                    kind: ImageChangeKind::Tag,
                    from: Some(format!("ghcr.io/home-assistant/home-assistant:{from}@sha256:aa")),
                    to: Some(format!("ghcr.io/home-assistant/home-assistant:{to}@sha256:bb")),
                }],
                ..Default::default()
            },
        }
    }

    #[test]
    fn windows() {
        // 03:00 for 60 minutes
        assert!(in_window(180, 180, 60) && in_window(239, 180, 60) && !in_window(240, 180, 60));
        assert!(!in_window(179, 180, 60));
        // across midnight: 23:30 for 90 minutes
        assert!(in_window(23 * 60 + 45, 23 * 60 + 30, 90) && in_window(30, 23 * 60 + 30, 90));
        assert!(!in_window(61, 23 * 60 + 30, 90));
        assert_eq!(minutes_into(10, 1430), 20);
    }

    #[test]
    fn window_of_the_scheduled_task() {
        let set = AutoUpdateSettings { hour: 4, minute: 30, window_min: 90, ..Default::default() };
        let t = rynas_api_types::scheduler::system::docker_auto_update(&set);
        let at = |h: i8, m: i8| {
            let z = jiff::Zoned::now().date().at(h, m, 0, 0).to_zoned(jiff::tz::TimeZone::system()).unwrap();
            z.timestamp().as_millisecond() as u64
        };
        // inside: the window opened at 04:30 ends at 06:00
        assert_eq!(window_end(&t, at(4, 50)), Some(at(6, 0)));
        assert_eq!(window_end(&t, at(5, 59)), Some(at(6, 0)));
        // before and after it
        assert_eq!(window_end(&t, at(4, 29)), None);
        assert_eq!(window_end(&t, at(6, 0)), None);
    }

    #[test]
    fn verdicts_of_stacks() {
        let report = UpdateReport {
            checked_at_ms: 1,
            images: vec![
                img("nginx:1.27", "web", "sha256:n"),
                img("postgres:16", "db", "sha256:p"),
                img("app/x:latest", "blog", "sha256:x"),
                img("app/y:sha-123", "odd", "sha256:y"),
                img("redis:7.2.4", "foreign", "sha256:r"),
            ],
            ..Default::default()
        };
        let mut set = AutoUpdateSettings::default();
        set.stacks.insert("web".into(), StackAutoUpdate { enabled: true, policy: None });
        set.stacks.insert("db".into(), StackAutoUpdate { enabled: true, policy: Some(UpdatePolicy::Patch) });
        let mut held = BTreeMap::new();
        let managed = |n: &str| n != "foreign";
        let v = stack_updates(&report, &set, &[], &held, &managed);
        let get = |n: &str| v.iter().find(|u| u.stack == n).unwrap().clone();
        assert_eq!((get("web").verdict, get("web").enabled), (UpdateVerdict::Auto, true));
        assert_eq!(get("db").verdict, UpdateVerdict::AfterPull);
        assert_eq!(get("blog").verdict, UpdateVerdict::AfterPull);
        assert!(!get("blog").enabled);
        assert_eq!((get("odd").verdict, get("odd").reason), (UpdateVerdict::Hold, Some(HoldReason::Policy)));
        assert_eq!(get("foreign").reason, Some(HoldReason::NotManaged));
        // a download already held for these digests stays held
        held.insert("blog".into(), Held { digests: ["sha256:x".to_string()].into(), reason: Some(HoldReason::Major) });
        let v = stack_updates(&report, &set, &[], &held, &managed);
        assert_eq!(v.iter().find(|u| u.stack == "blog").unwrap().reason, Some(HoldReason::Major));
    }

    #[test]
    fn catalogue_apps_follow_their_policy() {
        let set = AutoUpdateSettings::default();
        let a = app("ha", Some(UpdatePolicy::CalverPatch), "2026.11.0", "2026.11.1");
        assert_eq!(judge_app(&a, None).0, Verdict::Auto);
        let b = app("ha", Some(UpdatePolicy::CalverPatch), "2026.11.3", "2026.12.0");
        assert_eq!(judge_app(&b, None).0, Verdict::Hold(HoldReason::Month));
        // the stack's own choice wins
        assert_eq!(judge_app(&b, Some(UpdatePolicy::Minor)).0, Verdict::Auto);
        assert_eq!(judge_app(&a, Some(UpdatePolicy::Notify)).0, Verdict::Hold(HoldReason::Policy));
        let mut c = a.clone();
        c.plan.missing_values = vec!["admin_email".into()];
        assert_eq!(judge_app(&c, None).0, Verdict::Hold(HoldReason::NeedsReview));
        let mut d = a.clone();
        d.plan.to_version = "2.0.0".into();
        assert_eq!(judge_app(&d, None).0, Verdict::Hold(HoldReason::NeedsReview));
        // the app's pinned image is not judged twice through the digest check
        let report = UpdateReport {
            images: vec![img("ghcr.io/home-assistant/home-assistant:2026.11.0", "ha", "sha256:z")],
            ..Default::default()
        };
        let v = stack_updates(&report, &set, &[b], &BTreeMap::new(), &|_| true);
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].app.as_deref(), v[0].reason), (Some("home-assistant"), Some(HoldReason::Month)));
        assert_eq!(v[0].policy, Some(UpdatePolicy::CalverPatch));
        assert_eq!(v[0].change.as_deref(), Some("2026.11.3 → 2026.12.0"));
    }

    #[test]
    fn notifications() {
        let e = auto_event(AutoOutcome::RolledBack, "web", Some("container web is unhealthy"), None);
        assert_eq!(e.code, "docker.auto_rolled_back");
        assert_eq!(e.fields["reason"], "container web is unhealthy");
        assert_eq!(auto_event(AutoOutcome::Updated, "web", None, Some("1 → 2")).severity, NotifySeverity::Info);
        assert_eq!(tag_of("x/y:1.2@sha256:ab").as_deref(), Some("1.2"));
    }

    #[test]
    fn only_the_judged_services_are_updated() {
        let si = |service: &str, image: &str| ServiceImage {
            service: service.into(),
            image: image.into(),
            image_id: format!("sha256:{service}"),
            digest: None,
        };
        let before = vec![
            si("web", "nginx:1.27"),
            si("worker", "nginx:1.27"),
            // a floating image whose check failed: not in the judged list
            si("db", "postgres:latest"),
            si("cache", "redis:7"),
        ];
        let judged = vec!["nginx:1.27".to_string(), "redis:7".to_string()];
        assert_eq!(services_of(&[], &before, &judged), vec!["cache", "web", "worker"]);
        assert!(services_of(&[], &before, &["other:1".to_string()]).is_empty());
        // services from the compose model (also the ones not running now)
        let config = vec![
            ("web".to_string(), Some("nginx:1.27".to_string())),
            ("db".to_string(), Some("postgres:latest".to_string())),
            ("proxy".to_string(), Some("nginx:1.27".to_string())),
            ("built".to_string(), None),
        ];
        assert_eq!(services_of(&config, &[], &judged), vec!["proxy", "web"]);
        assert_eq!(services_of(&config, &before, &judged), vec!["cache", "proxy", "web", "worker"]);
    }

    #[tokio::test]
    async fn a_hung_task_is_cancelled_and_given_up() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let running = rynas_api_types::TaskInfo {
            status: TaskStatus::Running,
            ..Default::default()
        };
        let ms = Duration::from_millis;
        // never ends, not even after the cancellation
        let cancelled = AtomicBool::new(false);
        let w = wait_task(|| Some(running.clone()), || cancelled.store(true, Ordering::Relaxed), ms(30), ms(30), ms(5)).await;
        assert!(matches!(w, Waited::TimedOut));
        assert!(cancelled.load(Ordering::Relaxed));
        // ends through its failure path once cancelled
        let cancelled = AtomicBool::new(false);
        let w = wait_task(
            || {
                Some(if cancelled.load(Ordering::Relaxed) {
                    rynas_api_types::TaskInfo {
                        status: TaskStatus::Cancelled,
                        ..Default::default()
                    }
                } else {
                    running.clone()
                })
            },
            || cancelled.store(true, Ordering::Relaxed),
            ms(30),
            ms(500),
            ms(5),
        )
        .await;
        assert!(matches!(w, Waited::Finished(t) if t.status == TaskStatus::Cancelled));
        // a task that ends in time is not cancelled
        let w = wait_task(
            || {
                Some(rynas_api_types::TaskInfo {
                    status: TaskStatus::Ok,
                    ..Default::default()
                })
            },
            || panic!("cancelled a finished task"),
            ms(30),
            ms(30),
            ms(5),
        )
        .await;
        assert!(matches!(w, Waited::Finished(_)));
    }
}
