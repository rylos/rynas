//! The schedule of a PC's backups (S6 of `docs/research/scheduling.md`): on schedule or
//! paused, and when: "every N hours since the last backup", at set times (the shared calendar
//! editor of every other schedule: presets, weekdays, a systemd calendar expression, its next
//! times), or at every start of the PC; the delay after the PC starts; an optional allowed
//! start window (the shared weekly grid); the next starts from the server preview computed
//! with the agent's own rules (`rynas_schedule::window`, the NAS's time zone). One component
//! for both places a PC's plan is set (Backup › PCs: file backups, and the btrfs folder's
//! Settings).

use leptos::prelude::*;
use rynas_api_types::pcagent::PcPlan;
use rynas_api_types::scheduler::{SchedulePreviewRequest, TaskTrigger, week_window_valid};

use super::ScheduleEditor;
use super::window::NextRuns;
use crate::form::{number_field, segmented, select_field, switch};
use crate::i18n::{t, tf, tr};
use crate::week_grid::{GridLevel, GridTone, week_grid};

/// The grid's two levels: no backup starts / backups may start.
const LEVELS: &[GridLevel] = &[
    GridLevel { code: '0', label_key: "sv.grid_off", tone: GridTone::Off },
    GridLevel { code: '1', label_key: "sv.grid_on", tone: GridTone::Full },
];

const PAUSED: &[(&str, &str)] = &[("on", "pca.f.scheduled"), ("paused", "pca.f.paused")];

/// When backups run: every N hours, or at set times (and, with its own switch, also at every
/// start of the PC).
const MODES: &[(&str, &str)] = &[("every", "pca.f.mode.every"), ("calendar", "pca.f.mode.calendar")];

/// The calendar a PC switched to set times starts from: every day at 02:00.
const DEFAULT_CALENDAR: &str = "*-*-* 02:00:00";

/// A new window: every night 22:00–06:00.
pub fn default_window() -> String {
    (0..168).map(|c| if !(6..22).contains(&(c % 24)) { '1' } else { '0' }).collect()
}

/// The agent is known and older than the first one that follows a window.
pub fn ignores_window(agent_version: &str) -> bool {
    !rynas_api_types::pcagent::agent_follows_window(agent_version)
}

/// The agent is known and older than the first one that follows set times and "at every start".
pub fn ignores_calendar(agent_version: &str) -> bool {
    !rynas_api_types::pcagent::agent_follows_calendar(agent_version)
}

/// State of a PC's schedule in a settings dialog.
#[derive(Clone, Copy)]
pub struct IntervalEditor {
    paused: RwSignal<String>,
    /// `every` or `calendar` ([`MODES`]).
    mode: RwSignal<String>,
    /// Also at every start of the PC.
    at_boot: RwSignal<bool>,
    every: RwSignal<String>,
    /// The set times: the calendar editor every other schedule uses.
    cal: ScheduleEditor,
    delay: RwSignal<String>,
    limit: RwSignal<bool>,
    grid: RwSignal<String>,
    next: NextRuns,
    agent: StoredValue<String>,
}

impl IntervalEditor {
    /// From the saved plan; `last_ok_ms`: the PC's last good backup (the next starts count from
    /// it); `agent_version`: the PC's agent (an older one is told to update for the window, the
    /// set times and "at every start").
    pub fn new(plan: &PcPlan, last_ok_ms: Option<u64>, agent_version: &str) -> Self {
        let paused = RwSignal::new(if plan.paused { "paused" } else { "on" }.to_string());
        let mode = RwSignal::new(if plan.calendar.is_some() { "calendar" } else { "every" }.to_string());
        let at_boot = RwSignal::new(plan.at_boot);
        let every = RwSignal::new(plan.interval_hours.to_string());
        let cal = ScheduleEditor::new(&TaskTrigger::Calendar {
            on_calendar: plan.calendar.clone().unwrap_or_else(|| DEFAULT_CALENDAR.into()),
            cron: None,
            persistent: true,
            randomized_delay_s: 0,
        });
        let delay = RwSignal::new(plan.start_delay_min.to_string());
        let limit = RwSignal::new(plan.window.is_some());
        let grid = RwSignal::new(plan.window.clone().filter(|w| week_window_valid(w)).unwrap_or_else(default_window));
        // the next starts with the agent's rules: from the last good backup, inside the window
        // (set times without a window: the calendar editor lists its times already; the starts
        // of the PC are not foreseeable)
        let next = NextRuns::of_request(move || {
            if paused.with(|p| p == "paused") {
                return None;
            }
            let window = if limit.get() { Some(grid.get()).filter(|w| week_window_valid(w)) } else { None };
            if limit.get() && window.is_none() {
                return None;
            }
            let trigger = match mode.get().as_str() {
                "calendar" if window.is_some() => {
                    TaskTrigger::Calendar { on_calendar: cal.expression_now()?, cron: None, persistent: true, randomized_delay_s: 0 }
                }
                "every" => TaskTrigger::Interval { hours: every.with(|e| e.trim().parse().ok()).filter(|h| (1..=168).contains(h))?, after_boot_min: 0 },
                _ => return None,
            };
            Some(SchedulePreviewRequest { trigger: Some(trigger), window, last_ms: last_ok_ms, ..Default::default() })
        });
        IntervalEditor { paused, mode, at_boot, every, cal, delay, limit, grid, next, agent: StoredValue::new(agent_version.to_string()) }
    }

    /// The i18n key of what is wrong (a window without any open hour, set times that do not
    /// make a schedule), if anything.
    pub fn problem(self) -> Option<&'static str> {
        if self.limit.get_untracked() && !self.grid.with_untracked(|g| week_window_valid(g)) {
            return Some("sch.allowed.empty");
        }
        if self.mode.get_untracked() == "calendar" {
            return self.cal.on_calendar().err();
        }
        None
    }

    /// Writes the editor's values into `plan` (the interval is kept with set times too:
    /// switching back finds it).
    pub fn apply(self, plan: &mut PcPlan) {
        let n = |s: RwSignal<String>| s.get_untracked().trim().parse::<u32>().unwrap_or(0);
        let mode = self.mode.get_untracked();
        plan.interval_hours = n(self.every).clamp(1, 168);
        plan.start_delay_min = n(self.delay);
        plan.paused = self.paused.get_untracked() == "paused";
        plan.window = self.limit.get_untracked().then(|| self.grid.get_untracked());
        plan.at_boot = self.at_boot.get_untracked();
        plan.calendar = if mode == "calendar" { self.cal.on_calendar().ok() } else { None };
    }

    pub fn view(self) -> AnyView {
        let e = self;
        let old_window = e.agent.with_value(|v| ignores_window(v));
        let old_calendar = e.agent.with_value(|v| ignores_calendar(v));
        let delay_field = move |help: &'static str| number_field("pca.f.delay", Some(help), e.delay.into(), Callback::new(move |v| e.delay.set(v)), 0, 1440);
        let body = move || match e.mode.get().as_str() {
            "calendar" => view! {
                {e.cal.view_calendar(super::Preset::DAILY_OPTIONS)}
                {delay_field("pca.f.delay_calendar_help")}
            }
            .into_any(),
            _ => view! {
                <div class="form-row">
                    {number_field("pca.f.interval", Some("pca.f.interval_help"), e.every.into(), Callback::new(move |v| e.every.set(v)), 1, 168)}
                    {delay_field("pca.f.delay_help")}
                </div>
            }
            .into_any(),
        };
        view! {
            {select_field("pca.f.state", None, PAUSED, e.paused.into(), Callback::new(move |v| e.paused.set(v)))}
            {segmented("pca.f.when", MODES, e.mode.into(), Callback::new(move |v| e.mode.set(v)))}
            {body}
            {switch("pca.f.at_boot", Some("pca.f.at_boot_help"), e.at_boot.into(), Callback::new(move |v| e.at_boot.set(v)))}
            {move || (old_calendar && (e.mode.get() != "every" || e.at_boot.get())).then(|| view! {
                <p class="field-help warn-text">{e.agent.with_value(|v| tf("pca.f.old_agent_mode", &[("version", v)]))}</p>
            })}
            {switch("sch.allowed.switch", Some("sch.allowed.help"), e.limit.into(), Callback::new(move |v| e.limit.set(v)))}
            <Show when=move || e.limit.get()>
                <div class="field">
                    {week_grid(LEVELS, e.grid, "sch.allowed.title", None)}
                    <span class="field-help">{tr("sch.allowed.grid_help")}</span>
                    {old_window.then(|| view! { <p class="field-help warn-text">{e.agent.with_value(|v| tf("sch.allowed.old_agent", &[("version", v)]))}</p> })}
                </div>
            </Show>
            {e.next.view(|| 0)}
            <Show when=move || e.paused.with(|p| p == "paused")>
                <p class="field-help">{t("sch.allowed.paused_help")}</p>
            </Show>
        }
        .into_any()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_and_windows() {
        assert!(ignores_window("0.8.19"));
        assert!(!ignores_window("0.8.20"));
        assert!(!ignores_window("0.9.0-rc1"));
        assert!(!ignores_window(""));
        assert!(ignores_calendar("0.8.20"));
        assert!(!ignores_calendar("0.8.21"));
        let w = default_window();
        assert!(week_window_valid(&w));
        assert_eq!(&w[..24], "111111000000000000000011");
    }
}
