//! The windows and times of day of the pages that schedule something besides the Task
//! Scheduler (power, updates, Docker, PC backups, notifications, deduplication): one
//! component, the same texts everywhere.
//!
//! - [`WeekWindowEditor`]: a daily window, its start (one `type=time` input) and its length
//!   in minutes. A window always means the same thing: work starts at a random time inside
//!   it, and nothing new starts after it ends (before S3 "window length" meant a random start
//!   on the Updates page and "start only inside" on the Docker page: both were true of both).
//! - [`day_chips`], [`time_field`], [`hour_select`]: the weekday chips (Monday first) and the
//!   time inputs shared with the schedule editor.
//! - [`NextRuns`]: the next runs or windows of a schedule, from the same server preview as the
//!   schedule editor (the NAS's time zone and its clock changes).

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::scheduler::{SchedulePreview, SchedulePreviewRequest, TaskTrigger};

use crate::api;
use crate::form::number_field;
use crate::i18n::{self, t, tf, tr};
use crate::pages::scheduler::DAY_KEYS;

/// Runs (or windows) listed.
const RUNS: u32 = 3;

/// Weekday chips in a fieldset, `day` 0 = Monday … 6 = Sunday.
pub fn day_chips(
    label: &'static str,
    help: Option<&'static str>,
    on: impl Fn(u8) -> bool + Copy + Send + Sync + 'static,
    toggle: impl Fn(u8, bool) + Copy + Send + Sync + 'static,
) -> AnyView {
    view! {
        <fieldset class="fieldset">
            <legend class="field-label">{tr(label)}</legend>
            {day_chips_group(None, on, toggle)}
            {help.map(|h| view! { <span class="field-help">{tr(h)}</span> })}
        </fieldset>
    }
    .into_any()
}

/// The chips alone (a row of a list: quiet hours), named by `aria_label`.
pub fn day_chips_group(
    aria_label: Option<&'static str>,
    on: impl Fn(u8) -> bool + Copy + Send + Sync + 'static,
    toggle: impl Fn(u8, bool) + Copy + Send + Sync + 'static,
) -> AnyView {
    let days = (0..7u8)
        .map(|d| {
            view! {
                <label class="check day">
                    <input type="checkbox" prop:checked=move || on(d) on:change=move |ev| toggle(d, event_target_checked(&ev)) />
                    {tr(DAY_KEYS[d as usize])}
                </label>
            }
        })
        .collect_view();
    view! { <div class="quiet-days" role=aria_label.map(|_| "group") aria-label=aria_label.map(tr)>{days}</div> }.into_any()
}

/// A time of day (`HH:MM`, local time of the NAS).
pub fn time_field(label: &'static str, help: Option<&'static str>, value: Signal<String>, set: Callback<String>) -> AnyView {
    let id = crate::form::fid();
    let hid = format!("{id}-h");
    view! {
        <div class="field">
            <label class="field-label" for=id.clone()>{tr(label)}</label>
            <input id=id class="input tnum" type="time" required aria-describedby=hid.clone() prop:value=move || value.get() on:input=move |ev| set.run(event_target_value(&ev)) />
            {help.map(|h| view! { <span class="field-help" id=hid>{tr(h)}</span> })}
        </div>
    }
    .into_any()
}

/// A whole hour of the day (00:00 … 23:00).
pub fn hour_select(label: &'static str, help: Option<&'static str>, value: Signal<u8>, set: Callback<u8>) -> AnyView {
    let id = crate::form::fid();
    let hid = help.map(|_| format!("{id}h"));
    view! {
        <div class="field">
            <label class="field-label" for=id.clone()>{tr(label)}</label>
            <select id=id class="input tnum" aria-describedby=hid.clone() on:change=move |ev| set.run(event_target_value(&ev).parse().unwrap_or(0))>
                {(0..24u8).map(|h| view! { <option value=h.to_string() selected=move || value.get() == h>{i18n::hm(u32::from(h), 0)}</option> }).collect_view()}
            </select>
            {help.zip(hid).map(|(h, id)| view! { <span class="field-help" id=id>{tr(h)}</span> })}
        </div>
    }
    .into_any()
}

/// `HH:MM` → hour and minute.
pub fn parse_hhmm(v: &str) -> Option<(u8, u8)> {
    let (h, m) = v.trim().split_once(':')?;
    let (h, m) = (h.parse::<u8>().ok()?, m.get(..2)?.parse::<u8>().ok()?);
    (h < 24 && m < 60).then_some((h, m))
}

pub fn hhmm(h: u8, m: u8) -> String {
    format!("{h:02}:{m:02}")
}

/// A daily calendar at `HH:MM`.
pub fn daily(h: u8, m: u8) -> TaskTrigger {
    TaskTrigger::Calendar { on_calendar: format!("*-*-* {h:02}:{m:02}:00"), cron: None, persistent: true, randomized_delay_s: 0 }
}

/// The next runs of a schedule (or, with a length, the next windows), asked from the server
/// 300 ms after the last change.
#[derive(Clone, Copy)]
pub struct NextRuns {
    preview: RwSignal<Option<SchedulePreview>>,
    seq: StoredValue<u32>,
}

impl NextRuns {
    /// `trigger`: the schedule now (`None`: nothing to show).
    pub fn new(trigger: impl Fn() -> Option<TaskTrigger> + Send + Sync + 'static) -> Self {
        let n = NextRuns { preview: RwSignal::new(None), seq: StoredValue::new(0) };
        Effect::new(move |_| {
            let trig = trigger();
            n.seq.update_value(|s| *s += 1);
            let mine = n.seq.get_value();
            let Some(trig) = trig else {
                n.preview.set(None);
                return;
            };
            set_timeout(
                move || {
                    if n.seq.get_value() != mine {
                        return;
                    }
                    spawn_local(async move {
                        let req = SchedulePreviewRequest { trigger: Some(trig), count: Some(RUNS), ..Default::default() };
                        let r = api::post::<_, SchedulePreview>("/api/v1/scheduler/preview", Some(&req)).await;
                        if n.seq.get_value() == mine {
                            n.preview.set(r.ok().filter(|p| p.valid));
                        }
                    });
                },
                Duration::from_millis(300),
            );
        });
        n
    }

    /// The list: "Next runs" (or "Next windows" with `length_min` > 0, each "start – end").
    pub fn view(self, length_min: impl Fn() -> u32 + Send + Sync + 'static) -> AnyView {
        (move || {
            let p = self.preview.get()?;
            if p.runs.is_empty() {
                return None;
            }
            let len = length_min();
            let head = if len > 0 { t("sch.win.next") } else { t("sch.next_runs") };
            let head = if p.tz.is_empty() { head.to_string() } else { format!("{head} ({})", p.tz) };
            let runs = p
                .runs
                .iter()
                .map(|r| {
                    let end = (len > 0).then(|| {
                        let (h, m) = r.local.get(11..16).and_then(parse_hhmm).unwrap_or((0, 0));
                        let e = (u32::from(h) * 60 + u32::from(m) + len) % 1440;
                        tf("sch.win.until", &[("time", &i18n::hm(e / 60, e % 60))])
                    });
                    view! { <li>{i18n::wall_datetime(&r.local)}{end.map(|e| view! { " " {e} })}</li> }
                })
                .collect_view();
            Some(view! {
                <div class="cal-preview" aria-live="polite">
                    <span class="field-label">{head}</span>
                    <ol class="plain-list tnum">{runs}</ol>
                </div>
            })
        })
        .into_any()
    }
}

/// A daily window: its start and its length in minutes, with the same help and the next
/// windows wherever it is edited (automatic system updates, Docker's automatic updates).
#[derive(Clone, Copy)]
pub struct WeekWindowEditor {
    start: RwSignal<String>,
    length: RwSignal<String>,
    next: NextRuns,
}

/// The bounds of a window's length, in minutes.
pub const WINDOW_MIN: u16 = 10;
pub const WINDOW_MAX: u16 = 360;

impl WeekWindowEditor {
    pub fn daily(hour: u8, minute: u8, length_min: u16) -> Self {
        let start = RwSignal::new(hhmm(hour, minute));
        let length = RwSignal::new(length_min.to_string());
        let next = NextRuns::new(move || {
            let (h, m) = start.with(|s| parse_hhmm(s))?;
            Some(daily(h, m))
        });
        WeekWindowEditor { start, length, next }
    }

    /// Start hour, minute and length when they are valid.
    pub fn value(self) -> Option<(u8, u8, u16)> {
        let (h, m) = self.start.with(|s| parse_hhmm(s))?;
        let len: u16 = self.length.with(|l| l.trim().parse().ok())?;
        (WINDOW_MIN..=WINDOW_MAX).contains(&len).then_some((h, m, len))
    }

    /// The window shown again from saved values (the form was reset).
    pub fn set(self, hour: u8, minute: u8, length_min: u16) {
        self.start.set(hhmm(hour, minute));
        self.length.set(length_min.to_string());
    }

    /// `on_change`: told after every edit (the page's draft follows the editor).
    pub fn view(self, on_change: Callback<()>) -> AnyView {
        let e = self;
        let length_now = move || e.length.with(|l| l.trim().parse::<u32>().unwrap_or(0));
        view! {
            <div class="form-row">
                {time_field("sch.win.start", None, e.start.into(), Callback::new(move |v| { e.start.set(v); on_change.run(()); }))}
                {number_field("sch.win.length", None, e.length.into(), Callback::new(move |v| { e.length.set(v); on_change.run(()); }), i64::from(WINDOW_MIN), i64::from(WINDOW_MAX))}
            </div>
            <p class="field-help">{tr("sch.win.help")}</p>
            {e.next.view(length_now)}
        }
        .into_any()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_of_day() {
        assert_eq!(parse_hhmm("03:05"), Some((3, 5)));
        assert_eq!(parse_hhmm("23:59:00"), Some((23, 59)));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(hhmm(4, 0), "04:00");
        assert!(matches!(daily(4, 30), TaskTrigger::Calendar { on_calendar, .. } if on_calendar == "*-*-* 04:30:00"));
    }
}
