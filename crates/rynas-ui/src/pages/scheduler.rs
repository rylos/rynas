//! Task Scheduler (D-TASKS; DSM Control Panel › Task Scheduler, OMV Scheduled Jobs): tasks
//! that rynasd turns into persistent systemd units, with next run, last result, enable/disable,
//! run now and the run history with its output (and the live output of the run going on now,
//! [`super::scheduler_live`]); the shared schedule editor ([`crate::schedule::ScheduleEditor`],
//! live preview of the next runs in the NAS's time zone); read-only views of the system timers
//! and crontab entries found on the NAS ("create a task" from a cron line). Writes stage
//! pending changes (`/config/apply` applies them).

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::PendingOp;
use rynas_api_types::scheduler::{
    Blocker, CronEntry, LiveRun, NotifyPolicy, OverviewOwner, PauseRequest, QueueWait, Resource, RunConflicts, RunNowRequest, RunTrigger, ScheduledTask, SchedulerPause, SkipReason,
    SmartTestKind, SystemTimer, TaskAction, TaskRun, TaskTrigger, TaskView, YieldInfo,
};

use rynas_schedule::describe::{all_day, day_span};
use rynas_schedule::{Description, MonthDay, Nth, Recurrence, TimeOfDay, Weekdays, describe};

use super::scheduler_live;
use crate::api;
use crate::form::{In, area_field, lens, lens_bool, number_field, save_bar, select_field, switch, tabs, text_field};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::schedule::ScheduleEditor;
use crate::toast::{saved_applied, saved_pending, toast};
use crate::ui::{self, Confirm, EmptyState, PageHeader, Remote, Tone, confirm, drawer};

// ------------------------------------------------------------------ schedules in words

/// Weekdays in words: "Mon–Fri", "Mon, Wed", "Sat, Sun".
pub fn weekdays_text(days: Weekdays) -> String {
    let d = days.days();
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < d.len() {
        let mut j = i;
        while j + 1 < d.len() && d[j + 1] == d[j] + 1 {
            j += 1;
        }
        if j >= i + 2 {
            parts.push(format!("{}–{}", t(DAY_KEYS[d[i] as usize]), t(DAY_KEYS[d[j] as usize])));
        } else {
            parts.extend(d[i..=j].iter().map(|x| t(DAY_KEYS[*x as usize]).to_string()));
        }
        i = j + 1;
    }
    parts.join(", ")
}

fn times_text(times: &[TimeOfDay]) -> String {
    times.iter().map(|x| i18n::hm(u32::from(x.hour), u32::from(x.minute))).collect::<Vec<_>>().join(", ")
}

const NTH_KEYS: [(Nth, &str); 5] =
    [(Nth::First, "sch.nth.first"), (Nth::Second, "sch.nth.second"), (Nth::Third, "sch.nth.third"), (Nth::Fourth, "sch.nth.fourth"), (Nth::Last, "sch.nth.last")];

/// A preset schedule in words ("Every 15 min, 08:00–18:45, Mon–Fri").
pub fn recurrence_text(r: &Recurrence) -> String {
    let hm = |x: TimeOfDay| i18n::hm(u32::from(x.hour), u32::from(x.minute));
    let within = |what: String, days: Weekdays| {
        let what = match day_span(r) {
            Some((a, b)) if !all_day(r) => tf("sch.desc.span", &[("what", &what), ("from", &hm(a)), ("to", &hm(b))]),
            _ => what,
        };
        if days.every_day() { what } else { tf("sch.desc.on_days", &[("what", &what), ("days", &weekdays_text(days))]) }
    };
    match r {
        Recurrence::Daily { days, times } if days.every_day() => tf("sch.desc.daily", &[("time", &times_text(times))]),
        Recurrence::Daily { days, times } => tf("sch.desc.weekly", &[("days", &weekdays_text(*days)), ("time", &times_text(times))]),
        Recurrence::Minutes { every, days, .. } => within(tf("sch.desc.minutes", &[("n", &every.to_string())]), *days),
        Recurrence::Hours { every: 1, minute, days, .. } => within(tf("sch.desc.hourly", &[("m", &format!("{minute:02}"))]), *days),
        Recurrence::Hours { every, minute, days, .. } => {
            within(tf("sch.desc.hours", &[("n", &every.to_string()), ("m", &format!("{minute:02}"))]), *days)
        }
        Recurrence::Monthly { day: MonthDay::Days(d), times } => {
            let d = d.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", ");
            tf("sch.desc.monthly", &[("d", &d), ("time", &times_text(times))])
        }
        Recurrence::Monthly { day: MonthDay::Last, times } => tf("sch.desc.monthly_last", &[("time", &times_text(times))]),
        Recurrence::MonthlyNth { nth, weekday, times } => {
            let n = NTH_KEYS.iter().find(|(k, _)| k == nth).map_or("", |(_, key)| t(key));
            tf("sch.desc.monthly_nth", &[("nth", n), ("day", t(DAY_KEYS[usize::from(*weekday % 7)])), ("time", &times_text(times))])
        }
        Recurrence::Yearly { month, day, times } => {
            tf("sch.desc.yearly", &[("date", &i18n::day_month(u32::from(*month), u32::from(*day))), ("time", &times_text(times))])
        }
    }
}

/// An `OnCalendar=` expression in words; the normalized expression when no preset shows it.
pub fn calendar_text(on_calendar: &str) -> String {
    match describe(on_calendar) {
        Description::Preset(r) => recurrence_text(&r),
        Description::Advanced { normalized } => normalized,
        Description::Invalid(_) => on_calendar.to_string(),
    }
}

/// A trigger in words ("Daily at 03:00", "At boot (+60 s)").
pub fn trigger_text(tr: &TaskTrigger) -> String {
    match tr {
        TaskTrigger::Manual => t("sch.trig.manual").to_string(),
        TaskTrigger::Boot { delay_s } => tf("sch.trig.boot_text", &[("s", &delay_s.to_string())]),
        TaskTrigger::Calendar { on_calendar, cron, .. } => {
            if on_calendar.is_empty() {
                return cron.clone().unwrap_or_default();
            }
            calendar_text(on_calendar)
        }
        TaskTrigger::Recurrence { recurrence } => recurrence_text(recurrence),
        TaskTrigger::Interval { hours, .. } => tf("sch.trig.interval_text", &[("n", &hours.to_string())]),
        TaskTrigger::Window { .. } => t("sch.trig.window_text").to_string(),
        TaskTrigger::After { task, .. } => tf("sch.trig.after_text", &[("task", task)]),
    }
}

pub const DAY_KEYS: [&str; 7] = ["day.1", "day.2", "day.3", "day.4", "day.5", "day.6", "day.7"];

// ------------------------------------------------------------------ action draft (pure)

/// Kinds of action a new task can have.
const ACTIONS: &[(&str, &str)] = &[
    ("command", "sch.act.command"),
    ("script", "sch.act.script"),
    ("scrub", "sch.act.scrub"),
    ("smart_test", "sch.act.smart_test"),
    ("recycle_cleanup", "sch.act.recycle_cleanup"),
];
const SMART: &[(&str, &str)] = &[("short", "sch.smart.short"), ("long", "sch.smart.long")];
const NOTIFY: &[(&str, &str)] = &[("on_failure", "sch.notify.on_failure"), ("always", "sch.notify.always"), ("never", "sch.notify.never")];

/// The editable parts of a task (text fields for every action kind).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ActionDraft {
    pub kind: String,
    pub command: String,
    pub script: String,
    pub user: String,
    pub filesystem: String,
    pub disk: String,
    pub test: String,
    pub share: String,
    pub days: String,
}

impl ActionDraft {
    pub fn from_action(a: &TaskAction) -> Self {
        let mut d = ActionDraft { user: "root".into(), test: "short".into(), ..Default::default() };
        match a {
            TaskAction::Command { command, user } => {
                d.kind = "command".into();
                d.command = command.clone();
                d.user = user.clone();
            }
            TaskAction::Script { script, user } => {
                d.kind = "script".into();
                d.script = script.clone();
                d.user = user.clone();
            }
            TaskAction::Scrub { filesystem } => {
                d.kind = "scrub".into();
                d.filesystem = filesystem.clone();
            }
            TaskAction::SmartTest { disk, test } => {
                d.kind = "smart_test".into();
                d.disk = disk.clone();
                d.test = if *test == SmartTestKind::Long { "long" } else { "short" }.into();
            }
            TaskAction::RecycleCleanup { share, days } => {
                d.kind = "recycle_cleanup".into();
                d.share = share.clone().unwrap_or_default();
                d.days = days.map(|d| d.to_string()).unwrap_or_default();
            }
            TaskAction::Snapshot { .. } => d.kind = "snapshot".into(),
            TaskAction::Replicate { .. } => d.kind = "replicate".into(),
            TaskAction::PowerShutdown { .. } => d.kind = "power_shutdown".into(),
            TaskAction::Backup { .. } => d.kind = "backup".into(),
            TaskAction::Dedup { .. } => d.kind = "dedup".into(),
            TaskAction::UpdatesCheck
            | TaskAction::SystemUpgrade
            | TaskAction::DockerImageCheck
            | TaskAction::DockerAutoUpdate
            | TaskAction::AppsCatalog
            | TaskAction::PcMaintenance
            | TaskAction::PcRetention
            | TaskAction::MdCheck { .. } => d.kind = "system".into(),
        }
        d
    }

    pub fn action(&self) -> Result<TaskAction, &'static str> {
        let user = if self.user.trim().is_empty() { "root".to_string() } else { self.user.trim().to_string() };
        Ok(match self.kind.as_str() {
            "command" if self.command.trim().is_empty() => return Err("sch.v.command"),
            "command" => TaskAction::Command { command: self.command.trim().to_string(), user },
            "script" if self.script.trim().is_empty() => return Err("sch.v.script"),
            "script" => TaskAction::Script { script: self.script.clone(), user },
            "scrub" if !self.filesystem.trim().starts_with('/') => return Err("sch.v.filesystem"),
            "scrub" => TaskAction::Scrub { filesystem: self.filesystem.trim().to_string() },
            "smart_test" if self.disk.trim().is_empty() => return Err("sch.v.disk"),
            "smart_test" => TaskAction::SmartTest {
                disk: self.disk.trim().trim_start_matches("/dev/").to_string(),
                test: if self.test == "long" { SmartTestKind::Long } else { SmartTestKind::Short },
            },
            "recycle_cleanup" => TaskAction::RecycleCleanup {
                share: (!self.share.trim().is_empty()).then(|| self.share.trim().to_string()),
                days: match self.days.trim() {
                    "" => None,
                    d => Some(d.parse().map_err(|_| "sch.v.days")?),
                },
            },
            _ => return Err("sch.v.action"),
        })
    }
}

/// `[a-z0-9][a-z0-9-]{0,47}`.
pub fn valid_task_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty() && b.len() <= 48 && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit()) && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// An id suggested from the display name ("Nightly rsync" → "nightly-rsync").
pub fn slug(name: &str) -> String {
    let mut s = String::new();
    for c in name.trim().to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            s.push(c);
        } else if !s.is_empty() && !s.ends_with('-') {
            s.push('-');
        }
    }
    let s = s.trim_end_matches('-');
    s.chars().take(48).collect::<String>().trim_end_matches('-').to_string()
}

/// What a task does, in one line.
pub fn action_text(a: &TaskAction) -> (String, String) {
    match a {
        TaskAction::Command { command, user } => (t("sch.act.command").to_string(), format!("{user}: {}", command.lines().next().unwrap_or(""))),
        TaskAction::Script { script, user } => {
            let first = script.lines().find(|l| !l.starts_with("#!") && !l.trim().is_empty()).unwrap_or("");
            (t("sch.act.script").to_string(), format!("{user}: {first}"))
        }
        TaskAction::Snapshot { job } => (t("sch.act.snapshot").to_string(), job.clone()),
        TaskAction::Replicate { job, target } => (t("sch.act.replicate").to_string(), format!("{job} → {target}")),
        TaskAction::Scrub { filesystem } => (t("sch.act.scrub").to_string(), filesystem.clone()),
        TaskAction::SmartTest { disk, test } => {
            (t("sch.act.smart_test").to_string(), format!("{disk} · {}", t(if *test == SmartTestKind::Long { "sch.smart.long" } else { "sch.smart.short" })))
        }
        TaskAction::RecycleCleanup { share, .. } => (t("sch.act.recycle_cleanup").to_string(), share.clone().unwrap_or_else(|| t("sch.all_shares").to_string())),
        TaskAction::PowerShutdown { rule } => (t("sch.act.power_shutdown").to_string(), rule.clone()),
        TaskAction::Backup { job } => (t("sch.act.backup").to_string(), job.clone()),
        TaskAction::Dedup { volume } => (t("sch.act.dedup").to_string(), crate::pages::storage::dedup::job_name(volume)),
        TaskAction::UpdatesCheck => (t("sch.act.updates_check").to_string(), t("upd.title").to_string()),
        TaskAction::SystemUpgrade => (t("sch.act.system_upgrade").to_string(), t("upd.title").to_string()),
        TaskAction::DockerImageCheck => (t("sch.act.docker_image_check").to_string(), t("dk.title").to_string()),
        TaskAction::DockerAutoUpdate => (t("sch.act.docker_auto_update").to_string(), t("dk.title").to_string()),
        TaskAction::AppsCatalog => (t("sch.act.apps_catalog").to_string(), t("dk.title").to_string()),
        TaskAction::PcMaintenance => (t("sch.act.pc_maintenance").to_string(), t("nav.backup").to_string()),
        TaskAction::PcRetention => (t("sch.act.pc_retention").to_string(), t("nav.backup").to_string()),
        TaskAction::MdCheck { resume } => (
            t(if *resume { "sch.act.md_check_continue" } else { "sch.act.md_check" }).to_string(),
            t("sch.system_timer").to_string(),
        ),
    }
}

/// The page where the feature that owns one of rynas's own jobs sets it (its schedule is
/// read-only here), and that page's name.
pub fn owner_page(a: &TaskAction) -> Option<(&'static str, &'static str)> {
    match a {
        TaskAction::UpdatesCheck | TaskAction::SystemUpgrade => Some(("/system/updates", "upd.title")),
        TaskAction::DockerImageCheck | TaskAction::DockerAutoUpdate | TaskAction::AppsCatalog => Some(("/docker", "dk.title")),
        TaskAction::PcMaintenance | TaskAction::PcRetention => Some(("/backup", "nav.backup")),
        TaskAction::PowerShutdown { .. } => Some(("/hardware", "nav.hardware")),
        _ => None,
    }
}

/// Who a job belongs to, in words (the owner column, the Schedule page).
pub fn owner_text(o: OverviewOwner) -> String {
    t(match o {
        OverviewOwner::User => "sch.title",
        OverviewOwner::Snapshots => "sch.own.snapshots",
        OverviewOwner::Replication => "sch.own.replication",
        OverviewOwner::Backup => "sch.own.backup",
        OverviewOwner::Power => "sch.own.power",
        OverviewOwner::Dedup => "sch.own.dedup",
        OverviewOwner::Updates => "sch.own.updates",
        OverviewOwner::Docker => "sch.own.docker",
        OverviewOwner::Apps => "sch.own.apps",
        OverviewOwner::PcBackup => "sch.own.pc_backup",
        OverviewOwner::System => "sch.own.system",
    })
    .to_string()
}

/// The owner of a task.
pub fn owner_of(a: &TaskAction) -> OverviewOwner {
    OverviewOwner::of(&rynas_api_types::scheduler::TaskOwner::of_action(a))
}

/// The CSS suffix of an owner's colour (`own-<suffix>`).
pub fn owner_class(o: OverviewOwner) -> &'static str {
    match o {
        OverviewOwner::User => "user",
        OverviewOwner::Snapshots => "snapshots",
        OverviewOwner::Replication => "replication",
        OverviewOwner::Backup => "backup",
        OverviewOwner::Power => "power",
        OverviewOwner::Dedup => "dedup",
        OverviewOwner::Updates => "updates",
        OverviewOwner::Docker => "docker",
        OverviewOwner::Apps => "apps",
        OverviewOwner::PcBackup => "pc",
        OverviewOwner::System => "system",
    }
}

/// Where a task managed by another page is changed (`None`: edited here).
fn managed_href(a: &TaskAction) -> Option<String> {
    match a {
        TaskAction::Dedup { volume } => Some(crate::pages::storage::dedup::job_href(volume)),
        _ => None,
    }
}

/// Follows a run started with "Run now": polls the task until it is no longer running and a run newer
/// than `before` is recorded. `None` when the run cannot be followed (gone, or still running after ~30 min).
pub async fn follow_run(id: &str, before: Option<u64>) -> Option<TaskRun> {
    let url = format!("/api/v1/scheduler/tasks/{}", api::encode(id));
    for i in 0..1000u32 {
        // fast at first (most runs take seconds), then every 2 s
        api::sleep(if i < 20 { 500 } else { 2000 }).await;
        match api::get::<TaskView>(&url).await {
            Ok(v) if !v.status.running => {
                if let Some(r) = v.status.last_run.filter(|r| Some(r.id) != before) {
                    return Some(r);
                }
            }
            Ok(_) => {}
            Err(e) if e.code() == rynas_api_types::error_codes::NOT_FOUND => return None,
            Err(_) => {}
        }
    }
    None
}

/// Outcome of a run: tone and text ("Succeeded", "Failed (exit 2)", "Timed out"), or why
/// its start was skipped ("Skipped: paused").
pub fn run_status(r: &TaskRun) -> (Tone, String) {
    if let Some(s) = r.skipped {
        let (tone, key) = match s {
            SkipReason::PausedAll => (Tone::Unknown, "sch.skip.paused_all"),
            SkipReason::Paused => (Tone::Unknown, "sch.skip.paused"),
            SkipReason::Missed => (Tone::Warn, "sch.skip.missed"),
            SkipReason::Overlap => (Tone::Unknown, "sch.skip.overlap"),
            SkipReason::Running => (Tone::Unknown, "sch.skip.running"),
            SkipReason::Window => (Tone::Unknown, "sch.skip.window"),
            SkipReason::NotDue => (Tone::Unknown, "sch.skip.not_due"),
            SkipReason::Busy => (Tone::Warn, "sch.skip.busy"),
            SkipReason::Maintenance => (Tone::Unknown, "sch.skip.maintenance"),
        };
        return (tone, t(key).to_string());
    }
    if r.yielded {
        return (Tone::Unknown, t("sch.run.yielded").to_string());
    }
    if r.ok {
        return (Tone::Ok, t("sch.run.ok").to_string());
    }
    let text = match r.result.as_str() {
        "timeout" => t("sch.run.timeout").to_string(),
        "signal" | "core-dump" => tf("sch.run.signal", &[("sig", r.signal.as_deref().unwrap_or("?"))]),
        _ => match r.exit_code {
            Some(c) => tf("sch.run.exit", &[("code", &c.to_string())]),
            None => format!("{} ({})", t("sch.run.failed"), r.result),
        },
    };
    (Tone::Err, text)
}

/// How a recorded start came about, when it was not simply its schedule ("Catch-up",
/// "Retry 2", "Run now"), with the detail of its outcome.
pub fn run_origin(r: &TaskRun) -> Option<String> {
    let how = match r.trigger? {
        RunTrigger::Timer => None,
        RunTrigger::CatchUp => Some(t("sch.trigger.catch_up").to_string()),
        RunTrigger::Boot => Some(t("sch.trigger.boot").to_string()),
        RunTrigger::Manual => Some(t("sch.trigger.manual").to_string()),
        RunTrigger::Retry => Some(tf("sch.trigger.retry", &[("n", &r.attempt.to_string())])),
        RunTrigger::Queued => Some(t("sch.trigger.queued").to_string()),
        RunTrigger::Other => Some(t("sch.trigger.other").to_string()),
        RunTrigger::Resume => Some(t("sch.trigger.resume").to_string()),
    };
    let detail = match (r.skipped, r.detail.as_deref()) {
        (Some(SkipReason::Missed), Some(d)) => Some(tf("sch.skip.missed_detail", &[("deadline", d)])),
        // the server's sentence of a scrub that gave way, in the interface's language
        (None, Some(_)) if r.yielded => Some(t("sch.yielded_detail").to_string()),
        // the server names what it waited for in English (the journal's words): the chip says
        // enough
        (Some(SkipReason::Busy | SkipReason::Maintenance), _) => None,
        (_, Some(d)) if !d.is_empty() => Some(d.to_string()),
        _ => None,
    };
    let waited = (r.queued_ms >= 1000).then(|| tf("sch.waited", &[("d", &i18n::duration(r.queued_ms / 1000))]));
    let ungated = r.ungated.then(|| t("sch.ungated").to_string());
    let parts: Vec<String> = [how, waited, detail, ungated].into_iter().flatten().collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The i18n key of one of rynas's own jobs by its task id (their stored names are English).
pub(crate) fn own_job_key(id: &str) -> Option<&'static str> {
    use rynas_api_types::scheduler::system as s;
    Some(match id {
        s::UPDATES_CHECK => "sch.act.updates_check",
        s::SYSTEM_UPGRADE => "sch.act.system_upgrade",
        s::DOCKER_IMAGE_CHECK => "sch.act.docker_image_check",
        s::DOCKER_AUTO_UPDATE => "sch.act.docker_auto_update",
        s::APPS_CATALOG => "sch.act.apps_catalog",
        s::PC_MAINTENANCE => "sch.act.pc_maintenance",
        s::PC_RETENTION => "sch.act.pc_retention",
        s::MD_CHECK_START => "sch.act.md_check",
        s::MD_CHECK_CONTINUE => "sch.act.md_check_continue",
        _ => return None,
    })
}

/// A resource of the queue in words ("/srv/vol1", "disk sda", "the package system").
fn resource_text(r: &Resource) -> String {
    match r {
        Resource::Volume { path } if path == "*" => t("sch.res.every").to_string(),
        Resource::Volume { path } => path.clone(),
        Resource::Disk { disk } => tf("sch.res.disk", &[("disk", disk)]),
        Resource::Packages => t("sch.res.packages").to_string(),
        Resource::Power => t("sch.res.power").to_string(),
        Resource::Maintenance => t("sch.res.maintenance").to_string(),
    }
}

/// Who holds a resource, in words: a task by its name, other work by its type.
fn blocker_who(b: &Blocker) -> String {
    match (&b.task, &b.kind) {
        (Some(id), _) => own_job_key(id)
            .map(|k| t(k).to_string())
            .unwrap_or_else(|| if b.name.is_empty() { id.clone() } else { b.name.clone() }),
        (None, Some(k)) => crate::tasks::task_label(k),
        (None, None) if !b.name.is_empty() => b.name.clone(),
        (None, None) => t("sch.other_work").to_string(),
    }
}

/// "Queued: waiting for Backup offsite on /srv/vol1".
pub fn queued_text(q: &QueueWait) -> String {
    if let Some(o) = q.window_opens_ms {
        return tf("sch.q.window", &[("when", &i18n::short_when(o))]);
    }
    match &q.blocker {
        Some(b) => tf("sch.q.waiting", &[("who", &blocker_who(b)), ("res", &resource_text(&b.resource))]),
        None => t("sch.q.queued").to_string(),
    }
}

/// "Gave way to Backup offsite on /srv/vol1: resumes by itself".
pub fn yielded_text(y: &YieldInfo) -> String {
    match (&y.to, y.stopped) {
        (_, false) => t("sch.y.stopping").to_string(),
        (Some(b), true) => tf("sch.y.gave_way", &[("who", &blocker_who(b)), ("res", &resource_text(&b.resource))]),
        (None, true) => t("sch.y.will_resume").to_string(),
    }
}

/// One line of the run now question: who, on what.
fn busy_line(b: &Blocker) -> String {
    format!("{} · {}", blocker_who(b), resource_text(&b.resource))
}

/// A run now waiting for the user's answer: the task, the work it would slow down, the
/// row's "running" flag and the latest run before it.
#[derive(Clone)]
struct RunAsk {
    id: String,
    name: String,
    busy: Vec<Blocker>,
    running: RwSignal<bool>,
    before: Option<u64>,
}

/// Starts the task (`skip_queue`: ahead of the queue, the user confirmed) and follows its run
/// to a toast.
async fn start_run(id: String, name: String, before: Option<u64>, skip_queue: bool, running: RwSignal<bool>, tasks: Remote<Vec<TaskView>>) {
    running.set(true);
    let body = RunNowRequest { skip_queue };
    let r = api::post_empty(&format!("/api/v1/scheduler/tasks/{}/run", api::encode(&id)), Some(&body)).await;
    match r {
        Ok(()) => {
            toast(Tone::Info, tf("sch.started", &[("name", &name)]));
            set_timeout(move || tasks.reload(), Duration::from_millis(800));
            let done = follow_run(&id, before).await;
            running.try_set(false);
            match done {
                Some(r) if r.yielded => {}
                Some(r) if r.ok => toast(Tone::Ok, tf("sch.finished_ok", &[("name", &name)])),
                Some(r) => toast(Tone::Err, tf("sch.finished_err", &[("name", &name), ("why", &run_status(&r).1)])),
                None => {}
            }
            tasks.reload();
        }
        Err(e) => {
            running.set(false);
            if e.code() == rynas_api_types::error_codes::NOT_APPLIED {
                toast(Tone::Err, t("sch.not_applied").to_string())
            } else {
                toast(Tone::Err, e.text())
            }
        }
    }
}

/// Run now while other work holds what the task needs: names that work, and lets the user
/// run at once (slowing it down) or wait in the queue.
fn run_ask_dialog(ask: RunAsk, tasks: Remote<Vec<TaskView>>, close: Callback<()>) -> AnyView {
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let lines: Vec<String> = ask.busy.iter().map(busy_line).collect();
    let (a1, a2) = (ask.clone(), ask.clone());
    let queue = move |_| {
        close.run(());
        let a = a2.clone();
        spawn_local(start_run(a.id, a.name, a.before, false, a.running, tasks));
    };
    let extra = view! {
        <ul class="sch-busy-list">{lines.into_iter().map(|l| view! { <li>{l}</li> }).collect::<Vec<_>>()}</ul>
        <div class="card-actions">
            <button class="btn" on:click=queue>{tr("sch.run_queue")}</button>
        </div>
    }
    .into_any();
    confirm(Confirm {
        title: tf("sch.run_busy_title", &[("name", &ask.name)]),
        text: t("sch.run_busy_text").to_string(),
        action: "sch.run_anyway",
        danger: false,
        extra: Some(extra),
        typed: Signal::stored(None),
        busy,
        error,
        on_confirm: Callback::new(move |_| {
            close.run(());
            let a = a1.clone();
            spawn_local(start_run(a.id, a.name, a.before, true, a.running, tasks));
        }),
        on_cancel: close,
    })
}

// ------------------------------------------------------------------ page

#[component]
pub fn Scheduler() -> impl IntoView {
    let tab = RwSignal::new(0usize);
    let tasks = Remote::<Vec<TaskView>>::with_config("/api/v1/scheduler/tasks");
    let pause = Remote::<SchedulerPause>::new("/api/v1/scheduler/pause");
    // next runs and running state move by themselves: read again every 15 s while visible
    crate::live::poll(Duration::from_secs(15), move || {
        tasks.reload();
        pause.reload();
    });
    let edit = RwSignal::new(None::<(ScheduledTask, bool)>);
    let pausing = RwSignal::new(false);
    let paused = Memo::new(move |_| pause.data.with(|p| p.as_ref().is_some_and(|p| p.all.is_some())));
    view! {
        <div class="page">
            <PageHeader icon=icons::CALENDAR_CLOCK title="sch.title" subtitle="sch.subtitle">
                <Show when=move || !paused.get()>
                    <button class="btn ghost" on:click=move |_| pausing.set(true)>
                        <Icon icon=icons::PAUSE size=16 /><span class="btn-label">{tr("sch.pause_all")}</span>
                    </button>
                </Show>
                <button class="btn primary" on:click=move |_| edit.set(Some((new_task(), true)))>
                    <Icon icon=icons::PLUS size=16 /><span class="btn-label">{tr("sch.create")}</span>
                </button>
            </PageHeader>
            {pause_banner(pause, tasks)}
            <section class="card widget w-12">
                {tabs("sch", &["sch.tab.tasks", "sch.tab.timers", "sch.tab.cron"], tab, "sch.title")}
                <div role="tabpanel" id="sch-p0" aria-labelledby="sch-t0" hidden=move || tab.get() != 0>{tasks_panel(tasks, edit)}</div>
                <div role="tabpanel" id="sch-p1" aria-labelledby="sch-t1" hidden=move || tab.get() != 1>
                    {move || (tab.get() == 1).then(timers_panel)}
                </div>
                <div role="tabpanel" id="sch-p2" aria-labelledby="sch-t2" hidden=move || tab.get() != 2>
                    {move || (tab.get() == 2).then(|| cron_panel(edit))}
                </div>
            </section>
            {move || edit.get().map(|(task, is_new)| task_editor(task, is_new, Callback::new(move |_| edit.set(None))))}
            {move || pausing.get().then(|| pause_dialog(None, Callback::new(move |_| { pause.reload(); tasks.reload(); }), Callback::new(move |_| pausing.set(false))))}
        </div>
    }
}

/// "Resume" choices of the pause dialog: seconds from now (0: until resumed).
const PAUSE_FOR: &[(&str, &str)] = &[
    ("0", "sch.pause_for.never"),
    ("3600", "dd.h1"),
    ("28800", "dd.h8"),
    ("86400", "dd.h24"),
    ("604800", "sch.pause_for.d7"),
];

/// Pauses every task (`task` `None`) or one task (id, name).
pub(crate) fn pause_dialog(task: Option<(String, String)>, done: Callback<()>, close: Callback<()>) -> AnyView {
    let reason = RwSignal::new(String::new());
    let dur = RwSignal::new("0".to_string());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let url = match &task {
        Some((id, _)) => format!("/api/v1/scheduler/tasks/{}/pause", api::encode(id)),
        None => "/api/v1/scheduler/pause".to_string(),
    };
    let title = match &task {
        Some((_, n)) => tf("sch.pause_task_title", &[("name", n)]),
        None => t("sch.pause_title").to_string(),
    };
    let text = t(if task.is_some() { "sch.pause_task_text" } else { "sch.paused_text" }).to_string();
    let extra = view! {
        {text_field("sch.pause_reason", Signal::derive(move || reason.get()), Callback::new(move |v| reason.set(v)), Signal::stored(None), In::default())}
        {select_field("sch.pause_for", None, PAUSE_FOR, Signal::derive(move || dur.get()), Callback::new(move |v| dur.set(v)))}
    }
    .into_any();
    confirm(Confirm {
        title,
        text,
        action: "sch.pause",
        danger: false,
        extra: Some(extra),
        typed: Signal::stored(None),
        busy,
        error,
        on_confirm: Callback::new(move |_| {
            busy.set(true);
            let secs: u64 = dur.get_untracked().parse().unwrap_or(0);
            let body = PauseRequest {
                until_ms: (secs > 0).then(|| js_sys::Date::now() as u64 + secs * 1000),
                reason: reason.get_untracked(),
            };
            let url = url.clone();
            spawn_local(async move {
                let r = api::post::<_, SchedulerPause>(&url, Some(&body)).await;
                busy.set(false);
                match r {
                    Ok(_) => {
                        done.run(());
                        close.run(());
                    }
                    Err(e) => error.set(Some(e.text())),
                }
            });
        }),
        on_cancel: close,
    })
}

/// While every task is paused: what it means, since when, and Resume.
pub(crate) fn pause_banner(pause: Remote<SchedulerPause>, tasks: Remote<Vec<TaskView>>) -> impl IntoView {
    let busy = RwSignal::new(false);
    let resume = move |_| {
        busy.set(true);
        spawn_local(async move {
            let r = api::post::<(), SchedulerPause>("/api/v1/scheduler/resume", None).await;
            busy.set(false);
            match r {
                Ok(_) => toast(Tone::Ok, t("sch.resumed").to_string()),
                Err(e) => toast(Tone::Err, e.text()),
            }
            pause.reload();
            tasks.reload();
        });
    };
    move || {
        pause.data.get().and_then(|p| p.all).map(|a| {
            let mut when = tf("sch.paused_since", &[("when", &i18n::datetime(a.since_ms)), ("user", &a.by)]);
            if let Some(u) = a.until_ms {
                when.push_str(" · ");
                when.push_str(&tf("sch.paused_until", &[("when", &i18n::datetime(u))]));
            }
            let reason = (!a.reason.is_empty()).then(|| a.reason.clone());
            view! {
                <section class="card widget w-12" aria-labelledby="sch-paused-h">
                    {ui::note(Tone::Warn, icons::PAUSE, view! {
                        <strong id="sch-paused-h">{tr("sch.paused_banner")}</strong>" "{tr("sch.paused_text")}
                        {reason.map(|r| view! { " "<strong>{r}</strong> })}
                        " "<span class="muted">{when}</span>
                    }
                    .into_any())}
                    <div class="card-actions">
                        <button class="btn primary" disabled=move || busy.get() on:click=resume>
                            {crate::form::spinner(busy)}<Icon icon=icons::PLAY size=16 />{tr("sch.resume_all")}
                        </button>
                    </div>
                </section>
            }
        })
    }
}

fn new_task() -> ScheduledTask {
    ScheduledTask {
        id: String::new(),
        name: String::new(),
        description: String::new(),
        enabled: true,
        action: TaskAction::Command { command: String::new(), user: "root".into() },
        trigger: TaskTrigger::Calendar { on_calendar: "*-*-* 03:00:00".into(), cron: None, persistent: true, randomized_delay_s: 0 },
        timeout_s: None,
        nice: None,
        io_weight: None,
        notify: NotifyPolicy::OnFailure,
        notify_output: false,
        owner: Default::default(),
        policy: Default::default(),
    }
}

fn tasks_panel(tasks: Remote<Vec<TaskView>>, edit: RwSignal<Option<(ScheduledTask, bool)>>) -> AnyView {
    let q = RwSignal::new(String::new());
    let history = RwSignal::new(None::<ScheduledTask>);
    let del = RwSignal::new(None::<(String, String)>);
    let pausing = RwSignal::new(None::<(String, String)>);
    let asking = RwSignal::new(None::<RunAsk>);
    let table = move || {
        if let Some(v) = tasks.gate(5) {
            return v;
        }
        let needle = q.get().to_lowercase();
        let rows: Vec<TaskView> = tasks.data.with(|d| {
            d.iter()
                .flatten()
                .filter(|v| needle.is_empty() || v.task.id.contains(&needle) || v.task.name.to_lowercase().contains(&needle) || v.task.description.to_lowercase().contains(&needle))
                .cloned()
                .collect()
        });
        if rows.is_empty() {
            return if needle.is_empty() {
                view! {
                    // Create is in the page header: not repeated here
                    <EmptyState icon=icons::CALENDAR_CLOCK title=Signal::derive(|| t("sch.empty").to_string()) text=Signal::derive(|| t("sch.empty_text").to_string()) />
                }
                .into_any()
            } else {
                view! { <p class="muted table-none">{tr("table.no_match")}</p> }.into_any()
            };
        }
        view! {
            {ui::table("", &["sch.col.enabled", "sch.col.task", "sch.col.owner", "sch.col.action", "sch.col.schedule", "sch.col.last", ""], rows.into_iter().map(|v| task_row(v, tasks, edit, history, del, pausing, asking)).collect::<Vec<_>>())}
        }
        .into_any()
    };
    let deleter = move || {
        del.get().map(|(id, name)| {
            let busy = RwSignal::new(false);
            let error = RwSignal::new(None::<String>);
            let close = Callback::new(move |_| del.set(None));
            confirm(Confirm {
                title: tf("sch.delete_title", &[("name", &name)]),
                text: t("sch.delete_text").to_string(),
                action: "delete",
                danger: true,
                extra: None,
                typed: Signal::stored(None),
                busy,
                error,
                on_confirm: Callback::new(move |_| {
                    busy.set(true);
                    let id = id.clone();
                    spawn_local(async move {
                        let r = api::delete_empty(&format!("/api/v1/scheduler/tasks/{}", api::encode(&id))).await;
                        busy.set(false);
                        match r {
                            Ok(()) => {
                                saved_pending();
                                crate::config::changed();
                                close.run(());
                            }
                            Err(e) if e.code() == rynas_api_types::error_codes::CONFLICT => error.set(Some(t("sch.delete_job_task").to_string())),
                            Err(e) => error.set(Some(e.text())),
                        }
                    });
                }),
                on_cancel: close,
            })
        })
    };
    view! {
        <div class="toolbar">{crate::form::search_box("sch.search", q)}</div>
        {table}
        {move || history.get().map(|t| history_drawer(t, tasks, Callback::new(move |_| history.set(None))))}
        {deleter}
        {move || pausing.get().map(|target| pause_dialog(Some(target), Callback::new(move |_| tasks.reload()), Callback::new(move |_| pausing.set(None))))}
        {move || asking.get().map(|a| run_ask_dialog(a, tasks, Callback::new(move |_| asking.set(None))))}
    }
    .into_any()
}

fn task_row(
    v: TaskView,
    tasks: Remote<Vec<TaskView>>,
    edit: RwSignal<Option<(ScheduledTask, bool)>>,
    history: RwSignal<Option<ScheduledTask>>,
    del: RwSignal<Option<(String, String)>>,
    pausing: RwSignal<Option<(String, String)>>,
    asking: RwSignal<Option<RunAsk>>,
) -> AnyView {
    let task = v.task.clone();
    let st = v.status.clone();
    let id = task.id.clone();
    // rynas's own jobs are named by their action (in the interface's language); their stored
    // name and description are English
    let own_job = own_job_key(&task.id).is_some()
        || (owner_page(&task.action).is_some() && !matches!(task.action, TaskAction::PowerShutdown { .. }));
    let name = if own_job {
        action_text(&task.action).0
    } else if task.name.is_empty() {
        task.id.clone()
    } else {
        task.name.clone()
    };
    let deleting = st.pending == Some(PendingOp::Delete);
    // a deduplication task follows its window in Storage: read-only here
    // the automatic system updates run on the system's own timer: listed, nothing to do here
    let system = st.system;
    let managed = managed_href(&task.action);
    let is_managed = managed.is_some() || system;
    let fixed = is_managed || !rynas_api_types::scheduler::TaskOwner::of_action(&task.action).is_user();
    // the feature decides whether these are on
    let feature_switch = system || matches!(task.action, TaskAction::DockerAutoUpdate | TaskAction::AppsCatalog);
    let owner = owner_page(&task.action);
    let (akind, adetail) = action_text(&task.action);
    let enabled = task.enabled;
    let toggling = RwSignal::new(false);
    let running = RwSignal::new(false);
    let id_t = id.clone();
    let toggle = move |on: bool| {
        toggling.set(true);
        let id = id_t.clone();
        spawn_local(async move {
            let r = api::post::<(), TaskView>(&format!("/api/v1/scheduler/tasks/{}/{}", api::encode(&id), if on { "enable" } else { "disable" }), None).await;
            toggling.set(false);
            match r {
                Ok(_) => {
                    saved_pending();
                    crate::config::changed();
                }
                Err(e) => {
                    toast(Tone::Err, e.text());
                    tasks.reload();
                }
            }
        });
    };
    let (id_r, name_r) = (id.clone(), name.clone());
    let before = st.last_run.as_ref().map(|r| r.id);
    // run now: when other work holds what the task needs, the user chooses between running
    // at once (it slows that work down) and waiting in the queue
    let run = move |_| {
        running.set(true);
        let (id, name) = (id_r.clone(), name_r.clone());
        spawn_local(async move {
            let busy = api::get::<RunConflicts>(&format!("/api/v1/scheduler/tasks/{}/conflicts", api::encode(&id)))
                .await
                .map(|c| c.busy)
                .unwrap_or_default();
            if busy.is_empty() {
                start_run(id, name, before, false, running, tasks).await;
            } else {
                running.set(false);
                asking.set(Some(RunAsk { id, name, busy, running, before }));
            }
        });
    };
    let last = st.last_run.as_ref().map(|r| {
        let (tone, text) = run_status(r);
        view! {
            <div class="cell-main">
                {ui::status(tone, text)}
                <span class="cell-sub tnum">{i18n::short_when(r.end_ms)}</span>
            </div>
        }
    });
    let next = st.next_run_ms.map(|m| tf("sch.next", &[("when", &i18n::short_when(m))]));
    let (t_edit, t_hist, t_live) = (task.clone(), task.clone(), task.clone());
    let (n1, n2, n3, n4, n5, n6, n7) = (name.clone(), name.clone(), name.clone(), name.clone(), name.clone(), name.clone(), name.clone());
    let id_d = id.clone();
    ui::row_class(
        vec![
            ui::cell("sch.col.enabled", "switch-col", view! {
                <input type="checkbox" role="switch" class="switch" aria-label=ui::verb_name("sch.col.enabled", &n1)
                prop:checked=enabled disabled=move || toggling.get() || deleting || is_managed || feature_switch
                on:change=move |ev| toggle(event_target_checked(&ev)) />
            }
            .into_any()),
            ui::cell("sch.col.task", "", view! {
                <div class="cell-main">
                    {match (managed.clone(), system.then_some(owner).flatten()) {
                        (Some(href), _) => view! { <a class="link-strong" href=href title=tr("sch.managed_storage")>{name.clone()}</a> }.into_any(),
                        (None, Some((href, _))) => view! { <a class="link-strong" href=href>{name.clone()}</a> }.into_any(),
                        // the system's own timer (the md RAID check): nothing to edit here
                        (None, None) if system => view! { <strong>{name.clone()}</strong> }.into_any(),
                        (None, None) => view! { <button class="link-strong" disabled=deleting on:click=move |_| edit.set(Some((t_edit.clone(), false)))>{name.clone()}</button> }.into_any(),
                    }}
                    {managed.is_some().then(|| view! { <span class="cell-sub">{tr("sch.managed_storage")}</span> })}
                    {system.then(|| view! { <span class="cell-sub">{tr("sch.system_timer")}</span> })}
                    {owner.filter(|_| managed.is_none()).map(|(href, page)| view! {
                        <span class="cell-sub"><a href=href>{tf("sch.set_in", &[("page", t(page))])}</a></span>
                    })}
                    <span class="cell-sub mono">{task.id.clone()}</span>
                    {(!own_job && !task.description.is_empty()).then(|| view! { <span class="cell-sub">{task.description.clone()}</span> })}
                </div>
            }
            .into_any()),
            ui::cell("sch.col.owner", "", view! { <span class=format!("owner-tag own-{}", owner_class(owner_of(&task.action)))>{owner_text(owner_of(&task.action))}</span> }.into_any()),
            ui::cell("sch.col.action", "sch-action", view! {
                <div class="cell-main">
                    <span>{akind}</span>
                    <span class="cell-sub mono ellipsis-cell">{adetail}</span>
                </div>
            }
            .into_any()),
            ui::cell("sch.col.schedule", "", view! {
                <div class="cell-main">
                    <span>{trigger_text(&task.trigger)}</span>
                    <span class="cell-sub tnum">{next}</span>
                </div>
            }
            .into_any()),
            ui::cell("sch.col.last", "", view! {
                <div class="chips">
                    {st.paused.is_some().then(|| ui::status(Tone::Warn, t("sch.paused").to_string()))}
                    {st.retry.map(|r| ui::status(Tone::Info, tf("sch.retry_at", &[("n", &r.attempt.to_string()), ("of", &r.attempts.to_string()), ("when", &i18n::short_when(r.due_ms))])))}
                    {st.running.then(|| ui::status(Tone::Info, t("sch.running").to_string()))}
                    {st.queued.as_ref().filter(|_| !st.running).map(|q| ui::status(Tone::Unknown, queued_text(q)))}
                    {st.yielded.as_ref().filter(|_| !st.running).map(|y| ui::status(Tone::Unknown, yielded_text(y)))}
                    {if st.running { None } else { last.map(|l| l.into_any()) }}
                    {ui::pending_status(st.pending)}
                    {(st.last_run.is_none() && !st.running && st.pending.is_none()).then(|| view! { <span class="muted small">{tr("never_run")}</span> })}
                </div>
            }
            .into_any()),
            ui::actions(view! {
                <button class="icon-btn sm" title=tr("run_now") aria-label=ui::verb_name("run_now", &n2)
                    disabled=move || running.get() || st.running || !st.applied || is_managed on:click=run>
                    <Icon icon=icons::PLAY size=16 />
                </button>
                // the same drawer either way: while a run goes on (a timer's too) it opens on its
                // live output, so the button says that (one button: the row keeps its width)
                {if st.running {
                    view! {
                        <button class="icon-btn sm" title=tr("sch.live") aria-label=ui::verb_name("sch.live", &n6) on:click=move |_| history.set(Some(t_live.clone()))>
                            <Icon icon=icons::ACTIVITY size=16 />
                        </button>
                    }
                    .into_any()
                } else {
                    view! {
                        <button class="icon-btn sm" title=tr("sch.history") aria-label=ui::verb_name("sch.history", &n3) on:click=move |_| history.set(Some(t_hist.clone()))>
                            <Icon icon=icons::HISTORY size=16 />
                        </button>
                    }
                    .into_any()
                }}
                // pause: the task's scheduled starts are skipped (run now still works)
                {
                    let (id_p, name_p) = (id.clone(), n7.clone());
                    let is_paused = st.paused.is_some();
                    let toggle_pause = move |_| {
                        if !is_paused {
                            pausing.set(Some((id_p.clone(), name_p.clone())));
                            return;
                        }
                        let id = id_p.clone();
                        spawn_local(async move {
                            let r = api::post::<(), SchedulerPause>(&format!("/api/v1/scheduler/tasks/{}/resume", api::encode(&id)), None).await;
                            if let Err(e) = r {
                                toast(Tone::Err, e.text());
                            }
                            tasks.reload();
                        });
                    };
                    let key = if is_paused { "sch.resume" } else { "sch.pause" };
                    view! {
                        <button class="icon-btn sm" title=tr(key) aria-label=ui::verb_name(key, &n7) aria-pressed=is_paused.to_string()
                            disabled=deleting || !st.applied on:click=toggle_pause>
                            <Icon icon=icons::PAUSE size=16 />
                        </button>
                    }
                }
                <button class="icon-btn sm" title=tr("edit") aria-label=format!("{} {}", t("edit"), n4) disabled=deleting || is_managed on:click=move |_| edit.set(Some((task.clone(), false)))>
                    <Icon icon=icons::PENCIL size=16 />
                </button>
                <button class="icon-btn sm danger" title=move || if is_managed { t("sch.managed_storage") } else if fixed { t("sch.delete_job_task") } else { t("delete") } aria-label=format!("{} {}", t("delete"), n5)
                    disabled=deleting || fixed on:click=move |_| del.set(Some((id_d.clone(), name.clone())))>
                    <Icon icon=icons::TRASH_2 size=16 />
                </button>
            }
            .into_any()),
        ],
        if !enabled { "muted-row" } else { "" },
    )
}

pub(crate) fn task_editor(init: ScheduledTask, is_new: bool, close: Callback<()>) -> AnyView {
    let internal_fixed = !is_new && init.action.is_internal();
    // the schedule of a job's, a replica's or a power rule's task is theirs (the server
    // refuses a change here): shown, not edited
    let schedule_owned = !is_new
        && rynas_api_types::scheduler::TaskOwner::of_action(&init.action)
            .locked()
            .contains(&rynas_api_types::scheduler::LockedField::Schedule);
    let action_summary = action_text(&init.action);
    let trig = ScheduleEditor::new(&init.trigger);
    let act = RwSignal::new(ActionDraft::from_action(&init.action));
    let d = RwSignal::new(init);
    let id_touched = RwSignal::new(!is_new);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let none = Signal::derive(|| None::<String>);

    let (name, set_name0) = lens(d, |t| t.name.clone(), |t, v| t.name = v);
    let set_name = Callback::new(move |v: String| {
        if !id_touched.get_untracked() {
            let s = slug(&v);
            d.update(|t| t.id = s);
        }
        set_name0.run(v);
    });
    let (id, set_id0) = lens(d, |t| t.id.clone(), |t, v| t.id = v.trim().to_ascii_lowercase());
    let set_id = Callback::new(move |v| {
        id_touched.set(true);
        set_id0.run(v);
    });
    let id_err = Signal::derive(move || {
        let v = id.get();
        (!v.is_empty() && !valid_task_id(&v)).then(|| t("sch.v.id").to_string())
    });
    let (descr, set_descr) = lens(d, |t| t.description.clone(), |t, v| t.description = v);
    let (enabled, set_enabled) = lens_bool(d, |t| t.enabled, |t, v| t.enabled = v);
    let opt_num = |get: fn(&ScheduledTask) -> String, set: fn(&mut ScheduledTask, String)| lens(d, get, set);
    let (timeout, set_timeout_s) = opt_num(|t| t.timeout_s.map(|v| v.to_string()).unwrap_or_default(), |t, v| t.timeout_s = v.trim().parse().ok());
    let (nice, set_nice) = opt_num(|t| t.nice.map(|v| v.to_string()).unwrap_or_default(), |t, v| t.nice = v.trim().parse().ok());
    let (iow, set_iow) = opt_num(|t| t.io_weight.map(|v| v.to_string()).unwrap_or_default(), |t, v| t.io_weight = v.trim().parse().ok());
    let (notify, set_notify) = lens(
        d,
        |t| match t.notify {
            NotifyPolicy::Never => "never",
            NotifyPolicy::OnFailure => "on_failure",
            NotifyPolicy::Always => "always",
        }
        .to_string(),
        |t, v| {
            t.notify = match v.as_str() {
                "never" => NotifyPolicy::Never,
                "always" => NotifyPolicy::Always,
                _ => NotifyPolicy::OnFailure,
            }
        },
    );
    let (notify_output, set_notify_output) = lens_bool(d, |t| t.notify_output, |t, v| t.notify_output = v);
    // heavy work follows the global maintenance window unless it has a start window of its
    // own; stored only when it differs from that default
    let (in_window, set_in_window) = lens_bool(
        d,
        |t| t.policy.maintenance.unwrap_or(t.policy.window_min.unwrap_or(0) == 0),
        |t, v| t.policy.maintenance = (v != (t.policy.window_min.unwrap_or(0) == 0)).then_some(v),
    );
    let heavy = Memo::new(move |_| {
        if internal_fixed {
            d.with(|t| t.action.heavy())
        } else {
            act.with(|a| a.action().is_ok_and(|x| x.heavy()))
        }
    });

    let akind = Memo::new(move |_| act.with(|a| a.kind.clone()));
    let afield = move |get: fn(&ActionDraft) -> String, set: fn(&mut ActionDraft, String)| (Signal::derive(move || act.with(get)), Callback::new(move |v| act.update(|a| set(a, v))));
    let action_fields = move || {
        if internal_fixed {
            let (k, detail) = action_summary.clone();
            return view! {
                <dl class="kvs">
                    <div class="kv"><dt>{tr("sch.col.action")}</dt><dd>{k}</dd></div>
                    <div class="kv"><dt>{tr("sch.target")}</dt><dd class="mono">{detail}</dd></div>
                </dl>
                <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("sch.internal_help")}</p>
            }
            .into_any();
        }
        let user = {
            let (v, s) = afield(|a| a.user.clone(), |a, v| a.user = v);
            text_field("sch.f.user", v, s, none, In { mono: true, help: Some("sch.f.user_help"), placeholder: "root", ..In::default() })
        };
        let kind_sel = {
            let (v, s) = afield(|a| a.kind.clone(), |a, v| a.kind = v);
            select_field("sch.col.action", None, ACTIONS, v, s)
        };
        let rest = match akind.get().as_str() {
            "command" => {
                let (v, s) = afield(|a| a.command.clone(), |a, v| a.command = v);
                view! { {area_field("sch.f.command", v, s, Some("sch.f.command_help"), "rsync -a /volume1/photos/ /volume2/backup/photos/")}{user} }.into_any()
            }
            "script" => {
                let (v, s) = afield(|a| a.script.clone(), |a, v| a.script = v);
                view! { {area_field("sch.f.script", v, s, Some("sch.f.script_help"), "#!/bin/sh\nset -e\n")}{user} }.into_any()
            }
            "scrub" => {
                let (v, s) = afield(|a| a.filesystem.clone(), |a, v| a.filesystem = v);
                text_field("sch.f.filesystem", v, s, none, In { mono: true, help: Some("sch.f.filesystem_help"), placeholder: "/volume1", ..In::default() })
            }
            "smart_test" => {
                let (v, s) = afield(|a| a.disk.clone(), |a, v| a.disk = v);
                let (tv, ts) = afield(|a| a.test.clone(), |a, v| a.test = v);
                view! {
                    <div class="form-row">
                        {text_field("sch.f.disk", v, s, none, In { mono: true, placeholder: "sda", ..In::default() })}
                        {select_field("sch.f.test", None, SMART, tv, ts)}
                    </div>
                }
                .into_any()
            }
            "recycle_cleanup" => {
                let (v, s) = afield(|a| a.share.clone(), |a, v| a.share = v);
                let (dv, ds) = afield(|a| a.days.clone(), |a, v| a.days = v);
                view! {
                    <div class="form-row">
                        {text_field("shares.folder", v, s, none, In { mono: true, help: Some("sch.f.share_help"), ..In::default() })}
                        {number_field("sch.f.days", Some("sch.f.days_help"), dv, ds, 1, 3650)}
                    </div>
                }
                .into_any()
            }
            _ => ().into_any(),
        };
        view! { {kind_sel}{rest} }.into_any()
    };

    let save = Callback::new(move |_| {
        let mut task = d.get_untracked();
        if is_new && !valid_task_id(&task.id) {
            error.set(Some(t("sch.v.id").to_string()));
            return;
        }
        if !internal_fixed {
            match act.with_untracked(ActionDraft::action) {
                Ok(a) => task.action = a,
                Err(k) => {
                    error.set(Some(t(k).to_string()));
                    return;
                }
            }
        }
        if !schedule_owned {
            match trig.trigger() {
                Ok(tr) => {
                    task.trigger = tr;
                    trig.apply_policy(&mut task.policy);
                }
                Err(k) => {
                    error.set(Some(t(k).to_string()));
                    return;
                }
            }
        }
        task.name = task.name.trim().to_string();
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = if is_new {
                api::post::<_, TaskView>("/api/v1/scheduler/tasks", Some(&task)).await.map(|_| ())
            } else {
                api::put::<_, serde_json::Value>(&format!("/api/v1/scheduler/tasks/{}", api::encode(&task.id)), &task).await.map(|_| ())
            };
            match r {
                Ok(()) => {
                    if crate::config::apply_now(&[crate::config::keys::SCHEDULER], error).await {
                        saved_applied();
                        close.run(());
                    }
                }
                Err(e) => error.set(Some(e.text())),
            }
            let _ = busy.try_set(false);
        });
    });

    let title = {
        let n = d.with_untracked(|t| {
            if owner_page(&t.action).is_some() && !matches!(t.action, TaskAction::PowerShutdown { .. }) {
                action_text(&t.action).0
            } else if t.name.is_empty() {
                t.id.clone()
            } else {
                t.name.clone()
            }
        });
        Signal::derive(move || if is_new { t("sch.create").to_string() } else { tf("edit_named", &[("name", &n)]) })
    };
    let body = view! {
        <form class="form" on:submit=move |ev| { ev.prevent_default(); save.run(()); }>
            <h3 class="form-section">{tr("sch.sec.general")}</h3>
            {text_field("sch.f.name", name, set_name, none, In { maxlength: 80, ..In::default() })}
            {is_new.then(|| text_field("sch.f.id", id, set_id, id_err, In { mono: true, help: Some("sch.f.id_help"), maxlength: 48, ..In::default() }))}
            {text_field("sch.f.description", descr, set_descr, none, In::default())}
            {switch("sch.f.enabled", Some("sch.f.enabled_help"), enabled, set_enabled)}
            <h3 class="form-section">{tr("sch.sec.action")}</h3>
            {action_fields}
            <h3 class="form-section">{tr("sch.sec.schedule")}</h3>
            {if schedule_owned {
                view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("sch.schedule_owned")}</p> }.into_any()
            } else {
                trig.view(true)
            }}
            {move || heavy.get().then(|| switch("sch.f.maintenance", Some("sch.f.maintenance_help"), in_window, set_in_window))}
            <h3 class="form-section">{tr("sch.sec.notify")}</h3>
            {select_field("sch.f.notify", None, NOTIFY, notify, set_notify)}
            {switch("sch.f.notify_output", Some("sch.f.notify_output_help"), notify_output, set_notify_output)}
            {ui::fold(None, "advanced", false, None, view! { 
                <div class="form-row three">
                    {number_field("sch.f.timeout", Some("sch.f.timeout_help"), timeout, set_timeout_s, 1, 604800)}
                    {number_field("sch.f.nice", Some("sch.f.nice_help"), nice, set_nice, -20, 19)}
                    {number_field("sch.f.io_weight", Some("sch.f.io_weight_help"), iow, set_iow, 1, 10000)}
                </div>
             }.into_any())}
            <button type="submit" hidden=true></button>
        </form>
    }
    .into_any();
    let foot = save_bar(busy, Signal::derive(|| true), if is_new { "create" } else { "save" }, close, save, error);
    drawer(title, close, body, foot)
}

// ------------------------------------------------------------------ history

const HISTORY_PAGE: usize = 50;

pub(crate) fn history_drawer(task: ScheduledTask, tasks: Remote<Vec<TaskView>>, close: Callback<()>) -> AnyView {
    let id = task.id.clone();
    let runs = RwSignal::new(None::<Vec<TaskRun>>);
    let err = RwSignal::new(None::<String>);
    let more = RwSignal::new(false);
    let selected = RwSignal::new(None::<u64>);
    let output = RwSignal::new(None::<Result<TaskRun, String>>);
    let base = StoredValue::new(format!("/api/v1/scheduler/tasks/{}/history", api::encode(&id)));
    // the task runs now (the page's list: read every 15 s, and again when a live run ends)
    let id_r = id.clone();
    let running = Memo::new(move |_| tasks.data.with(|d| d.iter().flatten().any(|v| v.task.id == id_r && v.status.running)));
    // the live output is shown (instead of a recorded run): at first when the task runs
    let live = RwSignal::new(running.get_untracked());
    let live_run = RwSignal::new(None::<LiveRun>);
    let load = move |before: Option<u64>| {
        let mut url = format!("{}?limit={HISTORY_PAGE}", base.get_value());
        if let Some(b) = before {
            url.push_str(&format!("&before={b}"));
        }
        spawn_local(async move {
            match api::get::<Vec<TaskRun>>(&url).await {
                Ok(l) => {
                    let _ = more.try_set(l.len() >= HISTORY_PAGE);
                    if before.is_some() {
                        runs.try_update(|r| r.get_or_insert_with(Vec::new).extend(l));
                    } else {
                        if selected.try_get_untracked().flatten().is_none()
                            && live.try_get_untracked() == Some(false)
                            && let Some(first) = l.first()
                        {
                            open_run(base.get_value(), first.id, selected, output);
                        }
                        let _ = runs.try_set(Some(l));
                    }
                    let _ = err.try_set(None);
                }
                Err(e) if e.status() == 401 => {}
                Err(e) => {
                    let _ = err.try_set(Some(e.code().to_string()));
                }
            }
        });
    };
    load(None);
    // the live run ended: it becomes the newest history entry (recorded by the unit's last
    // step, its output copied a moment later); nothing was running: the newest entry
    let after_live = Callback::new(move |ran: bool| {
        tasks.reload();
        let inv = live_run.get_untracked().map(|r| r.invocation_id);
        spawn_local(async move {
            let url = format!("{}?limit={HISTORY_PAGE}", base.get_value());
            for attempt in 0..5u32 {
                if ran {
                    api::sleep(if attempt == 0 { 1200 } else { 1000 }).await;
                }
                let Ok(l) = api::get::<Vec<TaskRun>>(&url).await else { continue };
                let newest = l.first().cloned();
                let ready = newest.as_ref().is_some_and(|r| inv.as_deref().is_none_or(|i| r.invocation_id == i) && (r.output_bytes > 0 || attempt >= 3));
                if ran && !ready && attempt < 4 {
                    continue;
                }
                let _ = more.try_set(l.len() >= HISTORY_PAGE);
                let _ = runs.try_set(Some(l));
                if let Some(r) = newest {
                    if let Ok(full) = api::get::<TaskRun>(&format!("{}/{}", base.get_value(), r.id)).await {
                        let _ = output.try_set(Some(Ok(full)));
                    }
                    let _ = selected.try_set(Some(r.id));
                }
                break;
            }
            let _ = live.try_set(false);
        });
    });
    let show_live = move |_| {
        // already shown: setting it again would rebuild the view and restart its stream
        if live.get_untracked() {
            return;
        }
        selected.set(None);
        live.set(true);
    };
    let list = move || {
        if runs.with(Option::is_none) {
            return match err.get() {
                Some(code) => view! { <crate::ui::ErrorBox code=code on_retry=Callback::new(move |_| load(None)) /> }.into_any(),
                None => view! { <crate::ui::Skeleton lines=4 /> }.into_any(),
            };
        }
        let l = runs.get().unwrap_or_default();
        let now = running.get() || live.get();
        if l.is_empty() && !now {
            return view! { <EmptyState icon=icons::HISTORY title=Signal::derive(|| t("sch.no_runs").to_string()) text=Signal::derive(|| t("sch.no_runs_text").to_string()) /> }.into_any();
        }
        // the run going on now, first: its live output
        let current = now.then(|| {
            ui::row_flag(
                vec![
                    ui::cell("sch.col.started", "tnum", view! { {move || live_run.get().and_then(|r| r.start_ms).map(i18n::datetime).unwrap_or_else(|| ui::DASH.to_string())} }.into_any()),
                    ui::cell("sch.col.duration", "num tnum", view! { {ui::DASH} }.into_any()),
                    ui::cell("sch.col.result", "", ui::status(Tone::Info, t("sch.running").to_string())),
                    ui::actions(view! {
                        <button class="icon-btn sm" title=tr("sch.live") aria-label=tr("sch.live") aria-pressed=move || live.get().to_string() on:click=show_live>
                            <Icon icon=icons::ACTIVITY size=16 />
                        </button>
                    }
                    .into_any()),
                ],
                "selected",
                Signal::derive(move || live.get()),
            )
        });
        let rows = current.into_iter().chain(l.into_iter().map(|r| {
            let rid = r.id;
            let (tone, text) = run_status(&r);
            let started = r.start_ms.unwrap_or(r.end_ms);
            // a skipped start ran for no time at all
            let dur = match (r.skipped, r.start_ms) {
                (Some(_), _) => ui::DASH.to_string(),
                (None, Some(s)) => i18n::elapsed(r.end_ms.saturating_sub(s) / 1000),
                (None, None) => String::new(),
            };
            let label = format!("{}: {}", t("sch.show_output"), i18n::datetime(started));
            let origin = run_origin(&r);
            ui::row_flag(
                vec![
                    ui::cell("sch.col.started", "tnum", view! {
                        <div class="cell-main">
                            <span>{i18n::datetime(started)}</span>
                            {origin.map(|o| view! { <span class="cell-sub">{o}</span> })}
                        </div>
                    }
                    .into_any()),
                    ui::cell("sch.col.duration", "num tnum", view! { {dur} }.into_any()),
                    ui::cell("sch.col.result", "", view! { {ui::status(tone, text)} }.into_any()),
                    ui::actions(view! {
                        <button class="icon-btn sm" title=tr("sch.show_output") aria-label=label aria-pressed=move || (selected.get() == Some(rid)).to_string()
                            on:click=move |_| {
                                live.set(false);
                                open_run(base.get_value(), rid, selected, output);
                            }>
                            <Icon icon=icons::FILE_TEXT size=16 />
                        </button>
                    }
                    .into_any()),
                ],
                "selected",
                Signal::derive(move || !live.get() && selected.get() == Some(rid)),
            )
        }));
        view! {
            {ui::table("", &["sch.col.started", "#sch.col.duration", "sch.col.result", ""], rows.collect::<Vec<_>>())}
            <Show when=move || more.get()>
                <div class="load-more">
                    <button class="btn ghost sm" on:click=move |_| load(runs.with_untracked(|r| r.as_ref().and_then(|l| l.last().map(|x| x.id))))>{tr("nt.load_more")}</button>
                </div>
            </Show>
        }
        .into_any()
    };
    // the live view is built once per opening (its stream lives with it)
    let id_l = id.clone();
    let live_part = move || {
        live.get().then(|| {
            view! {
                <h3 class="mini-title">{tr("sch.live")}{move || live_run.get().and_then(|r| r.start_ms).map(|s| format!(" · {}", i18n::datetime(s)))}</h3>
                {scheduler_live::live_view(&id_l, Callback::new(move |r| live_run.set(Some(r))), after_live)}
            }
        })
    };
    let out = move || {
        if live.get() {
            return None;
        }
        output.get().map(|o| match o {
            Ok(r) => {
                // as a terminal shows it: a carriage return rewrites its line (rsync -P, curl)
                let text: String = r.output.as_deref().unwrap_or_default().lines().map(|l| scheduler_live::shown_line(l) + "\n").collect();
                view! {
                    <h3 class="mini-title">{tr("sch.output")}" · "{i18n::datetime(r.start_ms.unwrap_or(r.end_ms))}</h3>
                    {r.output_truncated.then(|| view! { <p class="field-help">{tf("sch.truncated", &[("size", &i18n::bytes(r.output_bytes))])}</p> })}
                    <pre class="log run-log" tabindex="0" aria-label=tr("sch.output")>{if text.is_empty() { t("sch.no_output").to_string() } else { text }}</pre>
                }
                .into_any()
            }
            Err(msg) => view! { <p class="form-error"><Icon icon=icons::CIRCLE_X size=16 />{msg}</p> }.into_any(),
        })
    };
    let title = {
        let n = if task.name.is_empty() { task.id.clone() } else { task.name.clone() };
        Signal::stored(tf("sch.history_title", &[("name", &n)]))
    };
    let body = view! { {list}{live_part}{out} }.into_any();
    let foot = ui::close_foot(close);
    view! { {ui::drawer_wide(title, close, body, foot)} }.into_any()
}

fn open_run(base: String, n: u64, selected: RwSignal<Option<u64>>, output: RwSignal<Option<Result<TaskRun, String>>>) {
    selected.set(Some(n));
    spawn_local(async move {
        match api::get::<TaskRun>(&format!("{base}/{n}")).await {
            Ok(r) if selected.get_untracked() == Some(n) => output.set(Some(Ok(r))),
            Ok(_) => {}
            Err(e) => output.set(Some(Err(e.text()))),
        }
    });
}

// ------------------------------------------------------------------ read-only views

fn timers_panel() -> AnyView {
    let timers = Remote::<Vec<SystemTimer>>::new("/api/v1/scheduler/system-timers");
    view! {
        <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("sch.timers_help")}</p>
        {timers.view(5, move || {
            let l = timers.data.get().unwrap_or_default();
            if l.is_empty() {
                return view! { <EmptyState icon=icons::CLOCK title=Signal::derive(|| t("sch.no_timers").to_string()) text=Signal::derive(String::new) /> }.into_any();
            }
            view! {
                {ui::table("", &["sch.col.timer", "sch.col.schedule", "sch.col.next", "sch.col.last"], l.into_iter().map(|tm| ui::row(vec![
                                ui::cell("sch.col.timer", "", view! { <div class="cell-main"><span class="mono">{tm.unit.clone()}</span><span class="cell-sub mono">{tm.activates.clone()}</span></div> }.into_any()),
                                ui::cell("sch.col.schedule", "mono", view! { {tm.on_calendar.join(" · ")} }.into_any()),
                                ui::cell("sch.col.next", "tnum", view! { {ui::when(tm.next_ms)} }.into_any()),
                                ui::cell("sch.col.last", "tnum", view! { {ui::when(tm.last_ms)} }.into_any()),
                            ])).collect::<Vec<_>>())}
            }
            .into_any()
        })}
    }
    .into_any()
}

fn cron_panel(edit: RwSignal<Option<(ScheduledTask, bool)>>) -> AnyView {
    let entries = Remote::<Vec<CronEntry>>::new("/api/v1/scheduler/cron-entries");
    view! {
        <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("sch.cron_help")}</p>
        {entries.view(4, move || {
            let l = entries.data.get().unwrap_or_default();
            if l.is_empty() {
                return view! { <EmptyState icon=icons::CLOCK title=Signal::derive(|| t("sch.no_cron").to_string()) text=Signal::derive(String::new) /> }.into_any();
            }
            view! {
                {ui::table("", &["sch.col.source", "sch.col.schedule", "sch.f.user", "sch.f.command", ""], l.into_iter().map(|c| {
                                let convertible = c.trigger.is_some();
                                let sched = c.trigger.as_ref().map(trigger_text).unwrap_or_default();
                                let c2 = c.clone();
                                let label = ui::verb_name("sch.convert", &c.command);
                                ui::row(vec![
                                    ui::cell("sch.col.source", "mono", view! { {format!("{}:{}", c.source, c.line)} }.into_any()),
                                    ui::cell("sch.col.schedule", "", view! { <div class="cell-main"><span class="mono">{c.schedule.clone()}</span><span class="cell-sub">{sched}</span></div> }.into_any()),
                                    ui::cell("sch.f.user", "mono", view! { {c.user.clone()} }.into_any()),
                                    ui::cell("sch.f.command", "mono ellipsis-cell", view! { {c.command.clone()} }.into_any()),
                                    ui::actions(view! {
                                        <button class="icon-btn sm" title=tr("sch.convert") aria-label=label disabled=!convertible
                                            on:click=move |_| {
                                                let mut task = new_task();
                                                task.action = TaskAction::Command { command: c2.command.clone(), user: c2.user.clone() };
                                                if let Some(tr) = c2.trigger.clone() {
                                                    task.trigger = tr;
                                                }
                                                task.description = format!("{}:{}", c2.source, c2.line);
                                                edit.set(Some((task, true)));
                                            }>
                                            <Icon icon=icons::PLUS size=16 />
                                        </button>
                                    }
                                    .into_any()),
                                ])
                            }).collect::<Vec<_>>())}
            }
            .into_any()
        })}
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedules_in_words() {
        assert_eq!(calendar_text("*-*-* 03:00:00"), "Daily at 03:00");
        assert_eq!(calendar_text("*-*-* 08,20:00"), "Daily at 08:00, 20:00");
        assert_eq!(calendar_text("Mon..Fri 02:30"), "Mon–Fri at 02:30");
        assert_eq!(calendar_text("Mon,Wed *-*-* 23:05:00"), "Mon, Wed at 23:05");
        assert_eq!(calendar_text("*-*-* *:00/15:00"), "Every 15 min");
        assert_eq!(calendar_text("Mon..Fri 08..18:00/15"), "Every 15 min, 08:00–18:45, Mon–Fri");
        assert_eq!(calendar_text("hourly"), "Hourly at :00");
        assert_eq!(calendar_text("*-*-* 00/6:30"), "Every 6 hours at :30");
        assert_eq!(calendar_text("*-*-* 08..20/4:00"), "Every 4 hours at :00, 08:00–20:00");
        assert_eq!(calendar_text("*-*-01,15 04:00"), "Monthly on day 1, 15 at 04:00");
        assert_eq!(calendar_text("*-*~01 23:00"), "Monthly on the last day at 23:00");
        assert_eq!(calendar_text("Sun *-*-01..07 02:30"), "Monthly on the first Sun at 02:30");
        assert_eq!(calendar_text("Fri *-*~07/1 18:00"), "Monthly on the last Fri at 18:00");
        assert_eq!(calendar_text("*-12-25 00:00"), "Yearly on 12-25 at 00:00");
        assert_eq!(calendar_text("quarterly"), "*-01,04,07,10-01 00:00:00");
        assert_eq!(calendar_text("nonsense"), "nonsense");
        assert_eq!(weekdays_text(Weekdays::of(&[0, 1, 2, 4, 5, 6])), "Mon–Wed, Fri–Sun");
        assert_eq!(weekdays_text(Weekdays::of(&[5, 6])), "Sat, Sun");
    }

    #[test]
    fn task_ids_and_slugs() {
        assert!(valid_task_id("nightly-rsync"));
        assert!(valid_task_id("0backup"));
        assert!(!valid_task_id("-x"));
        assert!(!valid_task_id("Nightly"));
        assert!(!valid_task_id(&"a".repeat(49)));
        assert_eq!(slug("Nightly rsync → bob's disk!"), "nightly-rsync-bob-s-disk");
        assert_eq!(slug("  "), "");
    }

    #[test]
    fn action_drafts() {
        let a = TaskAction::SmartTest { disk: "sda".into(), test: SmartTestKind::Long };
        assert_eq!(ActionDraft::from_action(&a).action().unwrap(), a);
        let mut d = ActionDraft { kind: "smart_test".into(), disk: "/dev/sdb".into(), test: "short".into(), ..Default::default() };
        assert_eq!(d.action().unwrap(), TaskAction::SmartTest { disk: "sdb".into(), test: SmartTestKind::Short });
        d.kind = "command".into();
        assert_eq!(d.action().unwrap_err(), "sch.v.command");
        d.command = "echo hi".into();
        assert_eq!(d.action().unwrap(), TaskAction::Command { command: "echo hi".into(), user: "root".into() });
        let r = ActionDraft { kind: "recycle_cleanup".into(), days: "x".into(), ..Default::default() };
        assert_eq!(r.action().unwrap_err(), "sch.v.days");
    }
}
