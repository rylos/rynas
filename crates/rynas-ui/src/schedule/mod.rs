//! The schedule editor shared by every page that schedules something (Task Scheduler,
//! snapshot jobs, replicas, backup jobs): run on a schedule, at boot or by hand; the presets of
//! `rynas-schedule` (every N minutes or hours, days and times, monthly by day or weekday,
//! yearly) or an advanced systemd calendar expression or crontab line; errors inline; a live
//! preview of the next runs in the NAS's time zone (`POST /api/v1/scheduler/preview`) with
//! the clock changes that touch the schedule and a warning when systemd would never run it.
//! Catch-up of missed runs and the random delay stay as they were. The windows, weekday chips
//! and times of the other pages that schedule something are in [`window`].

pub mod draft;
pub mod interval;
pub mod window;

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::scheduler::{
    CalendarSyntax, MissedPolicy, RunPolicy, SchedulePreview, SchedulePreviewRequest, TaskTrigger,
};
use rynas_schedule::recurrence::MINUTE_STEPS;
use rynas_schedule::Nth;

pub use draft::{Draft, DraftError, Preset};
pub use window::WeekWindowEditor;

use crate::api;
use crate::form::{In, number_field, segmented, select_field, switch, text_field};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::pages::scheduler::{DAY_KEYS, recurrence_text};
use crate::ui;

const TRIGGERS: &[(&str, &str)] = &[("calendar", "sch.trig.calendar"), ("boot", "sch.trig.boot"), ("manual", "sch.trig.manual")];
const TRIGGERS_TIMED: &[(&str, &str)] = &[("calendar", "sch.trig.calendar"), ("boot", "sch.trig.boot")];
const NTH_OPTIONS: &[(&str, &str)] =
    &[("first", "sch.f.nth.first"), ("second", "sch.f.nth.second"), ("third", "sch.f.nth.third"), ("fourth", "sch.f.nth.fourth"), ("last", "sch.f.nth.last")];
const NTHS: [Nth; 5] = [Nth::First, Nth::Second, Nth::Third, Nth::Fourth, Nth::Last];

/// Runs listed by the preview.
const PREVIEW_RUNS: u32 = 5;

/// State of the schedule part of an editor.
#[derive(Clone, Copy)]
pub struct ScheduleEditor {
    kind: RwSignal<String>,
    draft: RwSignal<Draft>,
    persistent: RwSignal<bool>,
    random_s: RwSignal<String>,
    boot_s: RwSignal<String>,
    preview: RwSignal<Option<SchedulePreview>>,
    checking: RwSignal<bool>,
    seq: StoredValue<u32>,
}

impl ScheduleEditor {
    pub fn new(trig: &TaskTrigger) -> Self {
        let (kind, draft, persistent, random_s, boot_s) = match trig {
            TaskTrigger::Calendar { on_calendar, cron, persistent, randomized_delay_s } => {
                ("calendar", Draft::from_calendar(on_calendar, cron.as_deref()), *persistent, randomized_delay_s.to_string(), "60".to_string())
            }
            TaskTrigger::Recurrence { recurrence } => {
                let mut d = Draft::default();
                d.set_recurrence(recurrence);
                ("calendar", d, true, "0".into(), "60".into())
            }
            TaskTrigger::Boot { delay_s } => ("boot", Draft::default(), true, "0".into(), delay_s.to_string()),
            // the kinds the Task Scheduler does not run yet are not offered: start from a daily run
            TaskTrigger::Manual | TaskTrigger::Interval { .. } | TaskTrigger::Window { .. } | TaskTrigger::After { .. } => {
                ("manual", Draft::default(), true, "0".into(), "60".into())
            }
        };
        let e = ScheduleEditor {
            kind: RwSignal::new(kind.into()),
            draft: RwSignal::new(draft),
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
            let expr = e.draft.with(Draft::expression);
            e.seq.update_value(|s| *s += 1);
            let mine = e.seq.get_value();
            let Some((expression, syntax)) = expr.ok().filter(|_| is_cal) else {
                e.preview.set(None);
                e.checking.set(false);
                return;
            };
            e.checking.set(true);
            set_timeout(
                move || {
                    if e.seq.get_value() != mine {
                        return;
                    }
                    spawn_local(async move {
                        let req = SchedulePreviewRequest { expression: Some(expression), syntax, count: Some(PREVIEW_RUNS), ..Default::default() };
                        let r = api::post::<_, SchedulePreview>("/api/v1/scheduler/preview", Some(&req)).await;
                        if e.seq.get_value() != mine {
                            return;
                        }
                        e.checking.set(false);
                        match r {
                            Ok(p) => e.preview.set(Some(p)),
                            Err(err) => e.preview.set(Some(SchedulePreview { valid: false, error: Some(err.text()), ..Default::default() })),
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
                let (expr, syntax) = self.draft.with_untracked(Draft::expression).map_err(DraftError::key)?;
                if self.preview.with_untracked(|p| p.as_ref().is_some_and(|p| !p.valid)) {
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

    /// Writes what the editor shows into a task's run policy (the policy is the reference for
    /// catch-up and random delay; a catch-up deadline set elsewhere is kept).
    pub fn apply_policy(self, p: &mut RunPolicy) {
        if self.kind.get_untracked() != "calendar" {
            return;
        }
        let within_h = match p.missed {
            Some(MissedPolicy::RunOnce { within_h }) => within_h,
            _ => None,
        };
        p.missed = Some(if self.persistent.get_untracked() { MissedPolicy::RunOnce { within_h } } else { MissedPolicy::Skip });
        p.jitter_s = Some(self.random_s.with_untracked(|v| v.trim().parse().unwrap_or(0)));
    }

    /// Trigger kind, preset fields, inline errors, preview of the next runs.
    pub fn view(self, allow_manual: bool) -> AnyView {
        let e = self;
        let kind = Memo::new(move |_| e.kind.get());
        let body = move || match kind.get().as_str() {
            "calendar" => view! {
                {select_field(
                    "sch.f.repeat",
                    None,
                    Preset::OPTIONS,
                    Signal::derive(move || e.draft.with(|d| d.preset.code().to_string())),
                    Callback::new(move |v: String| e.draft.update(|d| d.switch_to(Preset::parse(&v)))),
                )}
                {fields(e)}
                {draft_error(e)}
                {preview(e)}
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

/// A select whose options are not i18n keys (numbers, hours, month names).
fn pick(label: &'static str, help: Option<&'static str>, opts: Vec<(String, String)>, value: Signal<String>, set: Callback<String>) -> AnyView {
    let id = crate::form::fid();
    let hid = help.map(|_| format!("{id}h"));
    view! {
        <div class="field">
            <label class="field-label" for=id.clone()>{tr(label)}</label>
            <select id=id class="input" aria-describedby=hid.clone() on:change=move |ev| set.run(event_target_value(&ev))>
                {opts.into_iter().map(|(v, text)| {
                    let v2 = v.clone();
                    view! { <option value=v selected=move || value.with(|x| *x == v2)>{text}</option> }
                }).collect_view()}
            </select>
            {help.zip(hid).map(|(h, id)| view! { <span class="field-help" id=id>{tr(h)}</span> })}
        </div>
    }
    .into_any()
}

/// A field of the draft as a text signal and its setter.
fn bind(e: ScheduleEditor, get: fn(&Draft) -> String, set: fn(&mut Draft, String)) -> (Signal<String>, Callback<String>) {
    (Signal::derive(move || e.draft.with(get)), Callback::new(move |v| e.draft.update(|d| set(d, v))))
}

fn hours(e: ScheduleEditor) -> AnyView {
    let opts = || (0..24u32).map(|h| (h.to_string(), i18n::hm(h, 0))).collect::<Vec<_>>();
    let (fv, fs) = bind(e, |d| d.from.to_string(), |d, v| d.from = v.parse().unwrap_or(0));
    let (tv, ts) = bind(e, |d| d.to.to_string(), |d, v| d.to = v.parse().unwrap_or(23));
    view! {
        <div class="form-row">
            {pick("sch.f.from_hour", None, opts(), fv, fs)}
            {pick("sch.f.to_hour", Some("sch.f.hours_help"), opts(), tv, ts)}
        </div>
    }
    .into_any()
}

fn weekday_chips(e: ScheduleEditor, help: Option<&'static str>) -> AnyView {
    window::day_chips("sch.f.weekdays", help, move |d| e.draft.with(|x| x.day_on(d)), move |d, on| e.draft.update(|x| x.toggle_day(d, on)))
}

fn times(e: ScheduleEditor) -> AnyView {
    let list = move || {
        let n = e.draft.with(|d| d.times.len());
        (0..n)
            .map(|i| {
                let label = tf("sch.f.time_n", &[("n", &(i + 1).to_string())]);
                view! {
                    <div class="sched-time">
                        <input class="input tnum" type="time" aria-label=label.clone()
                            prop:value=move || e.draft.with(|d| d.times.get(i).cloned().unwrap_or_default())
                            on:input=move |ev| { let v = event_target_value(&ev); e.draft.update(|d| if let Some(t) = d.times.get_mut(i) { *t = v; }); } />
                        {(n > 1).then(|| ui::icon_btn(icons::X, tf("sch.f.remove_time", &[("n", &(i + 1).to_string())]), false, Signal::derive(|| false), Callback::new(move |_| e.draft.update(|d| d.remove_time(i)))))}
                    </div>
                }
            })
            .collect_view()
    };
    view! {
        <fieldset class="fieldset">
            <legend class="field-label">{tr("sch.f.times")}</legend>
            <div class="sched-times">
                {list}
                <button type="button" class="btn ghost sm" on:click=move |_| e.draft.update(Draft::add_time)>
                    <Icon icon=icons::PLUS size=14 />{tr("sch.f.add_time")}
                </button>
            </div>
        </fieldset>
    }
    .into_any()
}

fn fields(e: ScheduleEditor) -> impl IntoView {
    let preset = Memo::new(move |_| e.draft.with(|d| d.preset));
    let none = Signal::derive(|| None::<String>);
    move || match preset.get() {
        Preset::Minutes => {
            let steps = MINUTE_STEPS.iter().map(|m| (m.to_string(), m.to_string())).collect();
            let (v, s) = bind(e, |d| d.every_min.to_string(), |d, v| d.every_min = v.parse().unwrap_or(15));
            view! { {pick("sch.f.every_min", None, steps, v, s)}{hours(e)}{weekday_chips(e, Some("sch.f.days_any"))} }.into_any()
        }
        Preset::Hours => {
            let (v, s) = bind(e, |d| d.every_h.to_string(), |d, v| d.every_h = v.parse().unwrap_or(6));
            let (mv, ms) = bind(e, |d| d.minute.to_string(), |d, v| d.minute = v.parse().unwrap_or(0));
            view! {
                <div class="form-row">
                    {number_field("sch.f.every_hours", None, v, s, 1, 23)}
                    {number_field("sch.f.minute", Some("sch.f.minute_help"), mv, ms, 0, 59)}
                </div>
                {hours(e)}
                {weekday_chips(e, Some("sch.f.days_any"))}
            }
            .into_any()
        }
        Preset::Daily => times(e),
        Preset::Weekly => view! { {weekday_chips(e, None)}{times(e)} }.into_any(),
        Preset::Monthly => {
            let (v, s) = bind(e, |d| d.month_days.clone(), |d, v| d.month_days = v);
            let last = Signal::derive(move || e.draft.with(|d| d.last_day));
            view! {
                {switch("sch.f.last_day", None, last, Callback::new(move |v| e.draft.update(|d| d.last_day = v)))}
                {move || (!last.get()).then(|| text_field("sch.f.month_days", v, s, none, In { help: Some("sch.f.month_days_help"), placeholder: "1, 15", ..In::default() }))}
                {times(e)}
            }
            .into_any()
        }
        Preset::MonthlyNth => {
            let (nv, ns) = bind(
                e,
                |d| NTH_OPTIONS[NTHS.iter().position(|n| *n == d.nth).unwrap_or(0)].0.to_string(),
                |d, v| d.nth = NTH_OPTIONS.iter().position(|(c, _)| *c == v).map_or(Nth::First, |i| NTHS[i]),
            );
            let days = (0..7u8).map(|d| (d.to_string(), t(DAY_KEYS[d as usize]).to_string())).collect();
            let (wv, ws) = bind(e, |d| d.weekday.to_string(), |d, v| d.weekday = v.parse().unwrap_or(6));
            view! {
                <div class="form-row">
                    {select_field("sch.f.nth", None, NTH_OPTIONS, nv, ns)}
                    {pick("sch.f.weekday", None, days, wv, ws)}
                </div>
                {times(e)}
            }
            .into_any()
        }
        Preset::Yearly => {
            let months = (1..=12u32).map(|m| (m.to_string(), i18n::month_name(m))).collect();
            let (mv, ms) = bind(e, |d| d.month.to_string(), |d, v| d.month = v.parse().unwrap_or(1));
            let (dv, ds) = bind(e, |d| d.day.to_string(), |d, v| d.day = v.parse().unwrap_or(1));
            view! {
                <div class="form-row">
                    {pick("sch.f.month", None, months, mv, ms)}
                    {number_field("sch.f.day", None, dv, ds, 1, 31)}
                </div>
                {times(e)}
            }
            .into_any()
        }
        Preset::Advanced => {
            let (v, s) = bind(e, |d| d.advanced.clone(), |d, v| d.advanced = v);
            text_field("sch.f.expression", v, s, none, In { mono: true, help: Some("sch.f.expression_help"), placeholder: "Mon..Fri *-*-* 02:30:00", ..In::default() })
        }
        Preset::Cron => {
            let (v, s) = bind(e, |d| d.cron.clone(), |d, v| d.cron = v);
            text_field("sch.f.cron", v, s, none, In { mono: true, help: Some("sch.f.cron_help"), placeholder: "0 4 * * *", ..In::default() })
        }
    }
}

/// What the presets cannot make, at once (no round trip).
fn draft_error(e: ScheduleEditor) -> impl IntoView {
    move || {
        e.draft.with(Draft::expression).err().map(|err| {
            view! { <p class="field-error" role="alert"><Icon icon=icons::CIRCLE_X size=12 /><span class="err-text">{t(err.key())}</span></p> }
        })
    }
}

fn notice(icon: &'static str, tone: &'static str, text: String) -> AnyView {
    view! { <p class=format!("sched-note {tone}")><Icon icon=icon size=14 /><span>{text}</span></p> }.into_any()
}

/// The next runs in the NAS's time zone, the schedule in words, the clock changes that touch
/// it, and what systemd would refuse or never run.
fn preview(e: ScheduleEditor) -> impl IntoView {
    move || {
        let p = e.preview.get();
        let checking = e.checking.get();
        let local_ok = e.draft.with(|d| d.expression().is_ok());
        if !local_ok {
            return ().into_any();
        }
        let spinner = move || checking.then(|| view! { " " <span class="btn-spinner" aria-hidden="true"></span> });
        let body = match p {
            Some(p) if p.valid => {
                let words = match &p.preset {
                    Some(r) => view! { <p class="sched-words">{recurrence_text(r)}</p> }.into_any(),
                    None => (!p.normalized.is_empty())
                        .then(|| view! { <span class="field-help">{tr("sch.normalized")}" "<code class="mono">{p.normalized.clone()}</code></span> })
                        .into_any(),
                };
                let head = if p.tz.is_empty() { t("sch.next_runs").to_string() } else { tf("sch.p.next_in", &[("tz", &p.tz)]) };
                let runs = p
                    .runs
                    .iter()
                    .map(|r| view! { <li>{i18n::wall_datetime(&r.local)}{(!r.abbreviation.is_empty()).then(|| view! { " " <span class="muted">{r.abbreviation.clone()}</span> })}</li> })
                    .collect_view();
                let mut notes: Vec<AnyView> = Vec::new();
                if p.no_next_run {
                    notes.push(notice(icons::CIRCLE_X, "err", t("sch.p.no_next").to_string()));
                } else if p.stops {
                    let date = p.stops_on.as_deref().map(i18n::wall_date).unwrap_or_default();
                    notes.push(notice(icons::TRIANGLE_ALERT, "warn", tf("sch.p.stops", &[("date", &date)])));
                }
                for d in &p.dst {
                    let hm = |s: &str| s.split_once(':').map_or(s.to_string(), |(h, m)| i18n::hm(h.parse().unwrap_or(0), m.parse().unwrap_or(0)));
                    let key = if d.forward { "sch.p.dst_forward" } else { "sch.p.dst_back" };
                    notes.push(notice(icons::CLOCK, "info", tf(key, &[("date", &i18n::wall_date(&d.date)), ("from", &hm(&d.from)), ("to", &hm(&d.to))])));
                }
                view! {
                    {words}
                    {(!p.runs.is_empty()).then(|| view! {
                        <span class="field-label">{head}{spinner}</span>
                        <ol class="plain-list tnum">{runs}</ol>
                    })}
                    {(!notes.is_empty()).then(|| view! { <div class="sched-notes" role="status">{notes}</div> })}
                }
                .into_any()
            }
            Some(p) => view! {
                <p class="field-error" role="alert"><Icon icon=icons::CIRCLE_X size=12 /><span class="err-text">{tr("sch.v.calendar")}{p.error.filter(|e| !e.is_empty()).map(ui::tech_detail)}</span></p>
            }
            .into_any(),
            None if checking => view! { <span class="muted small">{tr("sch.checking")}</span> }.into_any(),
            None => ().into_any(),
        };
        view! { <div class="cal-preview" aria-live="polite">{body}</div> }.into_any()
    }
}
