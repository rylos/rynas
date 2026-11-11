//! Task Scheduler (D-TASKS; DSM Control Panel › Task Scheduler, OMV Scheduled Jobs): tasks
//! that rynasd turns into persistent systemd units, with next run, last result, enable/disable,
//! run now and the run history with its output; a calendar builder with a live preview of the
//! next runs (`POST /scheduler/tasks/preview-calendar`); read-only views of the system timers
//! and crontab entries found on the NAS ("create a task" from a cron line). Writes stage
//! pending changes (`/config/apply` applies them).

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::PendingOp;
use rynas_api_types::scheduler::{
    CalendarPreview, CalendarPreviewRequest, CalendarSyntax, CronEntry, NotifyPolicy, ScheduledTask, SmartTestKind, SystemTimer, TaskAction,
    TaskRun, TaskTrigger, TaskView,
};

use crate::api;
use crate::form::{In, area_field, lens, lens_bool, number_field, save_bar, segmented, select_field, switch, tabs, text_field};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::toast::{saved_applied, saved_pending, toast};
use crate::ui::{self, Confirm, EmptyState, PageHeader, Remote, Tone, confirm, drawer};

// ------------------------------------------------------------------ calendar builder (pure)

const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Repeat {
    Minutes,
    Hourly,
    Daily,
    Weekly,
    Monthly,
    Custom,
    Cron,
}

impl Repeat {
    fn code(self) -> &'static str {
        match self {
            Repeat::Minutes => "minutes",
            Repeat::Hourly => "hourly",
            Repeat::Daily => "daily",
            Repeat::Weekly => "weekly",
            Repeat::Monthly => "monthly",
            Repeat::Custom => "custom",
            Repeat::Cron => "cron",
        }
    }
    fn parse(s: &str) -> Repeat {
        [Repeat::Minutes, Repeat::Hourly, Repeat::Daily, Repeat::Weekly, Repeat::Monthly, Repeat::Custom, Repeat::Cron]
            .into_iter()
            .find(|r| r.code() == s)
            .unwrap_or(Repeat::Daily)
    }
}

/// What the calendar builder edits; [`Cal::expression`] turns it into `OnCalendar=` (or cron).
#[derive(Clone, PartialEq, Debug)]
pub struct Cal {
    pub repeat: Repeat,
    pub every_min: u32,
    pub minute: u32,
    /// "HH:MM".
    pub time: String,
    /// 1 = Monday … 7 = Sunday.
    pub days: Vec<u8>,
    pub dom: u32,
    pub custom: String,
    pub cron: String,
}

impl Default for Cal {
    fn default() -> Self {
        Cal { repeat: Repeat::Daily, every_min: 15, minute: 0, time: "03:00".into(), days: vec![1], dom: 1, custom: String::new(), cron: String::new() }
    }
}

fn hhmm(s: &str) -> Option<String> {
    let mut p = s.split(':');
    let h: u32 = p.next()?.parse().ok()?;
    let m: u32 = p.next()?.parse().ok()?;
    match p.next() {
        None | Some("00") | Some("0") => {}
        Some(_) => return None,
    }
    (h < 24 && m < 60).then(|| format!("{h:02}:{m:02}"))
}

impl Cal {
    /// The expression to preview and store, with its syntax.
    pub fn expression(&self) -> (String, CalendarSyntax) {
        let time = hhmm(&self.time).unwrap_or_else(|| "00:00".into());
        let e = match self.repeat {
            Repeat::Minutes => format!("*-*-* *:00/{:02}:00", self.every_min.clamp(1, 59)),
            Repeat::Hourly => format!("*-*-* *:{:02}:00", self.minute.min(59)),
            Repeat::Daily => format!("*-*-* {time}:00"),
            Repeat::Weekly if !self.days.is_empty() => {
                let mut d = self.days.clone();
                d.sort_unstable();
                d.dedup();
                let names: Vec<&str> = d.iter().filter(|x| (1..=7).contains(*x)).map(|x| WEEKDAYS[*x as usize - 1]).collect();
                format!("{} *-*-* {time}:00", names.join(","))
            }
            Repeat::Weekly => format!("*-*-* {time}:00"),
            Repeat::Monthly => format!("*-*-{:02} {time}:00", self.dom.clamp(1, 31)),
            Repeat::Custom => self.custom.trim().to_string(),
            Repeat::Cron => return (self.cron.trim().to_string(), CalendarSyntax::Cron),
        };
        (e, CalendarSyntax::Calendar)
    }

    /// Recognize the expressions the builder writes (and systemd's shortcuts); anything else
    /// opens as a custom expression.
    pub fn parse(expr: &str) -> Cal {
        let e = expr.trim();
        let mut c = Cal::default();
        let custom = |c: Cal| Cal { repeat: Repeat::Custom, custom: e.to_string(), ..c };
        match e {
            "hourly" => return Cal { repeat: Repeat::Hourly, ..c },
            "daily" => return Cal { repeat: Repeat::Daily, time: "00:00".into(), ..c },
            "weekly" => return Cal { repeat: Repeat::Weekly, time: "00:00".into(), ..c },
            "monthly" => return Cal { repeat: Repeat::Monthly, time: "00:00".into(), ..c },
            "" => return c,
            _ => {}
        }
        let parts: Vec<&str> = e.split_whitespace().collect();
        let (days, date, time) = match parts.as_slice() {
            [date, time] => (None, *date, *time),
            [days, date, time] => (Some(*days), *date, *time),
            _ => return custom(c),
        };
        if let Some(rest) = time.strip_prefix("*:") {
            if days.is_some() || date != "*-*-*" {
                return custom(c);
            }
            if let Some(n) = rest.strip_prefix("00/").or_else(|| rest.strip_prefix("0/")) {
                let n = n.trim_end_matches(":00");
                if let Ok(n) = n.parse::<u32>() {
                    return Cal { repeat: Repeat::Minutes, every_min: n, ..c };
                }
            } else if let Ok(m) = rest.trim_end_matches(":00").parse::<u32>()
                && rest.len() <= 5
            {
                return Cal { repeat: Repeat::Hourly, minute: m, ..c };
            }
            return custom(c);
        }
        let Some(t) = hhmm(time) else { return custom(c) };
        c.time = t;
        if let Some(days) = days {
            if date != "*-*-*" {
                return custom(c);
            }
            let mut out = Vec::new();
            for d in days.split(',') {
                match WEEKDAYS.iter().position(|w| w.eq_ignore_ascii_case(d)) {
                    Some(i) => out.push(i as u8 + 1),
                    None => return custom(c),
                }
            }
            return Cal { repeat: Repeat::Weekly, days: out, ..c };
        }
        if date == "*-*-*" {
            return Cal { repeat: Repeat::Daily, ..c };
        }
        if let Some(dd) = date.strip_prefix("*-*-")
            && let Ok(d) = dd.parse::<u32>()
            && (1..=31).contains(&d)
        {
            return Cal { repeat: Repeat::Monthly, dom: d, ..c };
        }
        custom(c)
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
            let c = Cal::parse(on_calendar);
            match c.repeat {
                Repeat::Minutes => tf("sch.desc.minutes", &[("n", &c.every_min.to_string())]),
                Repeat::Hourly => tf("sch.desc.hourly", &[("m", &format!("{:02}", c.minute))]),
                Repeat::Daily => tf("sch.desc.daily", &[("time", &c.time)]),
                Repeat::Weekly => {
                    let days: Vec<&str> = c.days.iter().map(|d| t(DAY_KEYS[*d as usize - 1])).collect();
                    tf("sch.desc.weekly", &[("days", &days.join(", ")), ("time", &c.time)])
                }
                Repeat::Monthly => tf("sch.desc.monthly", &[("d", &c.dom.to_string()), ("time", &c.time)]),
                Repeat::Custom | Repeat::Cron => on_calendar.clone(),
            }
        }
    }
}

pub const DAY_KEYS: [&str; 7] = ["day.1", "day.2", "day.3", "day.4", "day.5", "day.6", "day.7"];

// ------------------------------------------------------------------ trigger editor

const TRIGGERS: &[(&str, &str)] = &[("calendar", "sch.trig.calendar"), ("boot", "sch.trig.boot"), ("manual", "sch.trig.manual")];
const TRIGGERS_TIMED: &[(&str, &str)] = &[("calendar", "sch.trig.calendar"), ("boot", "sch.trig.boot")];
const REPEATS: &[(&str, &str)] = &[
    ("minutes", "sch.rep.minutes"),
    ("hourly", "sch.rep.hourly"),
    ("daily", "sch.rep.daily"),
    ("weekly", "sch.rep.weekly"),
    ("monthly", "sch.rep.monthly"),
    ("custom", "sch.rep.custom"),
    ("cron", "sch.rep.cron"),
];

/// State of the schedule part of an editor (shared with the snapshot jobs).
#[derive(Clone, Copy)]
pub struct TriggerEdit {
    kind: RwSignal<String>,
    cal: RwSignal<Cal>,
    persistent: RwSignal<bool>,
    random_s: RwSignal<String>,
    boot_s: RwSignal<String>,
    preview: RwSignal<Option<CalendarPreview>>,
    checking: RwSignal<bool>,
    seq: StoredValue<u32>,
}

impl TriggerEdit {
    pub fn new(trig: &TaskTrigger) -> Self {
        let (kind, cal, persistent, random_s, boot_s) = match trig {
            TaskTrigger::Calendar { on_calendar, cron, persistent, randomized_delay_s } => {
                let cal = if on_calendar.is_empty() && cron.is_some() {
                    Cal { repeat: Repeat::Cron, cron: cron.clone().unwrap_or_default(), ..Cal::default() }
                } else {
                    Cal::parse(on_calendar)
                };
                ("calendar", cal, *persistent, randomized_delay_s.to_string(), "60".to_string())
            }
            TaskTrigger::Boot { delay_s } => ("boot", Cal::default(), true, "0".into(), delay_s.to_string()),
            TaskTrigger::Manual => ("manual", Cal::default(), true, "0".into(), "60".into()),
        };
        let e = TriggerEdit {
            kind: RwSignal::new(kind.into()),
            cal: RwSignal::new(cal),
            persistent: RwSignal::new(persistent),
            random_s: RwSignal::new(random_s),
            boot_s: RwSignal::new(boot_s),
            preview: RwSignal::new(None),
            checking: RwSignal::new(false),
            seq: StoredValue::new(0),
        };
        // live preview of the next runs, 300 ms after the last change
        Effect::new(move |_| {
            let is_cal = e.kind.with(|k| k == "calendar");
            let (expr, syntax) = e.cal.with(Cal::expression);
            e.seq.update_value(|s| *s += 1);
            let mine = e.seq.get_value();
            if !is_cal {
                return;
            }
            if expr.is_empty() {
                e.preview.set(None);
                return;
            }
            e.checking.set(true);
            set_timeout(
                move || {
                    if e.seq.get_value() != mine {
                        return;
                    }
                    spawn_local(async move {
                        let req = CalendarPreviewRequest { expression: expr, syntax, count: Some(5) };
                        let r = api::post::<_, CalendarPreview>("/api/v1/scheduler/tasks/preview-calendar", Some(&req)).await;
                        if e.seq.get_value() != mine {
                            return;
                        }
                        e.checking.set(false);
                        match r {
                            Ok(p) => e.preview.set(Some(p)),
                            Err(err) => e.preview.set(Some(CalendarPreview { valid: false, error: Some(err.text()), ..Default::default() })),
                        }
                    });
                },
                Duration::from_millis(300),
            );
        });
        e
    }

    /// The trigger to save, or the i18n key of what is wrong.
    pub fn trigger(self) -> Result<TaskTrigger, &'static str> {
        let num = |s: RwSignal<String>| s.with_untracked(|v| v.trim().parse::<u32>().ok());
        match self.kind.get_untracked().as_str() {
            "boot" => Ok(TaskTrigger::Boot { delay_s: num(self.boot_s).ok_or("sch.v.delay")? }),
            "manual" => Ok(TaskTrigger::Manual),
            _ => {
                if self.preview.with_untracked(|p| p.as_ref().is_some_and(|p| !p.valid)) {
                    return Err("sch.v.calendar");
                }
                let (expr, syntax) = self.cal.with_untracked(Cal::expression);
                if expr.is_empty() {
                    return Err("sch.v.calendar");
                }
                let (on_calendar, cron) = if syntax == CalendarSyntax::Cron { (String::new(), Some(expr)) } else { (expr, None) };
                Ok(TaskTrigger::Calendar {
                    on_calendar,
                    cron,
                    persistent: self.persistent.get_untracked(),
                    randomized_delay_s: num(self.random_s).unwrap_or(0),
                })
            }
        }
    }

    /// Trigger kind, calendar builder, preview of the next runs.
    pub fn view(self, allow_manual: bool) -> AnyView {
        let e = self;
        let none = Signal::derive(|| None::<String>);
        let repeat = Memo::new(move |_| e.cal.with(|c| c.repeat));
        let cal_text = move |get: fn(&Cal) -> String, set: fn(&mut Cal, String)| (Signal::derive(move || e.cal.with(get)), Callback::new(move |v| e.cal.update(|c| set(c, v))));
        let builder = move || {
            let time = {
                let (v, s) = cal_text(|c| c.time.clone(), |c, v| c.time = v);
                text_field("sch.f.time", v, s, none, In { kind: "time", ..In::default() })
            };
            match repeat.get() {
                Repeat::Minutes => {
                    let (v, s) = cal_text(|c| c.every_min.to_string(), |c, v| c.every_min = v.parse().unwrap_or(15));
                    number_field("sch.f.every_min", None, v, s, 1, 59)
                }
                Repeat::Hourly => {
                    let (v, s) = cal_text(|c| c.minute.to_string(), |c, v| c.minute = v.parse().unwrap_or(0));
                    number_field("sch.f.minute", Some("sch.f.minute_help"), v, s, 0, 59)
                }
                Repeat::Daily => time,
                Repeat::Weekly => {
                    let days = (1..=7u8)
                        .map(|d| {
                            view! {
                                <label class="check day">
                                    <input type="checkbox" prop:checked=move || e.cal.with(|c| c.days.contains(&d))
                                        on:change=move |ev| { let on = event_target_checked(&ev); e.cal.update(|c| { c.days.retain(|x| *x != d); if on { c.days.push(d); c.days.sort_unstable(); } }); } />
                                    {tr(DAY_KEYS[d as usize - 1])}
                                </label>
                            }
                        })
                        .collect_view();
                    view! {
                        <fieldset class="fieldset">
                            <legend class="field-label">{tr("sch.f.weekdays")}</legend>
                            <div class="quiet-days">{days}</div>
                        </fieldset>
                        {time}
                    }
                    .into_any()
                }
                Repeat::Monthly => {
                    let (v, s) = cal_text(|c| c.dom.to_string(), |c, v| c.dom = v.parse().unwrap_or(1));
                    view! { <div class="form-row">{number_field("sch.f.dom", Some("sch.f.dom_help"), v, s, 1, 31)}{time}</div> }.into_any()
                }
                Repeat::Custom => {
                    let (v, s) = cal_text(|c| c.custom.clone(), |c, v| c.custom = v);
                    text_field("sch.f.expression", v, s, none, In { mono: true, help: Some("sch.f.expression_help"), placeholder: "Mon..Fri *-*-* 02:30:00", ..In::default() })
                }
                Repeat::Cron => {
                    let (v, s) = cal_text(|c| c.cron.clone(), |c, v| c.cron = v);
                    text_field("sch.f.cron", v, s, none, In { mono: true, help: Some("sch.f.cron_help"), placeholder: "0 4 * * *", ..In::default() })
                }
            }
        };
        let preview = move || {
            let p = e.preview.get();
            let checking = e.checking.get();
            view! {
                <div class="cal-preview" aria-live="polite">
                    {match p {
                        Some(p) if p.valid => view! {
                            <span class="field-label">{tr("sch.next_runs")}{checking.then(|| view! { " " <span class="btn-spinner" aria-hidden="true"></span> })}</span>
                            <ol class="plain-list tnum">{p.next_ms.iter().map(|m| view! { <li>{i18n::datetime(*m)}</li> }).collect_view()}</ol>
                            {(!p.normalized.is_empty()).then(|| view! { <span class="field-help">{tr("sch.normalized")}" "<code class="mono">{p.normalized.clone()}</code></span> })}
                        }.into_any(),
                        Some(p) => view! {
                            <p class="field-error" role="alert"><Icon icon=icons::CIRCLE_X size=12 /><span class="err-text">{tr("sch.v.calendar")}{p.error.filter(|e| !e.is_empty()).map(ui::tech_detail)}</span></p>
                        }.into_any(),
                        None if checking => view! { <span class="muted small">{tr("sch.checking")}</span> }.into_any(),
                        None => ().into_any(),
                    }}
                </div>
            }
        };
        let kind = Memo::new(move |_| e.kind.get());
        let body = move || match kind.get().as_str() {
            "calendar" => view! {
                {select_field("sch.f.repeat", None, REPEATS, Signal::derive(move || e.cal.with(|c| c.repeat.code().to_string())), Callback::new(move |v: String| e.cal.update(|c| c.repeat = Repeat::parse(&v))))}
                {builder}
                {preview}
                {switch("sch.f.persistent", Some("sch.f.persistent_help"), e.persistent.into(), Callback::new(move |v| e.persistent.set(v)))}
                {number_field("sch.f.random", Some("sch.f.random_help"), e.random_s.into(), Callback::new(move |v| e.random_s.set(v)), 0, 86400)}
            }
            .into_any(),
            "boot" => number_field("sch.f.boot_delay", Some("sch.f.boot_delay_help"), e.boot_s.into(), Callback::new(move |v| e.boot_s.set(v)), 0, 86400),
            _ => view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("sch.trig.manual_help")}</p> }.into_any(),
        };
        view! {
            {segmented("sch.f.trigger", if allow_manual { TRIGGERS } else { TRIGGERS_TIMED }, e.kind.into(), Callback::new(move |v| e.kind.set(v)))}
            {body}
        }
        .into_any()
    }
}

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

/// Outcome of a run: tone and text ("Succeeded", "Failed (exit 2)", "Timed out").
pub fn run_status(r: &TaskRun) -> (Tone, String) {
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

// ------------------------------------------------------------------ page

#[component]
pub fn Scheduler() -> impl IntoView {
    let tab = RwSignal::new(0usize);
    let tasks = Remote::<Vec<TaskView>>::with_config("/api/v1/scheduler/tasks");
    // next runs and running state move by themselves: read again every 15 s while visible
    if let Ok(h) = set_interval_with_handle(
        move || {
            if !document().hidden() && crate::auth::auth().is_ready() {
                tasks.reload();
            }
        },
        Duration::from_secs(15),
    ) {
        on_cleanup(move || h.clear());
    }
    let edit = RwSignal::new(None::<(ScheduledTask, bool)>);
    view! {
        <div class="page">
            <PageHeader icon=icons::CALENDAR_CLOCK title="sch.title" subtitle="sch.subtitle">
                <button class="btn primary" on:click=move |_| edit.set(Some((new_task(), true)))>
                    <Icon icon=icons::PLUS size=16 /><span class="btn-label">{tr("sch.create")}</span>
                </button>
            </PageHeader>
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
        </div>
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
    }
}

fn tasks_panel(tasks: Remote<Vec<TaskView>>, edit: RwSignal<Option<(ScheduledTask, bool)>>) -> AnyView {
    let q = RwSignal::new(String::new());
    let history = RwSignal::new(None::<ScheduledTask>);
    let del = RwSignal::new(None::<(String, String)>);
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
                    <EmptyState icon=icons::CALENDAR_CLOCK title=Signal::derive(|| t("sch.empty").to_string()) text=Signal::derive(|| t("sch.empty_text").to_string())>
                        <button class="btn primary" on:click=move |_| edit.set(Some((new_task(), true)))><Icon icon=icons::PLUS size=16 />{tr("sch.create")}</button>
                    </EmptyState>
                }
                .into_any()
            } else {
                view! { <p class="muted table-none">{tr("table.no_match")}</p> }.into_any()
            };
        }
        view! {
            {ui::table("", &["sch.col.enabled", "sch.col.task", "sch.col.action", "sch.col.schedule", "sch.col.last", ""], rows.into_iter().map(|v| task_row(v, tasks, edit, history, del)).collect::<Vec<_>>())}
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
        {move || history.get().map(|t| history_drawer(t, Callback::new(move |_| history.set(None))))}
        {deleter}
    }
    .into_any()
}

fn task_row(
    v: TaskView,
    tasks: Remote<Vec<TaskView>>,
    edit: RwSignal<Option<(ScheduledTask, bool)>>,
    history: RwSignal<Option<ScheduledTask>>,
    del: RwSignal<Option<(String, String)>>,
) -> AnyView {
    let task = v.task.clone();
    let st = v.status.clone();
    let id = task.id.clone();
    let name = if task.name.is_empty() { task.id.clone() } else { task.name.clone() };
    let deleting = st.pending == Some(PendingOp::Delete);
    // a deduplication task follows its window in Storage: read-only here
    let managed = managed_href(&task.action);
    let is_managed = managed.is_some();
    let fixed = is_managed || matches!(task.action, TaskAction::Snapshot { .. } | TaskAction::Replicate { .. } | TaskAction::PowerShutdown { .. });
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
    let run = move |_| {
        running.set(true);
        let (id, name) = (id_r.clone(), name_r.clone());
        spawn_local(async move {
            let r = api::post_empty::<()>(&format!("/api/v1/scheduler/tasks/{}/run", api::encode(&id)), None).await;
            match r {
                Ok(()) => {
                    toast(Tone::Info, tf("sch.started", &[("name", &name)]));
                    set_timeout(move || tasks.reload(), Duration::from_millis(800));
                    let done = follow_run(&id, before).await;
                    running.try_set(false);
                    match done {
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
    let (t_edit, t_hist) = (task.clone(), task.clone());
    let (n1, n2, n3, n4, n5) = (name.clone(), name.clone(), name.clone(), name.clone(), name.clone());
    let id_d = id.clone();
    ui::row_class(
        vec![
            ui::cell("sch.col.enabled", "switch-col", view! {
                <input type="checkbox" role="switch" class="switch" aria-label=format!("{}: {}", t("sch.col.enabled"), n1)
                prop:checked=enabled disabled=move || toggling.get() || deleting || is_managed
                on:change=move |ev| toggle(event_target_checked(&ev)) />
            }
            .into_any()),
            ui::cell("sch.col.task", "", view! {
                <div class="cell-main">
                    {match managed.clone() {
                        Some(href) => view! { <a class="link-strong" href=href title=tr("sch.managed_storage")>{name.clone()}</a> }.into_any(),
                        None => view! { <button class="link-strong" disabled=deleting on:click=move |_| edit.set(Some((t_edit.clone(), false)))>{name.clone()}</button> }.into_any(),
                    }}
                    {is_managed.then(|| view! { <span class="cell-sub">{tr("sch.managed_storage")}</span> })}
                    <span class="cell-sub mono">{task.id.clone()}</span>
                    {(!task.description.is_empty()).then(|| view! { <span class="cell-sub">{task.description.clone()}</span> })}
                </div>
            }
            .into_any()),
            ui::cell("sch.col.action", "", view! {
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
                    {st.running.then(|| ui::status(Tone::Info, t("sch.running").to_string()))}
                    {if st.running { None } else { last.map(|l| l.into_any()) }}
                    {ui::pending_status(st.pending)}
                    {(st.last_run.is_none() && !st.running && st.pending.is_none()).then(|| view! { <span class="muted small">{tr("never_run")}</span> })}
                </div>
            }
            .into_any()),
            ui::actions(view! {
                <button class="icon-btn sm" title=tr("run_now") aria-label=format!("{}: {}", t("run_now"), n2)
                    disabled=move || running.get() || st.running || !st.applied || is_managed on:click=run>
                    <Icon icon=icons::PLAY size=16 />
                </button>
                <button class="icon-btn sm" title=tr("sch.history") aria-label=format!("{}: {}", t("sch.history"), n3) on:click=move |_| history.set(Some(t_hist.clone()))>
                    <Icon icon=icons::HISTORY size=16 />
                </button>
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

fn task_editor(init: ScheduledTask, is_new: bool, close: Callback<()>) -> AnyView {
    let internal_fixed = !is_new && init.action.is_internal();
    let action_summary = action_text(&init.action);
    let trig = TriggerEdit::new(&init.trigger);
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
        match trig.trigger() {
            Ok(tr) => task.trigger = tr,
            Err(k) => {
                error.set(Some(t(k).to_string()));
                return;
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
        let n = d.with_untracked(|t| if t.name.is_empty() { t.id.clone() } else { t.name.clone() });
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
            {trig.view(true)}
            <h3 class="form-section">{tr("sch.sec.notify")}</h3>
            {select_field("sch.f.notify", None, NOTIFY, notify, set_notify)}
            {switch("sch.f.notify_output", Some("sch.f.notify_output_help"), notify_output, set_notify_output)}
            <details class="advanced">
                <summary>{tr("advanced")}</summary>
                <div class="form-row three">
                    {number_field("sch.f.timeout", Some("sch.f.timeout_help"), timeout, set_timeout_s, 1, 604800)}
                    {number_field("sch.f.nice", Some("sch.f.nice_help"), nice, set_nice, -20, 19)}
                    {number_field("sch.f.io_weight", Some("sch.f.io_weight_help"), iow, set_iow, 1, 10000)}
                </div>
            </details>
            <button type="submit" hidden=true></button>
        </form>
    }
    .into_any();
    let foot = save_bar(busy, Signal::derive(|| true), if is_new { "create" } else { "save" }, close, save, error);
    drawer(title, close, body, foot)
}

// ------------------------------------------------------------------ history

const HISTORY_PAGE: usize = 50;

fn history_drawer(task: ScheduledTask, close: Callback<()>) -> AnyView {
    let id = task.id.clone();
    let runs = RwSignal::new(None::<Vec<TaskRun>>);
    let err = RwSignal::new(None::<String>);
    let more = RwSignal::new(false);
    let selected = RwSignal::new(None::<u64>);
    let output = RwSignal::new(None::<Result<TaskRun, String>>);
    let base = StoredValue::new(format!("/api/v1/scheduler/tasks/{}/history", api::encode(&id)));
    let load = move |before: Option<u64>| {
        let mut url = format!("{}?limit={HISTORY_PAGE}", base.get_value());
        if let Some(b) = before {
            url.push_str(&format!("&before={b}"));
        }
        spawn_local(async move {
            match api::get::<Vec<TaskRun>>(&url).await {
                Ok(l) => {
                    more.set(l.len() >= HISTORY_PAGE);
                    if before.is_some() {
                        runs.update(|r| r.get_or_insert_with(Vec::new).extend(l));
                    } else {
                        if selected.get_untracked().is_none()
                            && let Some(first) = l.first()
                        {
                            open_run(base.get_value(), first.id, selected, output);
                        }
                        runs.set(Some(l));
                    }
                    err.set(None);
                }
                Err(e) if e.status() == 401 => {}
                Err(e) => err.set(Some(e.code().to_string())),
            }
        });
    };
    load(None);
    let list = move || {
        if runs.with(Option::is_none) {
            return match err.get() {
                Some(code) => view! { <crate::ui::ErrorBox code=code on_retry=Callback::new(move |_| load(None)) /> }.into_any(),
                None => view! { <crate::ui::Skeleton lines=4 /> }.into_any(),
            };
        }
        let l = runs.get().unwrap_or_default();
        if l.is_empty() {
            return view! { <EmptyState icon=icons::HISTORY title=Signal::derive(|| t("sch.no_runs").to_string()) text=Signal::derive(|| t("sch.no_runs_text").to_string()) /> }.into_any();
        }
        view! {
            {ui::table("", &["sch.col.started", "#sch.col.duration", "sch.col.result", ""], l.into_iter().map(|r| {
                            let rid = r.id;
                            let (tone, text) = run_status(&r);
                            let started = r.start_ms.unwrap_or(r.end_ms);
                            let dur = r.start_ms.map(|s| i18n::elapsed(r.end_ms.saturating_sub(s) / 1000)).unwrap_or_default();
                            let label = format!("{}: {}", t("sch.show_output"), i18n::datetime(started));
                            ui::row_flag(
                                vec![
                                    ui::cell("sch.col.started", "tnum", view! { {i18n::datetime(started)} }.into_any()),
                                    ui::cell("sch.col.duration", "num tnum", view! { {dur} }.into_any()),
                                    ui::cell("sch.col.result", "", view! { {ui::status(tone, text)} }.into_any()),
                                    ui::actions(view! {
                                        <button class="icon-btn sm" title=tr("sch.show_output") aria-label=label aria-pressed=move || (selected.get() == Some(rid)).to_string()
                                            on:click=move |_| open_run(base.get_value(), rid, selected, output)>
                                            <Icon icon=icons::FILE_TEXT size=16 />
                                        </button>
                                    }
                                    .into_any()),
                                ],
                                "selected",
                                Signal::derive(move || selected.get() == Some(rid)),
                            )
                        }).collect::<Vec<_>>())}
            <Show when=move || more.get()>
                <div class="load-more">
                    <button class="btn ghost sm" on:click=move |_| load(runs.with_untracked(|r| r.as_ref().and_then(|l| l.last().map(|x| x.id))))>{tr("nt.load_more")}</button>
                </div>
            </Show>
        }
        .into_any()
    };
    let out = move || {
        output.get().map(|o| match o {
            Ok(r) => {
                let text = r.output.clone().unwrap_or_default();
                view! {
                    <h3 class="drawer-h">{tr("sch.output")}" · "{i18n::datetime(r.start_ms.unwrap_or(r.end_ms))}</h3>
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
    let body = view! { {list}{out} }.into_any();
    let foot = view! { <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("close")}</button> }.into_any();
    view! { <div class="wide-drawer">{drawer(title, close, body, foot)}</div> }.into_any()
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
                                ui::cell("sch.col.next", "tnum", view! { {tm.next_ms.map(i18n::datetime).unwrap_or_else(|| "—".into())} }.into_any()),
                                ui::cell("sch.col.last", "tnum", view! { {tm.last_ms.map(i18n::datetime).unwrap_or_else(|| "—".into())} }.into_any()),
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
                                let label = format!("{}: {}", t("sch.convert"), c.command);
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

    fn roundtrip(c: Cal) {
        let (e, _) = c.expression();
        assert_eq!(Cal::parse(&e), c, "{e}");
    }

    #[test]
    fn calendar_builder_roundtrips() {
        roundtrip(Cal { repeat: Repeat::Daily, time: "02:30".into(), ..Cal::default() });
        roundtrip(Cal { repeat: Repeat::Weekly, days: vec![1, 5], time: "03:00".into(), ..Cal::default() });
        roundtrip(Cal { repeat: Repeat::Monthly, dom: 15, time: "04:05".into(), ..Cal::default() });
        roundtrip(Cal { repeat: Repeat::Hourly, minute: 7, ..Cal::default() });
        roundtrip(Cal { repeat: Repeat::Minutes, every_min: 15, ..Cal::default() });
    }

    #[test]
    fn calendar_expressions() {
        let c = Cal { repeat: Repeat::Weekly, days: vec![5, 1, 1], time: "3:00".into(), ..Cal::default() };
        assert_eq!(c.expression().0, "Mon,Fri *-*-* 03:00:00");
        assert_eq!(Cal { repeat: Repeat::Monthly, dom: 1, time: "04:00".into(), ..Cal::default() }.expression().0, "*-*-01 04:00:00");
        assert_eq!(Cal { repeat: Repeat::Cron, cron: " 0 4 * * * ".into(), ..Cal::default() }.expression(), ("0 4 * * *".into(), CalendarSyntax::Cron));
        // shortcuts and systemd's own forms
        assert_eq!(Cal::parse("hourly").repeat, Repeat::Hourly);
        assert_eq!(Cal::parse("daily").time, "00:00");
        assert_eq!(Cal::parse("*-*-* 02:30").time, "02:30");
        assert_eq!(Cal::parse("*:0/10").repeat, Repeat::Custom); // no date part: kept as written
        let odd = Cal::parse("Mon..Fri 02:30");
        assert_eq!((odd.repeat, odd.custom.as_str()), (Repeat::Custom, "Mon..Fri 02:30"));
        assert_eq!(Cal::parse("Sat,Sun *-*-* 09:15:00").days, vec![6, 7]);
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
