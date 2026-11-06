//! Hardware › UPS (DSM Control Panel › Hardware & Power › UPS): live status from upsd
//! (`/api/v1/ups`, then `ups.status` events), the power failure policy
//! (`/api/v1/ups/settings`, staged like every configuration change), the battery test
//! (`POST /api/v1/ups/test`, task `ups.test`; only the tests the UPS lists) and the recent UPS
//! notifications.
//!
//! The texts follow the backend of `/api/v1/ups/settings`: plain NUT (shutdown through
//! upsmon), synodeb (Safe Mode) or none (install NUT). Low battery and a critical UPS (OB+LB)
//! always trigger it: the form says so next to the timer, which only adds a trigger. synodeb
//! is named only where it is the backend.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::notify::Notification;
use rynas_api_types::ups::*;

use crate::api;
use crate::form::{number_field, spinner, stale_notice, switch, Draft};
use crate::i18n::{self, t, tr};
use crate::icons::{self, Icon};
use crate::live::use_live;
use crate::toast::{saved_applied, toast};
use crate::ui::{self, Confirm, EmptyState, Meter, Remote, Skeleton, Status, Tone, WidgetHead, confirm, status, text};

const SETTINGS: &str = "/api/v1/ups/settings";
const EVENTS: &str = "/api/v1/notifications?category=ups&limit=10";

/// Overall state of a reading: tone and i18n key.
pub fn state_of(u: &UpsStatus) -> (Tone, &'static str) {
    match u.link {
        UpsLink::Unreachable => (Tone::Unknown, "ups.st_unreachable"),
        UpsLink::NotConfigured => (Tone::Unknown, "ups.st_none"),
        UpsLink::Connected if u.has("FSD") => (Tone::Err, "ups.st_fsd"),
        UpsLink::Connected if u.on_battery() && u.low_battery() => (Tone::Err, "ups.st_critical"),
        UpsLink::Connected if u.low_battery() => (Tone::Err, "ups.st_low"),
        UpsLink::Connected if u.on_battery() => (Tone::Warn, "ups.st_battery"),
        UpsLink::Connected if u.has("RB") => (Tone::Warn, "ups.st_replace"),
        UpsLink::Connected if u.has("OL") => (Tone::Ok, "ups.st_online"),
        UpsLink::Connected => (Tone::Unknown, "ups.st_unknown"),
    }
}

/// i18n key of a status flag (`None`: shown as is).
fn flag_key(f: &str) -> Option<&'static str> {
    Some(match f {
        "OL" => "ups.f_ol",
        "OB" => "ups.f_ob",
        "LB" => "ups.f_lb",
        "CHRG" => "ups.f_chrg",
        "DISCHRG" => "ups.f_dischrg",
        "RB" => "ups.f_rb",
        "FSD" => "ups.f_fsd",
        "BYPASS" => "ups.f_bypass",
        "CAL" => "ups.f_cal",
        "OVER" => "ups.f_over",
        _ => return None,
    })
}

fn flag_tone(f: &str) -> Tone {
    match f {
        "OL" | "CHRG" => Tone::Ok,
        "OB" | "DISCHRG" | "RB" | "BYPASS" | "CAL" => Tone::Warn,
        "LB" | "FSD" | "OVER" => Tone::Err,
        _ => Tone::Info,
    }
}

fn charge_tone(pct: f32) -> Tone {
    if pct < 30.0 {
        Tone::Err
    } else if pct < 60.0 {
        Tone::Warn
    } else {
        Tone::Ok
    }
}

fn load_tone(pct: f32) -> Tone {
    if pct >= 90.0 {
        Tone::Err
    } else if pct >= 70.0 {
        Tone::Warn
    } else {
        Tone::Ok
    }
}

fn volts(v: Option<f32>) -> String {
    v.map(|v| format!("{} V", i18n::num(v as f64, 0))).unwrap_or_else(|| "—".into())
}

fn dash(v: Option<String>) -> String {
    v.unwrap_or_else(|| "—".into())
}

fn kv(label: &'static str, value: AnyView) -> AnyView {
    view! { <div class="kv"><dt>{tr(label)}</dt><dd>{value}</dd></div> }.into_any()
}

// ------------------------------------------------------------------ panel

pub fn panel(_active: Signal<bool>) -> AnyView {
    let live = use_live();
    if live.ups.with_untracked(Option::is_none) {
        live.refresh_ups();
    }
    let settings = Remote::<UpsSettingsState>::with_config(SETTINGS);
    let events = Remote::<Vec<Notification>>::new(EVENTS);
    // a new UPS notification: read the list again
    Effect::new(move |prev: Option<u64>| {
        let g = crate::notify::server().map_or(0, |n| n.generation.get());
        if prev.is_some_and(|p| p != g) {
            events.reload();
        }
        g
    });
    #[cfg(feature = "demo")]
    if crate::auth::auth().phase.get_untracked() == crate::auth::Phase::Demo {
        settings.data.set(Some(crate::demo::ups_settings()));
        events.data.set(Some(crate::demo::ups_events()));
    }
    view! {
        {status_card(settings)}
        {super::ups_setup::card()}
        <div class="grid">
            {safemode_card(settings)}
            {test_card(settings)}
            {events_card(events)}
        </div>
    }
    .into_any()
}

/// The backend of the settings (`not_installed` while they load).
fn backend_of(settings: Remote<UpsSettingsState>) -> Signal<UpsBackend> {
    Signal::derive(move || settings.data.with(|s| s.as_ref().map_or(UpsBackend::NotInstalled, |s| s.backend)))
}

/// i18n key of the text under "no UPS" / "UPS service not running".
pub fn empty_text(link: UpsLink, backend: UpsBackend) -> &'static str {
    match (link, backend == UpsBackend::Synodeb) {
        (UpsLink::NotConfigured, true) => "ups.none_text_synodeb",
        (UpsLink::NotConfigured, false) => "ups.none_text",
        (_, true) => "ups.down_text_synodeb",
        (_, false) => "ups.down_text",
    }
}

/// One value of the live reading as a memo: it notifies only when that value changes, not at
/// every `ups.status` event (`T::default()` while there is no reading).
fn pick<T: PartialEq + Default + Send + Sync + 'static>(u: RwSignal<Option<UpsStatus>>, f: impl Fn(&UpsStatus) -> T + Send + Sync + 'static) -> Memo<T> {
    Memo::new(move |_| u.with(|s| s.as_ref().map(&f).unwrap_or_default()))
}

fn status_card(settings: Remote<UpsSettingsState>) -> AnyView {
    let live = use_live();
    let u = live.ups;
    let backend = backend_of(settings);
    // the card's frame changes only with the link; a new reading updates texts and bars
    let link = pick(u, |s| Some(s.link));
    let body = move || match link.get() {
        None => view! { <Skeleton lines=4 /> }.into_any(),
        Some(link) if link != UpsLink::Connected => {
            let title = if link == UpsLink::NotConfigured { "ups.none_title" } else { "ups.down_title" };
            let body = move || empty_text(link, backend.get());
            view! {
                <EmptyState icon=icons::BATTERY_CHARGING title=Signal::derive(move || t(title).to_string()) text=Signal::derive(move || t(body()).to_string())>
                    <button class="btn ghost" on:click=move |_| live.refresh_ups()><Icon icon=icons::REFRESH_CW size=16 />{tr("err.retry")}</button>
                </EmptyState>
            }
            .into_any()
        }
        Some(_) => connected_view(u),
    };
    view! {
        <section class="card widget">
            <WidgetHead icon=icons::BATTERY_CHARGING title="ups.status" />
            {body}
        </section>
    }
    .into_any()
}

/// The status of a connected UPS. Built once per connection: every value is its own memo and
/// text node, the flags a keyed list, so a reading every few seconds changes texts and bar
/// widths in place (no blink; hover and selection kept).
fn connected_view(u: RwSignal<Option<UpsStatus>>) -> AnyView {
    let state = pick(u, |s| Some(state_of(s)));
    let flags = pick(u, |s| s.flags.clone());
    let charge = pick(u, |s| s.battery_charge_pct);
    let runtime = pick(u, |s| s.battery_runtime_s);
    let load = pick(u, |s| s.load_pct);
    let input = pick(u, |s| s.input_voltage_v);
    let output = pick(u, |s| s.output_voltage_v);
    let manufacturer = pick(u, |s| s.manufacturer.clone());
    let model = pick(u, |s| s.model.clone());
    let serial = pick(u, |s| s.serial.clone());
    let beeper = pick(u, |s| s.beeper.clone());
    let battery_date = pick(u, |s| s.battery_date.clone());
    let driver = pick(u, |s| format!("{} · {}", dash(s.name.clone()), dash(s.driver.clone())));
    let read_at = pick(u, |s| s.read_at_ms);
    let has_charge = Memo::new(move |_| charge.get().is_some());
    let has_load = Memo::new(move |_| load.get().is_some());
    let has_beeper = Memo::new(move |_| beeper.with(Option::is_some));
    let has_date = Memo::new(move |_| battery_date.with(Option::is_some));
    let charge_text = move || charge.get().map(|c| i18n::pct(c as f64)).unwrap_or_else(|| "—".into());
    let runtime_text = move || runtime.get().map(i18n::duration).unwrap_or_else(|| "—".into());
    let aria = move || format!("{}: {} · {}: {}", t("ups.charge"), charge_text(), t("ups.runtime"), runtime_text());
    let beeper_text = move || {
        beeper
            .get()
            .map(|b| {
                let k = match b.as_str() {
                    "enabled" => "ups.beeper_on",
                    "disabled" => "ups.beeper_off",
                    "muted" => "ups.beeper_muted",
                    _ => "",
                };
                if k.is_empty() { b } else { t(k).to_string() }
            })
            .unwrap_or_default()
    };
    view! {
        <div class="ups-head">
            <Status tone=Signal::derive(move || state.get().map_or(Tone::Unknown, |s| s.0)) text=Signal::derive(move || state.get().map(|s| t(s.1).to_string()).unwrap_or_default()) />
            <span class="ups-flags">
                <For each=move || flags.get() key=|f| f.clone() let:f>
                    {status(flag_tone(&f), flag_key(&f).map(|k| t(k).to_string()).unwrap_or_else(|| f.clone()))}
                </For>
            </span>
        </div>
        <div class="hw-summary ups-kpis" role="img" aria-label=aria>
            <div class="hw-sum-item">
                <span class="muted small">{tr("ups.charge")}</span>
                <strong class="tnum">{charge_text}</strong>
                {move || has_charge.get().then(|| view! {
                    <Meter pct=Signal::derive(move || charge.get().unwrap_or(0.0) as f64) tone=Signal::derive(move || charge_tone(charge.get().unwrap_or(0.0))) />
                })}
            </div>
            <div class="hw-sum-item">
                <span class="muted small">{tr("ups.runtime")}</span>
                <strong class="tnum">{runtime_text}</strong>
            </div>
            <div class="hw-sum-item">
                <span class="muted small">{tr("ups.load")}</span>
                <strong class="tnum">{move || load.get().map(|l| i18n::pct(l as f64)).unwrap_or_else(|| "—".into())}</strong>
                {move || has_load.get().then(|| view! {
                    <Meter pct=Signal::derive(move || load.get().unwrap_or(0.0) as f64) tone=Signal::derive(move || load_tone(load.get().unwrap_or(0.0))) />
                })}
            </div>
            <div class="hw-sum-item">
                <span class="muted small">{tr("ups.input")}</span>
                <strong class="tnum">{move || volts(input.get())}</strong>
            </div>
            <div class="hw-sum-item">
                <span class="muted small">{tr("ups.output")}</span>
                <strong class="tnum">{move || volts(output.get())}</strong>
            </div>
        </div>
        <dl class="kvs">
            {kv("ups.manufacturer", view! { <span>{move || dash(manufacturer.get())}</span> }.into_any())}
            {kv("f.model", view! { <span>{move || dash(model.get())}</span> }.into_any())}
            {kv("sto.serial", view! { <span class="mono">{move || dash(serial.get())}</span> }.into_any())}
            {move || has_beeper.get().then(|| kv("ups.beeper", view! { <span>{beeper_text}</span> }.into_any()))}
            {move || has_date.get().then(|| kv("ups.battery_date", view! { <span>{move || battery_date.get().unwrap_or_default()}</span> }.into_any()))}
            {kv("ups.driver", view! { <span>{move || driver.get()}</span> }.into_any())}
            {kv("hw.updated", view! { <span>{move || i18n::short_when(read_at.get())}</span> }.into_any())}
        </dl>
    }
    .into_any()
}

/// Minutes in the form, seconds in the API.
#[derive(Clone, PartialEq)]
struct Form {
    timer: bool,
    minutes: String,
    pre_timeout: String,
}

impl Form {
    fn of(s: &UpsSettings) -> Self {
        Form {
            timer: s.safemode_after_s.is_some(),
            minutes: (s.safemode_after_s.unwrap_or(300) / 60).to_string(),
            pre_timeout: s.pre_safemode_timeout_s.to_string(),
        }
    }

    /// The settings, or the i18n key of the first invalid field.
    fn settings(&self) -> Result<UpsSettings, &'static str> {
        let (lo, hi) = SAFEMODE_AFTER_RANGE_S;
        let after = if self.timer {
            match self.minutes.trim().parse::<u32>() {
                Ok(m) if m * 60 >= lo && m * 60 <= hi => Some(m * 60),
                _ => return Err("ups.v_minutes"),
            }
        } else {
            None
        };
        let (lo, hi) = PRE_SAFEMODE_TIMEOUT_RANGE_S;
        match self.pre_timeout.trim().parse::<u32>() {
            Ok(p) if (lo..=hi).contains(&p) => Ok(UpsSettings { safemode_after_s: after, pre_safemode_timeout_s: p }),
            _ => Err("ups.v_pre"),
        }
    }
}

fn safemode_card(settings: Remote<UpsSettingsState>) -> AnyView {
    let body = move || {
        if let Some(v) = settings.gate(4) {
            return v;
        }
        let st = settings.data.get_untracked().unwrap_or_default();
        if !st.available {
            return view! { {ui::note(Tone::Info, icons::INFO, view! { {t("ups.need_nut")} }.into_any())} }.into_any();
        }
        safemode_form(settings, &st)
    };
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::SHIELD_ALERT title="ups.safemode" />
            {body}
        </section>
    }
    .into_any()
}

/// i18n keys of the policy form: the low battery statement, the timer switch and its help,
/// the help of the stop time. synodeb enters Safe Mode, NUT's upsmon shuts down.
pub fn policy_keys(b: UpsBackend) -> [&'static str; 4] {
    if b == UpsBackend::Synodeb {
        ["ups.lb_always", "ups.timer", "ups.timer_help", "ups.pre_timeout_help"]
    } else {
        ["ups.lb_always_nut", "ups.timer_nut", "ups.timer_help_nut", "ups.pre_timeout_help_nut"]
    }
}

fn safemode_form(settings: Remote<UpsSettingsState>, st: &UpsSettingsState) -> AnyView {
    let [k_lb, k_timer, k_timer_help, k_pre_help] = policy_keys(st.backend);
    let init = Form::of(st.pending.as_ref().unwrap_or(&st.applied));
    let draft = Draft::new(init, move || settings.data.with(|s| s.as_ref().map(|s| Form::of(s.pending.as_ref().unwrap_or(&s.applied)))));
    let d = draft.d;
    let dirty = draft.dirty();
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let pending = Signal::derive(move || settings.data.with(|s| s.as_ref().is_some_and(|s| s.pending.is_some())));
    let save = move || {
        let body = match d.with_untracked(Form::settings) {
            Ok(b) => b,
            Err(k) => {
                error.set(Some(t(k).to_string()));
                return;
            }
        };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = api::put::<_, UpsSettingsState>(SETTINGS, &body).await;
            match r {
                Ok(_) => {
                    if crate::config::apply_now(&[crate::config::keys::UPS], error).await {
                        saved_applied();
                    }
                }
                Err(e) => error.set(Some(e.text())),
            }
            let _ = busy.try_set(false);
        });
    };
    let reset = Callback::new(move |_| {
        draft.reset();
        error.set(None);
    });
    let timer = Signal::derive(move || d.with(|f| f.timer));
    view! {
        <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr(k_lb)}</p>
        {switch(k_timer, Some(k_timer_help), timer, Callback::new(move |v| d.update(|f| f.timer = v)))}
        <Show when=move || timer.get()>
            {number_field("ups.minutes", Some("ups.minutes_help"), Signal::derive(move || d.with(|f| f.minutes.clone())), Callback::new(move |v| d.update(|f| f.minutes = v)), 1, 1440)}
        </Show>
        {number_field("ups.pre_timeout", Some(k_pre_help), Signal::derive(move || d.with(|f| f.pre_timeout.clone())), Callback::new(move |v| d.update(|f| f.pre_timeout = v)), 10, 900)}
        {move || pending.get().then(|| view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{t("ups.pending_note")}</p> })}
        {stale_notice(draft.stale, reset)}
        {crate::form::form_error(error)}
        <div class="btn-row">
            <button type="button" class="btn ghost" disabled=move || !dirty.get() on:click=move |_| reset.run(())>{tr("reset")}</button>
            <button type="button" class="btn primary" disabled=move || busy.get() || !dirty.get() on:click=move |_| save()>
                {spinner(busy)}{tr("save")}
            </button>
        </div>
    }
    .into_any()
}

/// The command shown for a scheduled test: the quick test, else the deep one, when offered.
pub fn schedule_command(u: &UpsStatus) -> Option<&'static str> {
    if u.supports(BatteryTest::Quick) {
        Some("rynasctl ups test quick")
    } else if u.supports(BatteryTest::Deep) {
        Some("rynasctl ups test deep")
    } else {
        None
    }
}

fn test_card(settings: Remote<UpsSettingsState>) -> AnyView {
    let live = use_live();
    let ask_deep = RwSignal::new(false);
    let busy = RwSignal::new(None::<BatteryTest>);
    let backend = backend_of(settings);
    let loaded = Signal::derive(move || settings.data.with(Option::is_some));
    let available = Signal::derive(move || settings.data.with(|s| s.as_ref().is_some_and(|s| s.available)));
    // memos: notes and buttons are rebuilt only when what they show changes, not at every reading
    let connected = Memo::new(move |_| live.ups.with(|u| u.as_ref().is_some_and(|u| u.link == UpsLink::Connected)));
    let online = Memo::new(move |_| live.ups.with(|u| u.as_ref().is_some_and(|u| u.link == UpsLink::Connected && u.has("OL") && !u.on_battery())));
    let cmds = Memo::new(move |_| live.ups.with(|u| u.as_ref().map(|u| u.commands.clone()).unwrap_or_default()));
    let supports = move |t: BatteryTest| cmds.with(|c| c.iter().any(|c| c == t.nut_cmd()));
    let any = Memo::new(move |_| available.get() && BatteryTest::ALL.into_iter().any(supports));
    let synodeb = move || backend.get() == UpsBackend::Synodeb;
    let run = move |test: BatteryTest| {
        busy.set(Some(test));
        spawn_local(async move {
            let body = serde_json::to_string(&BatteryTestRequest { test }).ok();
            let r = crate::tasks::run_task("/api/v1/ups/test", body).await;
            busy.set(None);
            match r {
                Ok(info) if info.status == rynas_api_types::TaskStatus::Ok => toast(Tone::Ok, t("ups.test_done").to_string()),
                Ok(info) => toast(Tone::Err, crate::tasks::task_error(&info)),
                Err(e) => toast(Tone::Err, e.text()),
            }
            live.refresh_ups();
        });
    };
    let deep_dialog = move || {
        ask_deep.get().then(|| {
            let close = Callback::new(move |_| ask_deep.set(false));
            confirm(Confirm::new(
                t("ups.deep_title").to_string(),
                t(if synodeb() { "ups.deep_text" } else { "ups.deep_text_nut" }).to_string(),
                "ups.deep_start",
                true,
                Callback::new(move |_| {
                    ask_deep.set(false);
                    run(BatteryTest::Deep);
                }),
                close,
            ))
        })
    };
    // the pill is rebuilt only when the result changes, not at every reading
    let last = Memo::new(move |_| live.ups.with(|u| u.as_ref().and_then(|u| u.test_result.clone())));
    let result = move || {
        match last.get() {
            Some(r) => match test_result(&r) {
                Some((tone, key)) => status(tone, t(key).to_string()),
                // a value this build does not know: the UPS's own (English) text
                None => {
                    let l = r.to_ascii_lowercase();
                    let tone = if l.contains("pass") {
                        Tone::Ok
                    } else if l.contains("error") || l.contains("fail") || l.contains("abort") {
                        Tone::Err
                    } else {
                        Tone::Unknown
                    };
                    view! { <span lang="en">{status(tone, r)}</span> }.into_any()
                }
            },
            None => text(t("ups.no_result").to_string()),
        }
    };
    let can = move || available.get() && online.get() && busy.get().is_none();
    let spin = move |k: BatteryTest| (busy.get() == Some(k)).then(|| view! { <span class="btn-spinner" aria-hidden="true"></span> });
    let schedule = Memo::new(move |_| live.ups.with(|u| u.as_ref().and_then(schedule_command)).filter(|_| available.get()));
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::ACTIVITY title="ups.test" />
            <dl class="kvs">{kv("ups.last_result", view! { <span>{result}</span> }.into_any())}</dl>
            {move || (loaded.get() && !available.get()).then(|| view! { {ui::note(Tone::Info, icons::INFO, view! { {t("ups.need_nut")} }.into_any())} })}
            {move || (available.get() && connected.get() && !any.get()).then(|| view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{t("ups.test_unsupported")}</p> })}
            {move || (any.get() && !online.get()).then(|| view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{t("ups.test_online")}</p> })}
            {move || any.get().then(|| view! {
                <p class="sec-note"><Icon icon=icons::TRIANGLE_ALERT size=16 />{t(if synodeb() { "ups.test_lb" } else { "ups.test_lb_nut" })}</p>
                <div class="btn-row">
                    {supports(BatteryTest::Quick).then(|| view! {
                        <button type="button" class="btn primary" disabled=move || !can() on:click=move |_| run(BatteryTest::Quick)>
                            {move || spin(BatteryTest::Quick)}{tr("ups.quick")}
                        </button>
                    })}
                    {supports(BatteryTest::Deep).then(|| view! {
                        <button type="button" class="btn ghost" disabled=move || !can() on:click=move |_| ask_deep.set(true)>
                            {move || spin(BatteryTest::Deep)}{tr("ups.deep")}
                        </button>
                    })}
                    {supports(BatteryTest::Stop).then(|| view! {
                        <button type="button" class="btn ghost" disabled=move || !available.get() on:click=move |_| run(BatteryTest::Stop)>{tr("ups.stop")}</button>
                    })}
                </div>
            })}
            {move || schedule.get().map(|c| view! { <p class="muted small">{tr("ups.schedule_hint")}" "<code>{c}</code></p> })}
            {deep_dialog}
        </section>
    }
    .into_any()
}

fn events_card(events: Remote<Vec<Notification>>) -> AnyView {
    let body = move || {
        if let Some(v) = events.gate(3) {
            return v;
        }
        let list = events.data.get().unwrap_or_default();
        if list.is_empty() {
            return view! { <p class="muted">{t("ups.no_events")}</p> }.into_any();
        }
        view! {
            <ul class="events">
                {list.into_iter().map(|n| {
                    let tone = match n.severity {
                        rynas_api_types::notify::NotifySeverity::Critical | rynas_api_types::notify::NotifySeverity::Error => Tone::Err,
                        rynas_api_types::notify::NotifySeverity::Warning => Tone::Warn,
                        _ => Tone::Info,
                    };
                    view! {
                        <li>
                            <span class="mono small muted">{i18n::short_when(n.last_ms)}</span>
                            <Status tone=tone text=t(crate::notify::severity_key(n.severity)).to_string() />
                            <span>{n.title}</span>
                        </li>
                    }
                }).collect_view()}
            </ul>
        }
        .into_any()
    };
    view! {
        <section class="card widget w-12">
            <WidgetHead icon=icons::HISTORY title="ups.events" />
            {body}
        </section>
    }
    .into_any()
}

/// Tone and text key of the standard values of NUT's `ups.test.result` (drivers write them
/// with or without quotes and in any case: `Done and "passed"`); `None` for another value.
fn test_result(raw: &str) -> Option<(Tone, &'static str)> {
    let v: String = raw.chars().filter(|c| *c != '"' && *c != '\'').collect::<String>().trim().to_ascii_lowercase();
    let v = v.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(match v.as_str() {
        "done and passed" | "passed" => (Tone::Ok, "ups.tr.passed"),
        "done and warning" | "warning" => (Tone::Warn, "ups.tr.warning"),
        "done and error" | "error" | "failed" => (Tone::Err, "ups.tr.error"),
        "aborted" => (Tone::Err, "ups.tr.aborted"),
        "in progress" => (Tone::Info, "ups.tr.in_progress"),
        "no test initiated" | "no test" => (Tone::Unknown, "ups.tr.none"),
        "test scheduled" | "scheduled" => (Tone::Info, "ups.tr.scheduled"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nut_test_results_are_translated() {
        assert_eq!(test_result("Done and passed"), Some((Tone::Ok, "ups.tr.passed")));
        assert_eq!(test_result("Done and \"passed\""), Some((Tone::Ok, "ups.tr.passed")));
        assert_eq!(test_result(" done  and warning "), Some((Tone::Warn, "ups.tr.warning")));
        assert_eq!(test_result("Done and error"), Some((Tone::Err, "ups.tr.error")));
        assert_eq!(test_result("Aborted"), Some((Tone::Err, "ups.tr.aborted")));
        assert_eq!(test_result("In progress"), Some((Tone::Info, "ups.tr.in_progress")));
        assert_eq!(test_result("No test initiated"), Some((Tone::Unknown, "ups.tr.none")));
        assert_eq!(test_result("Battery replaced 2025"), None);
        for (_, k) in ["Done and passed", "Done and warning", "Done and error", "Aborted", "In progress", "No test initiated", "Test scheduled"]
            .iter()
            .filter_map(|r| test_result(r))
        {
            assert!(i18n::try_t(k).is_some(), "{k}");
        }
    }

    #[test]
    fn states() {
        let mut u = UpsStatus { link: UpsLink::Connected, flags: vec!["OL".into(), "CHRG".into()], ..Default::default() };
        assert_eq!(state_of(&u).1, "ups.st_online");
        u.flags = vec!["OB".into(), "DISCHRG".into()];
        assert_eq!(state_of(&u).1, "ups.st_battery");
        u.flags.push("LB".into());
        assert_eq!(state_of(&u), (Tone::Err, "ups.st_critical"));
        assert_eq!(state_of(&UpsStatus::default()).1, "ups.st_unreachable");
    }

    #[test]
    fn form() {
        let f = Form { timer: true, minutes: "5".into(), pre_timeout: "120".into() };
        assert_eq!(f.settings(), Ok(UpsSettings { safemode_after_s: Some(300), pre_safemode_timeout_s: 120 }));
        let off = Form { timer: false, minutes: "x".into(), ..f.clone() };
        assert_eq!(off.settings().unwrap().safemode_after_s, None);
        assert_eq!(Form { minutes: "0".into(), ..f.clone() }.settings(), Err("ups.v_minutes"));
        assert_eq!(Form { pre_timeout: "5".into(), ..f }.settings(), Err("ups.v_pre"));
    }

    /// Nothing a plain Debian user sees names synodeb.
    #[test]
    fn neutral_without_synodeb() {
        let keys = |b: UpsBackend| {
            let mut k = policy_keys(b).to_vec();
            k.push(empty_text(UpsLink::NotConfigured, b));
            k.push(empty_text(UpsLink::Unreachable, b));
            k
        };
        let generic: Vec<&str> = [UpsBackend::Nut, UpsBackend::NotInstalled].into_iter().flat_map(keys).chain(["ups.need_nut", "ups.test_unsupported", "ups.test_lb_nut", "ups.deep_text_nut", "ups.safemode", "ups.schedule_hint"]).collect();
        for lang in [i18n::Lang::En, i18n::Lang::It] {
            for k in &generic {
                let s = i18n::text_in(lang, k).unwrap_or_else(|| panic!("missing {k}"));
                assert!(!s.to_ascii_lowercase().contains("synodeb") && !s.contains("Synology") && !s.contains("DSM"), "{k}: {s}");
            }
        }
        assert_eq!(empty_text(UpsLink::Unreachable, UpsBackend::Synodeb), "ups.down_text_synodeb");
        assert_eq!(policy_keys(UpsBackend::Synodeb)[0], "ups.lb_always");
    }

    #[test]
    fn schedule_hint() {
        let mut u = UpsStatus { link: UpsLink::Connected, ..Default::default() };
        assert_eq!(schedule_command(&u), None);
        u.commands = vec!["test.battery.start.deep".into()];
        assert_eq!(schedule_command(&u), Some("rynasctl ups test deep"));
        u.commands.push("test.battery.start.quick".into());
        assert_eq!(schedule_command(&u), Some("rynasctl ups test quick"));
    }
}
