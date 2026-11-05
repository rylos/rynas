//! Hardware › Power schedule (DSM "Power Schedule"): weekly startup and shutdown rules
//! (`/api/v1/power/schedule`, staged like every configuration change) and the next times of
//! the applied schedule with the RTC wake alarm and the last boot (`next` of the same answer).
//!
//! The editor refuses what the API refuses (startup and shutdown at the same minute, a
//! shutdown less than 10 minutes after a startup, across midnight and the week too) before
//! anything is sent.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::PendingOp;
use rynas_api_types::power::*;
use wasm_bindgen::JsValue;

use crate::api;
use crate::form::{lens, lens_bool, number_field, save_bar, segmented, switch};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::pages::scheduler::DAY_KEYS;
use crate::toast::{saved_pending, toast};
use crate::ui::{self, Confirm, EmptyState, Remote, Tone, WidgetHead, cell, confirm, drawer, row, status, table, text};

const URL: &str = "/api/v1/power/schedule";
const WEEK_MIN: u32 = 7 * 1440;
/// A shutdown must come at least this long after a startup (the API's rule).
const MIN_UP_MIN: u32 = 10;
const ACTIONS: &[(&str, &str)] = &[("startup", "ps.startup"), ("shutdown", "ps.shutdown")];
const BUSY: &[(&str, &str)] = &[("wait", "ps.busy_wait"), ("skip", "ps.busy_skip"), ("force", "ps.busy_force")];

fn week_minutes(r: &PowerRule) -> impl Iterator<Item = u32> + '_ {
    r.days.iter().map(move |d| u32::from(d.index()) * 1440 + u32::from(r.hour) * 60 + u32::from(r.minute))
}

fn day_time(m: u32) -> String {
    format!("{} {:02}:{:02}", t(DAY_KEYS[(m / 1440) as usize % 7]), (m % 1440) / 60, m % 60)
}

/// The first conflict between enabled rules, as the API checks them (`None`: no conflict).
pub fn conflict(rules: &[PowerRule]) -> Option<String> {
    let on = |a: PowerAction| rules.iter().filter(move |r| r.enabled && r.action == a);
    for up in on(PowerAction::Startup) {
        for down in on(PowerAction::Shutdown) {
            for s in week_minutes(up) {
                for e in week_minutes(down) {
                    let delta = (e + WEEK_MIN - s) % WEEK_MIN;
                    if delta == 0 {
                        return Some(tf("ps.v_same", &[("when", &day_time(s))]));
                    }
                    if delta < MIN_UP_MIN {
                        return Some(tf("ps.v_close", &[("down", &day_time(e)), ("up", &day_time(s))]));
                    }
                }
            }
        }
    }
    None
}

/// A free rule id: "shutdown-2330", then "shutdown-2330-2"...
pub fn new_id(action: PowerAction, hour: u8, minute: u8, taken: &[&str]) -> String {
    let base = format!("{}-{hour:02}{minute:02}", if action == PowerAction::Startup { "startup" } else { "shutdown" });
    if !taken.contains(&base.as_str()) {
        return base;
    }
    (2..).map(|n| format!("{base}-{n}")).find(|c| !taken.contains(&c.as_str())).unwrap_or(base)
}

/// "Every day", "Mon–Fri", "Sat, Sun", "Mon, Wed".
pub fn days_text(days: &[Weekday]) -> String {
    let mut v: Vec<u8> = days.iter().map(|d| d.index()).collect();
    v.sort_unstable();
    v.dedup();
    match v.as_slice() {
        [0, 1, 2, 3, 4, 5, 6] => t("ps.every_day").to_string(),
        [0, 1, 2, 3, 4] => format!("{}–{}", t(DAY_KEYS[0]), t(DAY_KEYS[4])),
        _ => v.iter().map(|&i| t(DAY_KEYS[i as usize])).collect::<Vec<_>>().join(", "),
    }
}

fn hhmm(r: &PowerRule) -> String {
    format!("{:02}:{:02}", r.hour, r.minute)
}

fn parse_hhmm(v: &str) -> Option<(u8, u8)> {
    let (h, m) = v.trim().split_once(':')?;
    let (h, m) = (h.parse::<u8>().ok()?, m.get(..2)?.parse::<u8>().ok()?);
    (h < 24 && m < 60).then_some((h, m))
}

/// "today 23:30", "tomorrow 07:00", "Mon 07:00" (within a week), else the date.
fn when(ms: u64) -> String {
    let d = js_sys::Date::new(&JsValue::from_f64(ms as f64));
    let now = js_sys::Date::new_0();
    let day = |x: &js_sys::Date| (x.get_full_year(), x.get_month(), x.get_date());
    let tomorrow = js_sys::Date::new(&JsValue::from_f64(now.get_time() + 86_400_000.0));
    let time = i18n::time_hm(ms as f64);
    if day(&d) == day(&now) {
        format!("{} {time}", t("ps.today"))
    } else if day(&d) == day(&tomorrow) {
        format!("{} {time}", t("ps.tomorrow"))
    } else if (ms as f64) - now.get_time() < 6.0 * 86_400_000.0 {
        // getDay: 0 = Sunday
        format!("{} {time}", t(DAY_KEYS[(d.get_day() as usize + 6) % 7]))
    } else {
        i18n::datetime(ms)
    }
}

async fn stage(rules: Vec<PowerRule>) -> Result<(), api::ApiErr> {
    api::put::<_, PowerScheduleState>(URL, &PowerSchedule { rules }).await.map(|_| ())
}

fn staged() {
    saved_pending();
    crate::config::changed();
}

fn effective(s: &PowerScheduleState) -> Vec<PowerRule> {
    s.pending.as_ref().unwrap_or(&s.applied).rules.clone()
}

// ------------------------------------------------------------------ panel

pub fn panel(_active: Signal<bool>) -> AnyView {
    let st = Remote::<PowerScheduleState>::with_config(URL);
    // Some(None): a new rule
    let edit = RwSignal::new(None::<Option<PowerRule>>);
    let del = RwSignal::new(None::<String>);
    let rules = Signal::derive(move || st.data.with(|s| s.as_ref().map(effective).unwrap_or_default()));
    let wake_ok = Signal::derive(move || st.data.with(|s| s.as_ref().is_none_or(|s| s.next.wake.supported)));
    let list = move || {
        if let Some(v) = st.gate(4) {
            return v;
        }
        let s = st.data.get().unwrap_or_default();
        let rs = effective(&s);
        if rs.is_empty() {
            return view! {
                <EmptyState icon=icons::CALENDAR_CLOCK title=Signal::derive(|| t("ps.none_title").to_string()) text=Signal::derive(|| t("ps.none_text").to_string())>
                    <button class="btn primary" on:click=move |_| edit.set(Some(None))><Icon icon=icons::PLUS size=16 />{tr("add_rule")}</button>
                </EmptyState>
            }
            .into_any();
        }
        let rows = rs.iter().map(|r| rule_row(r.clone(), &s, rules, edit, del)).collect();
        table("ps-table", &["ps.col_on", "ps.col_action", "sch.f.weekdays", "sch.f.time", "ps.col_busy", ""], rows)
    };
    let deleter = move || {
        del.get().map(|id| {
            let busy = RwSignal::new(false);
            let error = RwSignal::new(None::<String>);
            let close = Callback::new(move |_| del.set(None));
            let target = id.clone();
            confirm(Confirm {
                title: t("ps.delete_title").to_string(),
                text: t("ps.delete_text").to_string(),
                action: "delete",
                danger: true,
                extra: None,
                typed: Signal::stored(None),
                busy,
                error,
                on_confirm: Callback::new(move |_| {
                    busy.set(true);
                    let rs: Vec<PowerRule> = rules.get_untracked().into_iter().filter(|r| r.id != target).collect();
                    spawn_local(async move {
                        let r = stage(rs).await;
                        busy.set(false);
                        match r {
                            Ok(()) => {
                                staged();
                                close.run(());
                            }
                            Err(e) => error.set(Some(e.text())),
                        }
                    });
                }),
                on_cancel: close,
            })
        })
    };
    view! {
        {next_card(st)}
        <section class="card widget w-12">
            <div class="toolbar">
                <h2 class="section-title ps-title"><Icon icon=icons::CALENDAR_CLOCK size=16 />{tr("ps.title")}</h2>
                <button class="btn primary" on:click=move |_| edit.set(Some(None)) disabled=move || !st.ready()>
                    <Icon icon=icons::PLUS size=16 /><span class="btn-label">{tr("add_rule")}</span>
                </button>
            </div>
            <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("ps.help")}</p>
            {list}
        </section>
        {move || edit.get().map(|r| editor(r, rules, wake_ok, Callback::new(move |_| edit.set(None))))}
        {deleter}
    }
    .into_any()
}

fn kv(label: &'static str, value: AnyView) -> AnyView {
    view! { <div class="kv"><dt>{tr(label)}</dt><dd>{value}</dd></div> }.into_any()
}

fn next_card(st: Remote<PowerScheduleState>) -> AnyView {
    let body = move || {
        if let Some(v) = st.gate(4) {
            return v;
        }
        let s = st.data.get().unwrap_or_default();
        let n = s.next;
        let next = |ms: Option<u64>, rule: Option<String>| match ms {
            Some(ms) => view! { <strong class="tnum">{when(ms)}</strong>{rule.map(|r| view! { <span class="muted small mono">" · "{r}</span> })} }.into_any(),
            None => text(t("ps.none").to_string()),
        };
        let startup = if n.wake.supported { next(n.next_startup_ms, n.next_startup_rule.clone()) } else { status(Tone::Warn, t("ps.wake_unsupported").to_string()) };
        let armed = match (n.wake.armed_ms, &n.wake.rtc) {
            (Some(ms), rtc) => text(format!("{}{}", i18n::datetime(ms), rtc.as_ref().map(|r| format!(" ({r})")).unwrap_or_default())),
            (None, _) => text(t("ps.not_armed").to_string()),
        };
        let arm_error = n.wake.last_armed.as_ref().and_then(|a| a.error.clone());
        let boot = n.last_boot.as_ref().map(|b| {
            let key = match b.cause {
                BootCause::ScheduledStartup => "ps.cause_scheduled",
                BootCause::Watchdog => "ps.cause_watchdog",
                BootCause::Unknown => "ps.cause_unknown",
            };
            let clean = match b.previous_shutdown_clean {
                Some(true) => t("ps.clean_yes"),
                Some(false) => t("ps.clean_no"),
                None => "—",
            };
            view! {
                {kv("last_boot", text(format!("{} · {}", i18n::datetime(b.boot_ms), t(key))))}
                {kv("ps.prev_clean", text(clean.to_string()))}
            }
        });
        view! {
            <div class="ps-next">
                <div class="ps-next-item"><Icon icon=icons::PLAY size=18 /><div><span class="muted small">{tr("ps.next_startup")}</span><div>{startup}</div></div></div>
                <div class="ps-next-item"><Icon icon=icons::UNPLUG size=18 /><div><span class="muted small">{tr("ps.next_shutdown")}</span><div>{next(n.next_shutdown_ms, n.next_shutdown_rule.clone())}</div></div></div>
            </div>
            {s.pending.is_some().then(|| view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{t("ps.pending_note")}</p> })}
            {n.wake.warning.clone().map(|w| view! { {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {w} }.into_any())} })}
            {arm_error.map(|e| view! { {ui::note(Tone::Err, icons::CIRCLE_X, view! { {tf("ps.arm_error", &[("e", &e)])} }.into_any())} })}
            <dl class="kvs">
                {kv("ps.time_zone", text(n.time_zone.clone()))}
                {kv("ps.wake_alarm", armed)}
                {boot}
            </dl>
        }
        .into_any()
    };
    view! {
        <section class="card widget">
            <WidgetHead icon=icons::CLOCK title="ps.next" />
            {body}
        </section>
    }
    .into_any()
}

fn rule_row(r: PowerRule, s: &PowerScheduleState, rules: Signal<Vec<PowerRule>>, edit: RwSignal<Option<Option<PowerRule>>>, del: RwSignal<Option<String>>) -> AnyView {
    let pending = s.rules.iter().find(|x| x.id == r.id).and_then(|x| x.pending);
    let toggling = RwSignal::new(false);
    let id = r.id.clone();
    // a refused change puts the switch back
    let toggle = move |el: web_sys::HtmlInputElement| {
        let on = el.checked();
        let mut rs = rules.get_untracked();
        if let Some(x) = rs.iter_mut().find(|x| x.id == id) {
            x.enabled = on;
        }
        if let Some(msg) = conflict(&rs) {
            toast(Tone::Err, msg);
            el.set_checked(!on);
            return;
        }
        toggling.set(true);
        spawn_local(async move {
            let res = stage(rs).await;
            toggling.set(false);
            match res {
                Ok(()) => staged(),
                Err(e) => {
                    el.set_checked(!on);
                    toast(Tone::Err, e.text());
                }
            }
        });
    };
    let action = if r.action == PowerAction::Startup {
        view! { <span class="ps-action up"><Icon icon=icons::PLAY size=14 />{t("ps.startup")}</span> }.into_any()
    } else {
        view! { <span class="ps-action down"><Icon icon=icons::UNPLUG size=14 />{t("ps.shutdown")}</span> }.into_any()
    };
    let busy = match (r.action, r.shutdown) {
        (PowerAction::Shutdown, Some(p)) => match p.when_busy {
            BusyPolicy::Wait => tf("ps.busy_wait_n", &[("n", &p.max_wait_min.to_string())]),
            BusyPolicy::Skip => t("ps.busy_skip").to_string(),
            BusyPolicy::Force => t("ps.busy_force").to_string(),
        },
        _ => "—".to_string(),
    };
    let pend = pending.map(|p| {
        let key = match p {
            PendingOp::Add => "pending.add",
            PendingOp::Change => "pending.change",
            PendingOp::Delete => "pending.delete",
        };
        status(Tone::Info, t(key).to_string())
    });
    let label = format!("{} {} {}", t(if r.action == PowerAction::Startup { "ps.startup" } else { "ps.shutdown" }), days_text(&r.days), hhmm(&r));
    let enabled = r.enabled;
    let (re, rd) = (r.clone(), r.id.clone());
    row(vec![
        cell(
            "ps.col_on",
            "",
            view! {
                <input type="checkbox" role="switch" class="switch" aria-label=format!("{}: {label}", t("ps.col_on"))
                    prop:checked=enabled disabled=move || toggling.get()
                    on:change=move |ev| toggle(event_target::<web_sys::HtmlInputElement>(&ev)) />
            }
            .into_any(),
        ),
        cell("ps.col_action", "", view! { {action}{pend} }.into_any()),
        cell("sch.f.weekdays", "", text(days_text(&r.days))),
        cell("sch.f.time", "tnum", text(hhmm(&r))),
        cell("ps.col_busy", "muted", text(busy)),
        cell(
            "",
            "num",
            view! {
                <div class="row-actions">
                    <button class="icon-btn sm" aria-label=format!("{}: {label}", t("edit")) on:click=move |_| edit.set(Some(Some(re.clone())))>
                        <Icon icon=icons::PENCIL size=16 />
                    </button>
                    <button class="icon-btn sm danger" aria-label=format!("{}: {label}", t("delete")) on:click=move |_| del.set(Some(rd.clone()))>
                        <Icon icon=icons::TRASH_2 size=16 />
                    </button>
                </div>
            }
            .into_any(),
        ),
    ])
}

// ------------------------------------------------------------------ editor

fn editor(orig: Option<PowerRule>, rules: Signal<Vec<PowerRule>>, wake_ok: Signal<bool>, close: Callback<()>) -> AnyView {
    let is_new = orig.is_none();
    let orig_id = StoredValue::new(orig.as_ref().map(|r| r.id.clone()));
    let d = RwSignal::new(orig.unwrap_or(PowerRule {
        id: String::new(),
        enabled: true,
        action: PowerAction::Shutdown,
        days: Weekday::ALL.to_vec(),
        hour: 23,
        minute: 0,
        shutdown: Some(ShutdownPolicy::default()),
    }));
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let (action, set_action) = lens(
        d,
        |r| if r.action == PowerAction::Startup { "startup".into() } else { "shutdown".into() },
        |r, v| r.action = if v == "startup" { PowerAction::Startup } else { PowerAction::Shutdown },
    );
    let time = RwSignal::new(d.with_untracked(hhmm));
    let (enabled, set_enabled) = lens_bool(d, |r| r.enabled, |r, v| r.enabled = v);
    let (when_busy, set_when_busy) = lens(
        d,
        |r| match r.shutdown.unwrap_or_default().when_busy {
            BusyPolicy::Wait => "wait".into(),
            BusyPolicy::Skip => "skip".into(),
            BusyPolicy::Force => "force".into(),
        },
        |r, v| {
            r.shutdown.get_or_insert_with(ShutdownPolicy::default).when_busy = match v.as_str() {
                "skip" => BusyPolicy::Skip,
                "force" => BusyPolicy::Force,
                _ => BusyPolicy::Wait,
            }
        },
    );
    let (wait, set_wait) = lens(
        d,
        |r| r.shutdown.unwrap_or_default().max_wait_min.to_string(),
        |r, v| r.shutdown.get_or_insert_with(ShutdownPolicy::default).max_wait_min = v.trim().parse().unwrap_or(0),
    );
    let (c_storage, set_c_storage) = lens_bool(d, |r| r.shutdown.unwrap_or_default().check_storage, |r, v| r.shutdown.get_or_insert_with(ShutdownPolicy::default).check_storage = v);
    let (c_tasks, set_c_tasks) = lens_bool(d, |r| r.shutdown.unwrap_or_default().check_tasks, |r, v| r.shutdown.get_or_insert_with(ShutdownPolicy::default).check_tasks = v);
    let (c_smb, set_c_smb) = lens_bool(d, |r| r.shutdown.unwrap_or_default().check_smb, |r, v| r.shutdown.get_or_insert_with(ShutdownPolicy::default).check_smb = v);
    let is_shutdown = move || d.with(|r| r.action == PowerAction::Shutdown);
    // the schedule as it would be saved, and what is wrong with it
    let candidate = move || -> Result<Vec<PowerRule>, String> {
        let mut r = d.get();
        let Some((h, m)) = parse_hhmm(&time.get()) else { return Err(t("ps.v_time").to_string()) };
        (r.hour, r.minute) = (h, m);
        if r.days.is_empty() {
            return Err(t("ps.v_days").to_string());
        }
        match r.action {
            PowerAction::Startup => r.shutdown = None,
            PowerAction::Shutdown => {
                let p = r.shutdown.get_or_insert_with(ShutdownPolicy::default);
                if !(1..=720).contains(&p.max_wait_min) {
                    return Err(tf("syn.v.range", &[("min", "1"), ("max", "720")]));
                }
            }
        }
        let orig_id = orig_id.get_value();
        let mut rs = rules.get();
        rs.retain(|x| Some(&x.id) != orig_id.as_ref());
        if orig_id.is_none() || r.id.is_empty() {
            let taken: Vec<&str> = rs.iter().map(|x| x.id.as_str()).collect();
            r.id = new_id(r.action, h, m, &taken);
        }
        match &orig_id {
            Some(id) => {
                let pos = rules.with(|all| all.iter().position(|x| &x.id == id)).unwrap_or(rs.len()).min(rs.len());
                rs.insert(pos, r);
            }
            None => rs.push(r),
        }
        if let Some(msg) = conflict(&rs) {
            return Err(msg);
        }
        Ok(rs)
    };
    let problem = Memo::new(move |_| candidate().err());
    let save = Callback::new(move |_| {
        let rs = match candidate() {
            Ok(rs) => rs,
            Err(e) => {
                error.set(Some(e));
                return;
            }
        };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = stage(rs).await;
            busy.set(false);
            match r {
                Ok(()) => {
                    staged();
                    close.run(());
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    });
    let tid = crate::form::fid();
    let days = (0..7usize)
        .map(|i| {
            let day = Weekday::ALL[i];
            view! {
                <label class="check day">
                    <input type="checkbox" prop:checked=move || d.with(|r| r.days.contains(&day))
                        on:change=move |ev| { let on = event_target_checked(&ev); d.update(|r| { r.days.retain(|x| *x != day); if on { r.days.push(day); r.days.sort(); } }); } />
                    {tr(DAY_KEYS[i])}
                </label>
            }
        })
        .collect_view();
    let body = view! {
        <form class="form" on:submit=move |ev| { ev.prevent_default(); save.run(()); }>
            {segmented("ps.col_action", ACTIONS, action, set_action)}
            {move || (!is_shutdown() && !wake_ok.get()).then(|| view! {
                {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {t("ps.wake_unsupported_help")} }.into_any())}
            })}
            <fieldset class="fieldset">
                <legend class="field-label">{tr("sch.f.weekdays")}</legend>
                <div class="quiet-days">{days}</div>
            </fieldset>
            <div class="field">
                <label class="field-label" for=tid.clone()>{tr("sch.f.time")}</label>
                <input id=tid class="input tnum" type="time" required prop:value=move || time.get() on:input=move |ev| time.set(event_target_value(&ev)) />
                <span class="field-help">{tr("ps.time_help")}</span>
            </div>
            {switch("ps.enabled", None, enabled, set_enabled)}
            <fieldset class="fieldset" hidden=move || !is_shutdown()>
                <legend class="field-label">{tr("ps.checks")}</legend>
                <span class="field-help">{tr("ps.checks_help")}</span>
                {switch("ps.check_storage", None, c_storage, set_c_storage)}
                {switch("ps.check_tasks", None, c_tasks, set_c_tasks)}
                {switch("ps.check_smb", None, c_smb, set_c_smb)}
                {segmented("ps.when_busy", BUSY, when_busy, set_when_busy)}
                <div hidden=move || when_busy.get() != "wait">{number_field("ps.max_wait", Some("ps.max_wait_help"), wait, set_wait, 1, 720)}</div>
            </fieldset>
            {move || problem.get().map(|p| view! {
                {ui::note(Tone::Err, icons::CIRCLE_X, view! { {p} }.into_any())}
            })}
        </form>
    }
    .into_any();
    let can = Signal::derive(move || problem.with(Option::is_none));
    let foot = save_bar(busy, can, if is_new { "create" } else { "save" }, close, save, error);
    let title = Signal::derive(move || t(if is_new { "add_rule" } else { "ps.edit" }).to_string());
    drawer(title, close, body, foot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str, action: PowerAction, days: &[Weekday], hour: u8, minute: u8) -> PowerRule {
        PowerRule { id: id.into(), enabled: true, action, days: days.to_vec(), hour, minute, shutdown: None }
    }

    #[test]
    fn conflicts_like_the_api() {
        use PowerAction::*;
        use Weekday::*;
        let up = rule("up", Startup, &[Mon], 7, 0);
        assert_eq!(conflict(&[up.clone(), rule("down", Shutdown, &[Mon], 23, 30)]), None);
        // same minute
        assert!(conflict(&[up.clone(), rule("down", Shutdown, &[Mon], 7, 0)]).unwrap().contains("07:00"));
        // 9 minutes after the startup
        assert!(conflict(&[up.clone(), rule("down", Shutdown, &[Mon], 7, 9)]).is_some());
        assert_eq!(conflict(&[up.clone(), rule("down", Shutdown, &[Mon], 7, 10)]), None);
        // a shutdown before the startup is fine
        assert_eq!(conflict(&[up, rule("down", Shutdown, &[Mon], 6, 55)]), None);
        // across midnight and the end of the week
        assert!(conflict(&[rule("up", Startup, &[Sun], 23, 55), rule("down", Shutdown, &[Mon], 0, 2)]).is_some());
        // disabled rules are not checked
        let mut off = rule("down", Shutdown, &[Mon], 7, 0);
        off.enabled = false;
        assert_eq!(conflict(&[rule("up", Startup, &[Mon], 7, 0), off]), None);
    }

    #[test]
    fn ids_and_times() {
        assert_eq!(new_id(PowerAction::Shutdown, 23, 30, &[]), "shutdown-2330");
        assert_eq!(new_id(PowerAction::Startup, 7, 5, &["startup-0705"]), "startup-0705-2");
        assert_eq!(parse_hhmm("07:05"), Some((7, 5)));
        assert_eq!(parse_hhmm("23:59:00"), Some((23, 59)));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm(""), None);
        assert_eq!(days_text(&Weekday::ALL), "Every day");
        assert_eq!(days_text(&[Weekday::Fri, Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu]), "Mon–Fri");
        assert_eq!(days_text(&[Weekday::Sat, Weekday::Sun]), "Sat, Sun");
    }
}
