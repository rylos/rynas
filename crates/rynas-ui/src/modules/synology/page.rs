//! Hardware › Synology (the Synology module's tab, D-MODULE): every synohwd feature
//! (D-SYNOPLUG, `docs/plugins/synology.md`).
//!
//! - live status (`/modules/synology/status`, re-read every 10 s while the tab is shown): the
//!   chassis with bays, LEDs, temperatures and standby, fans, events, boot cause;
//! - DSM's fan curves of the model (`/modules/synology/fan-curves`) drawn with the readings;
//! - the `synology.hw` settings (`/modules/synology/settings`): one draft for every card,
//!   staged with one Save (pending bar, apply in the configuration page);
//! - actions: test beep, mute, identify a bay.
//!
//! The tab exists only where the module is present (`super::present`); should the API still answer 404
//! the panel shows the neutral "no hardware provider" state.

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::network::{LinkKind, NetInterface};
use rynas_api_types::synology::*;

use crate::api;
use crate::form::{Draft, In, stale_notice, lens, lens_bool, segmented, select_field, switch, text_field};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::pages::dashboard::led_state;
use crate::pages::security::copy_button;
use crate::toast::{saved_pending, toast};
use crate::ui::{self, EmptyState, Meter, Remote, Status, Tone, WidgetHead, status};

const SETTINGS: &str = "/api/v1/modules/synology/settings";
const POLL: Duration = Duration::from_secs(10);

const FAN_MODES: &[(&str, &str)] = &[("low", "syn.fan_low"), ("high", "syn.fan_high"), ("full", "syn.fan_full")];
const POWER_BUTTON: &[(&str, &str)] = &[("poweroff", "syn.pb_poweroff"), ("log", "syn.btn_log"), ("ignore", "syn.btn_ignore")];
const RESET_BUTTON: &[(&str, &str)] = &[("network_rescue", "syn.rb_rescue"), ("log", "syn.btn_log"), ("ignore", "syn.btn_ignore")];

fn fan_mode_str(m: FanMode) -> &'static str {
    m.as_str()
}
fn fan_mode_of(s: &str) -> FanMode {
    FanMode::ALL.into_iter().find(|m| m.as_str() == s).unwrap_or_default()
}
fn pb_str(a: PowerButtonAction) -> &'static str {
    match a {
        PowerButtonAction::Poweroff => "poweroff",
        PowerButtonAction::Log => "log",
        PowerButtonAction::Ignore => "ignore",
    }
}
fn pb_of(s: &str) -> PowerButtonAction {
    match s {
        "log" => PowerButtonAction::Log,
        "ignore" => PowerButtonAction::Ignore,
        _ => PowerButtonAction::Poweroff,
    }
}
fn rb_str(a: ResetButtonAction) -> &'static str {
    match a {
        ResetButtonAction::NetworkRescue => "network_rescue",
        ResetButtonAction::Log => "log",
        ResetButtonAction::Ignore => "ignore",
    }
}
fn rb_of(s: &str) -> ResetButtonAction {
    match s {
        "network_rescue" => ResetButtonAction::NetworkRescue,
        "ignore" => ResetButtonAction::Ignore,
        _ => ResetButtonAction::Log,
    }
}

/// `led-dot` class and `led.*` key of the front status LED.
fn led_name(l: StatusLed) -> &'static str {
    match l {
        StatusLed::Off => "off",
        StatusLed::Green => "green",
        StatusLed::GreenBlink => "green-blink",
        StatusLed::Orange => "orange",
        StatusLed::OrangeBlink => "orange-blink",
    }
}

fn class_key(c: SensorClass) -> &'static str {
    match c {
        SensorClass::Disk => "syn.cls_disk",
        SensorClass::Nvme => "syn.cls_nvme",
        SensorClass::Cpu => "syn.cls_cpu",
        SensorClass::Nic => "syn.cls_nic",
    }
}

fn class_color(c: SensorClass) -> &'static str {
    match c {
        SensorClass::Disk => "--c1",
        SensorClass::Nvme => "--c2",
        SensorClass::Cpu => "--c5",
        SensorClass::Nic => "--c4",
    }
}

/// The settings synohwd accepts (`RANGES` in synohwd.py); the API refuses anything else.
pub fn range_error(s: &SynologySettings) -> Option<&'static str> {
    if !(5..=300).contains(&s.fan_period_s) {
        Some("fan_period_s")
    } else if s.fan_min_duty_pct > 99 {
        Some("fan_min_duty_pct")
    } else if s.hysteresis_c > 20 {
        Some("hysteresis_c")
    } else if s.led_brightness == 0 {
        Some("led_brightness")
    } else if s.disk_standby_minutes > 330 {
        Some("disk_standby_minutes")
    } else if s.led_reduced_level > 7 {
        Some("led_reduced_level")
    } else if !led_schedule_valid(&s.led_schedule) {
        Some("led_schedule")
    } else {
        None
    }
}

/// Duty of a curve at `t` °C: the last step at or below it.
pub fn duty_at(c: &FanCurve, t: f32) -> u8 {
    c.points.iter().rev().find(|p| f32::from(p.threshold_c) <= t).map(|p| p.duty_pct).unwrap_or_else(|| c.points.first().map_or(0, |p| p.duty_pct))
}

/// The curves of `mode` that have a sensor with a reading (all of them when none has one).
fn shown_curves<'a>(mode: &'a FanModeCurves, readings: &[SensorReading]) -> Vec<&'a FanCurve> {
    let with: Vec<&FanCurve> =
        mode.curves.iter().filter(|c| readings.iter().any(|r| r.class == c.class && r.temperature_c.is_some())).collect();
    if with.is_empty() { mode.curves.iter().collect() } else { with }
}

/// Curves of the mode the draft shows (`full` has no table: the shutdown thresholds of `low`).
fn mode_curves(fc: &FanCurves, mode: FanMode) -> Option<&FanModeCurves> {
    let want = if mode == FanMode::Full { FanMode::Low } else { mode };
    fc.modes.iter().find(|m| m.mode == want)
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Step chart of DSM's curves (SVG markup, colors from the theme): x = °C, y = duty %, a dot per
/// sensor at its reading, the current duty as a dashed line.
pub fn curve_svg(curves: &[&FanCurve], readings: &[SensorReading], duty: Option<u8>) -> String {
    const W: f32 = 360.0;
    const H: f32 = 180.0;
    const L: f32 = 34.0;
    const R: f32 = 16.0;
    const T: f32 = 10.0;
    const B: f32 = 22.0;
    let first = curves.iter().filter_map(|c| c.points.iter().map(|p| p.threshold_c).filter(|&t| t > 0).min()).min().unwrap_or(40);
    let lo = (f32::from(first.saturating_sub(10)) / 10.0).floor() * 10.0;
    let hi = ((f32::from(curves.iter().map(|c| c.shutdown_c).max().unwrap_or(70)) + 4.0) / 10.0).ceil() * 10.0;
    let hi = if hi <= lo { lo + 10.0 } else { hi };
    let x = |t: f32| L + (t.clamp(lo, hi) - lo) / (hi - lo) * (W - L - R);
    let y = |d: f32| T + (1.0 - d.clamp(0.0, 100.0) / 100.0) * (H - T - B);
    let mut s = format!(r#"<svg viewBox="0 0 {W} {H}" class="cv" aria-hidden="true">"#);
    for d in [0.0, 50.0, 100.0] {
        s += &format!(r#"<line class="cv-grid" x1="{L}" x2="{}" y1="{y0}" y2="{y0}"/><text x="{}" y="{}" text-anchor="end">{d}%</text>"#, W - R, L - 4.0, y(d) + 3.5, y0 = y(d));
    }
    let mut t = lo;
    while t <= hi {
        s += &format!(r#"<text x="{}" y="{}" text-anchor="middle">{t}°</text>"#, x(t), H - 6.0);
        t += 10.0;
    }
    if let Some(d) = duty {
        s += &format!(r#"<line class="cv-duty" x1="{L}" x2="{}" y1="{y0}" y2="{y0}"/>"#, W - R, y0 = y(f32::from(d)));
    }
    for c in curves {
        let color = class_color(c.class);
        let Some(p0) = c.points.first() else { continue };
        let mut d = format!("M{} {}", x(lo), y(f32::from(p0.duty_pct)));
        let mut prev = p0.duty_pct;
        for p in c.points.iter().skip(1) {
            d += &format!(" H{} V{}", x(f32::from(p.threshold_c)), y(f32::from(p.duty_pct)));
            prev = p.duty_pct;
        }
        let (sx, sy) = (x(f32::from(c.shutdown_c)), y(f32::from(prev)));
        d += &format!(" H{sx}");
        s += &format!(r#"<path d="{d}" style="stroke:var({color})"/><path class="cv-stop" d="M{} {} l6 6 m0 -6 l-6 6" style="stroke:var({color})"/>"#, sx - 3.0, sy - 3.0);
        for r in readings.iter().filter(|r| r.class == c.class) {
            let Some(temp) = r.temperature_c else { continue };
            let dy = duty_at(c, temp);
            s += &format!(
                r#"<circle cx="{}" cy="{}" r="4.5" style="fill:var({color})"><title>{}: {} → {dy}%</title></circle>"#,
                x(temp),
                y(f32::from(dy)),
                esc(&r.sensor),
                i18n::celsius(temp)
            );
        }
    }
    s + "</svg>"
}

// ------------------------------------------------------------------ panel

pub fn panel(active: Signal<bool>) -> AnyView {
    let settings = Remote::<SynologySettingsState>::with_config(SETTINGS);
    let st = Remote::<SynologyStatus>::new("/api/v1/modules/synology/status");
    let curves = Remote::<FanCurves>::new("/api/v1/modules/synology/fan-curves");
    // the active mode and the boot change with an apply
    Effect::new(move |prev: Option<()>| {
        crate::config::generation();
        if prev.is_some() {
            st.reload();
            curves.reload();
        }
    });
    if let Ok(h) = set_interval_with_handle(
        move || {
            if active.get_untracked() && !document().hidden() && settings.err.with_untracked(Option::is_none) {
                st.reload();
                curves.reload();
            }
        },
        POLL,
    ) {
        on_cleanup(move || h.clear());
    }
    (move || {
        if settings.err.with(|e| e.as_deref() == Some("not_found")) {
            return view! {
                <div class="card soon">
                    <EmptyState icon=icons::CPU title=Signal::derive(|| t("hw.none_title").to_string()) text=Signal::derive(|| t("hw.none_text").to_string()) />
                </div>
            }
            .into_any();
        }
        if let Some(v) = settings.gate(8) {
            return view! { <div class="card">{v}</div> }.into_any();
        }
        // built once: the draft follows the server's settings (form::Draft); the notices of
        // the body read the current state
        let s = settings.data.get_untracked().unwrap_or_default();
        body(s, settings, st, curves, active)
    })
    .into_any()
}

fn body(s: SynologySettingsState, settings: Remote<SynologySettingsState>, st: Remote<SynologyStatus>, curves: Remote<FanCurves>, active: Signal<bool>) -> AnyView {
    let init = s.pending.clone().unwrap_or_else(|| s.applied.clone());
    let draft = Draft::new(init, move || settings.data.with(|s| s.as_ref().map(|s| s.pending.clone().unwrap_or_else(|| s.applied.clone()))));
    let d = draft.d;
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let dirty = draft.dirty();
    let pending = Signal::derive(move || settings.data.with(|s| s.as_ref().is_some_and(|s| s.pending.is_some())));
    let running = s.platform.synohwd_running;
    // synodeb ≥ 4.8 with the front LED chip: the schedule card (hidden otherwise). The last
    // answer stays while synohwd is not running (restarted by a save: its status is empty for a
    // moment), so the card does not vanish with its dialog; only a running synohwd hides it.
    let led_supported = Memo::new(move |prev: Option<&bool>| {
        st.data.with(|v| match v {
            Some(v) if v.running || v.led_schedule_supported => v.led_schedule_supported,
            _ => prev.copied().unwrap_or(false),
        })
    });
    let save = move || {
        if d.with_untracked(range_error).is_some() {
            error.set(Some(t("form.check").to_string()));
            return;
        }
        busy.set(true);
        error.set(None);
        let body = d.get_untracked();
        spawn_local(async move {
            let r = api::put::<_, SynologySettingsState>(SETTINGS, &body).await;
            busy.set(false);
            match r {
                Ok(_) => {
                    saved_pending();
                    crate::config::changed();
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    };
    let reset = Callback::new(move |_| {
        draft.reset();
        error.set(None);
    });
    view! {
        {(!running).then(|| view! {
            {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {t("syn.not_running")} }.into_any())}
        })}
        {(!s.foreign_commands.is_empty()).then(|| view! {
            {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tf("syn.foreign", &[("cmds", &s.foreign_commands.join(", "))])} }.into_any())}
        })}
        {summary(st, s.platform.clone(), pending, active)}
        <section class="card widget w-12">
            <h2 class="section-title"><Icon icon=icons::BOX size=16 />{tr("sto.chassis")}</h2>
            {st.view(5, move || chassis(st, d, running))}
        </section>
        <div class="grid">
            {fans_card(d, st, curves)}
            {thermal_card(d, curves)}
            {leds_card(d, running)}
            {buttons_card(d)}
            {move || led_supported.get().then(|| super::led::card(d, st))}
            {power_card(d)}
            {events_card(st)}
        </div>
        <div class="stage-bar" hidden=move || !dirty.get() && error.with(Option::is_none) role="region" aria-label=tr("syn.unsaved")>
            <Icon icon=icons::INFO size=16 />
            <span class="stage-text">{tr("syn.unsaved")}</span>
            {stale_notice(draft.stale, reset)}
            {crate::form::form_error(error)}
            <button type="button" class="btn ghost" disabled=move || !dirty.get() on:click=move |_| reset.run(())>{tr("reset")}</button>
            <button type="button" class="btn primary" disabled=move || busy.get() || !dirty.get() on:click=move |_| save()>
                {crate::form::spinner(busy)}{tr("save")}
            </button>
        </div>
    }
    .into_any()
}

fn sum_item(label: &'static str, value: AnyView) -> AnyView {
    view! { <div class="hw-sum-item"><span class="muted small">{tr(label)}</span>{value}</div> }.into_any()
}

fn summary(st: Remote<SynologyStatus>, platform: SynologyPlatform, pending: Signal<bool>, active: Signal<bool>) -> AnyView {
    // every value is a memo: a poll only rewrites what changed, and "updated … ago" counts the
    // seconds locally from when synohwd wrote the status
    let content = move || {
        let daemon = Memo::new(move |_| {
            st.data.with(|v| match v {
                Some(v) if v.running && !v.stale => (Tone::Ok, "syn.running"),
                Some(v) if v.running => (Tone::Warn, "hw.stale"),
                _ => (Tone::Err, "syn.stopped"),
            })
        });
        let written = written_at(st);
        let now = RwSignal::new(js_sys::Date::now());
        if let Ok(h) = set_interval_with_handle(
            move || {
                if active.get_untracked() && !document().hidden() {
                    now.set(js_sys::Date::now());
                }
            },
            Duration::from_secs(1),
        ) {
            on_cleanup(move || h.clear());
        }
        let age = move || match written.get() {
            Some(at) => format!("{} {}", i18n::duration(((now.get() - at).max(0.0) / 1000.0) as u64), t("hw.ago")),
            None => "—".into(),
        };
        let led = Memo::new(move |_| st.data.with(|v| v.as_ref().and_then(|v| v.status_led).map(led_name)));
        let duty = Memo::new(move |_| st.data.with(|v| v.as_ref().and_then(|v| v.fan_duty_pct)));
        let boot = Memo::new(move |_| {
            st.data.with(|v| {
                v.as_ref().and_then(|v| v.boot.as_ref()).map(|b| match b.cause {
                    rynas_api_types::power::BootCause::Watchdog => (Tone::Warn, "ps.cause_watchdog"),
                    rynas_api_types::power::BootCause::ScheduledStartup => (Tone::Ok, "ps.cause_scheduled"),
                    rynas_api_types::power::BootCause::Unknown => (Tone::Unknown, "ps.cause_unknown"),
                })
            })
        });
        view! {
            {sum_item("syn.daemon", (move || { let (tone, key) = daemon.get(); status(tone, t(key).to_string()) }).into_any())}
            {sum_item("hw.updated", view! { <strong class="tnum">{age}</strong> }.into_any())}
            {sum_item("hw.led", (move || match led.get() {
                Some(n) => {
                    let (tone, state, colour) = led_state(n);
                    view! { <span class="led-big" title=colour><span class=format!("led-dot {n}") aria-hidden="true"></span><Status tone=tone text=state /></span> }.into_any()
                }
                None => view! { <strong>"—"</strong> }.into_any(),
            }).into_any())}
            {sum_item("hw.duty", view! { <strong class="tnum">{move || pct_text(duty.get())}</strong> }.into_any())}
            {move || boot.get().map(|(tone, key)| sum_item("last_boot", status(tone, t(key).to_string())))}
        }
        .into_any()
    };
    view! {
        <div class="hw-summary card">
            <div class="hw-sum-item">
                <span class="muted small">{tr("f.model")}</span>
                <strong>{platform.model.clone()}</strong>
                <span class="muted small">{tf("syn.platform", &[("bays", &platform.bays.to_string()), ("fans", &platform.fans.to_string())])}</span>
            </div>
            {move || pending.get().then(|| sum_item("syn.settings", status(Tone::Info, t("pending.change").to_string())))}
            {st.view(2, content)}
        </div>
    }
    .into_any()
}

/// When synohwd wrote the status, on this browser's clock: `age_s` taken back from the moment
/// the answer arrived, kept while `read_at_ms` stays the same (no jitter from poll to poll).
fn written_at(st: Remote<SynologyStatus>) -> Memo<Option<f64>> {
    let anchor = Memo::new(move |prev: Option<&(Option<u64>, Option<f64>)>| {
        let (read_at, age) = st.data.with(|v| v.as_ref().map_or((None, None), |v| (v.read_at_ms, v.age_s)));
        if let Some(p) = prev.filter(|p| p.0.is_some() && p.0 == read_at && p.1.is_some()) {
            return *p;
        }
        (read_at, age.map(|a| js_sys::Date::now() - a as f64 * 1000.0))
    });
    Memo::new(move |_| anchor.get().1)
}

fn pct_text(p: Option<u8>) -> String {
    p.map(|d| format!("{d}%")).unwrap_or_else(|| "—".into())
}

// ------------------------------------------------------------------ chassis

/// The shared chassis drawing with synohwd's bays, the M.2 slots of the hardware status and the
/// drives of the storage inventory; identify and the disk-LED switch are the Synology parts.
fn chassis(st: Remote<SynologyStatus>, d: RwSignal<SynologySettings>, running: bool) -> AnyView {
    let live = crate::live::use_live();
    let sto = crate::pages::storage::sto();
    if sto.disks.with_untracked(Option::is_none) {
        sto.refresh(false);
    }
    let bays = Memo::new(move |_| {
        sto.disks.with(|dk| {
            let dk = dk.as_deref().unwrap_or(&[]);
            let mut v: Vec<_> = st.data.with(|v| {
                v.iter()
                    .flat_map(|v| v.bays.iter())
                    .map(|b| crate::chassis::bay_slot(b.number, b.present, b.powered, b.asleep, b.temperature_c, b.device.clone(), dk))
                    .collect()
            });
            v.sort_by_key(|x| x.key);
            v
        })
    });
    let m2 = Memo::new(move |_| {
        sto.disks.with(|dk| live.hardware.with(|h| h.as_ref().map(|h| crate::chassis::m2_slots(&h.m2, dk.as_deref().unwrap_or(&[]))).unwrap_or_default()))
    });
    let model = Memo::new(move |_| st.data.with(|v| v.as_ref().map(|v| v.model.clone()).unwrap_or_default()));
    let led = Memo::new(move |_| st.data.with(|v| Some(v.as_ref().and_then(|v| v.status_led).map_or("off", led_name).to_string())));
    let disk_leds = Memo::new(move |_| d.with(|x| x.disk_leds));
    crate::chassis::chassis(crate::chassis::Chassis {
        model: model.into(),
        bays,
        m2,
        status_led: led.into(),
        on_select: None,
        identify: Some(crate::chassis::Identify { running: Signal::derive(move || running), disk_leds: disk_leds.into() }),
    })
}

// ------------------------------------------------------------------ fans

fn fans_card(d: RwSignal<SynologySettings>, st: Remote<SynologyStatus>, curves: Remote<FanCurves>) -> AnyView {
    let (mode, set_mode) = lens(d, |s| fan_mode_str(s.fan_mode).into(), |s, v| s.fan_mode = fan_mode_of(&v));
    let (period, set_period) = lens(d, |s| s.fan_period_s.to_string(), |s, v| s.fan_period_s = v.trim().parse::<u32>().map_or(0, |n| n.min(9999) as u16));
    let (min_duty, set_min_duty) = lens(d, |s| s.fan_min_duty_pct.to_string(), |s, v| s.fan_min_duty_pct = v.trim().parse::<u32>().map_or(0, |n| n.min(255) as u8));
    let (hyst, set_hyst) = lens(d, |s| s.hysteresis_c.to_string(), |s, v| s.hysteresis_c = v.trim().parse::<u32>().map_or(0, |n| n.min(255) as u8));
    let range = |min: u32, max: u32| tf("hw.v_range", &[("min", &min.to_string()), ("max", &max.to_string())]);
    let e_period = Signal::derive(move || d.with(|s| !(5..=300).contains(&s.fan_period_s)).then(|| range(5, 300)));
    let e_duty = Signal::derive(move || d.with(|s| s.fan_min_duty_pct > 99).then(|| range(0, 99)));
    let e_hyst = Signal::derive(move || d.with(|s| s.hysteresis_c > 20).then(|| range(0, 20)));
    let mode_help = move || match d.with(|s| s.fan_mode) {
        FanMode::Low => t("syn.fan_low_help"),
        FanMode::High => t("syn.fan_high_help"),
        FanMode::Full => t("syn.fan_full_help"),
    };
    // built once: the duty and every fan row follow the status (rows keyed by fan number)
    let duty = Memo::new(move |_| st.data.with(|v| v.as_ref().and_then(|v| v.fan_duty_pct)));
    let fan_nums = Memo::new(move |_| st.data.with(|v| v.as_ref().map(|v| v.fans.iter().map(|f| f.number).collect::<Vec<u32>>()).unwrap_or_default()));
    let live = move || {
        view! {
            <div class="sub-row"><span>{tr("hw.duty")}</span><span class="tnum">{move || pct_text(duty.get())}</span></div>
            <Meter pct=Signal::derive(move || f64::from(duty.get().unwrap_or(0))) />
            <ul class="syn-fans">
                <For each=move || fan_nums.get() key=|n| *n children=move |n| fan_row(st, n) />
            </ul>
        }
        .into_any()
    };
    let chart = move || {
        let mode = Memo::new(move |_| d.with(|s| s.fan_mode));
        let full = Memo::new(move |_| mode.get() == FanMode::Full);
        let fig = Memo::new(move |_| {
            let mode = mode.get();
            curves.data.with(|fc| {
                let fc = fc.as_ref()?;
                let mc = mode_curves(fc, mode).filter(|_| mode != FanMode::Full)?;
                let shown = shown_curves(mc, &fc.readings);
                let svg = curve_svg(&shown, &fc.readings, if mode == fc.active_mode { fc.duty_pct } else { None });
                let aria = shown
                    .iter()
                    .map(|c| match now_of(fc, c.class) {
                        Some(n) => format!("{} {} → {}%", t(class_key(c.class)), i18n::celsius(n), duty_at(c, n)),
                        None => t(class_key(c.class)).to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                Some((svg, aria, shown.iter().map(|c| c.class).collect::<Vec<_>>(), mode != fc.active_mode))
            })
        });
        let has = Memo::new(move |_| fig.with(Option::is_some));
        let svg = Memo::new(move |_| fig.with(|f| f.as_ref().map(|f| f.0.clone()).unwrap_or_default()));
        let aria = Memo::new(move |_| fig.with(|f| f.as_ref().map(|f| format!("{}: {}", t("syn.curve"), f.1)).unwrap_or_default()));
        let legend = Memo::new(move |_| fig.with(|f| f.as_ref().map(|f| f.2.clone()).unwrap_or_default()));
        let preview = Memo::new(move |_| fig.with(|f| f.as_ref().is_some_and(|f| f.3)));
        view! {
            {move || full.get().then(|| view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("syn.full_note")}</p> })}
            {move || has.get().then(|| view! {
                <figure class="cv-wrap">
                    <div role="img" aria-label=move || aria.get() inner_html=move || svg.get()></div>
                    <figcaption>
                        <ul class="cv-legend">
                            {move || legend.get().into_iter().map(|c| {
                                let color = class_color(c);
                                view! { <li><span class="dot" style=format!("background:var({color})")></span>{tr(class_key(c))}</li> }
                            }).collect_view()}
                            <li><span class="cv-duty-key"></span>{tr("syn.curve_duty")}</li>
                        </ul>
                        {move || preview.get().then(|| view! { <span class="muted small">{tr("syn.curve_preview")}</span> })}
                    </figcaption>
                </figure>
            })}
        }
        .into_any()
    };
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::FAN title="hw.fans" />
            <div class="form">
                {segmented("syn.fan_mode", FAN_MODES, mode, set_mode)}
                <p class="field-help">{mode_help}</p>
                {st.view(3, live)}
                {curves.view(4, chart)}
                <details class="advanced">
                    <summary>{tr("advanced")}</summary>
                    <div class="form">
                        <div class="form-row three">
                            {text_field("syn.fan_period", period, set_period, e_period, In { kind: "number", help: Some("syn.fan_period_help"), ..In::default() })}
                            {text_field("syn.fan_min_duty", min_duty, set_min_duty, e_duty, In { kind: "number", help: Some("syn.fan_min_duty_help"), ..In::default() })}
                            {text_field("syn.hysteresis", hyst, set_hyst, e_hyst, In { kind: "number", help: Some("syn.hysteresis_help"), ..In::default() })}
                        </div>
                    </div>
                </details>
            </div>
        </section>
    }
    .into_any()
}

/// One fan of the status (its rpm and alarm follow the polls; the row stays).
fn fan_row(st: Remote<SynologyStatus>, n: u32) -> AnyView {
    let fan = Memo::new(move |_| st.data.with(|v| v.as_ref().and_then(|v| v.fans.iter().find(|f| f.number == n)).map(|f| (f.rpm, f.alarm))));
    let rpm = Memo::new(move |_| fan.get().and_then(|f| f.0));
    let alarm = Memo::new(move |_| fan.get().map_or(FanAlarm::Unknown, |f| f.1));
    view! {
        <li class="fan-row">
            <span class=move || if alarm.get() == FanAlarm::Stopped { "fan-icon alarm" } else { "fan-icon" }><Icon icon=icons::FAN size=16 /></span>
            <span>{move || tf("hw.fan_n", &[("n", &n.to_string())])}</span>
            <span class="tnum fan-rpm-s">{move || rpm.get().map(|r| format!("{r} rpm")).unwrap_or_else(|| "—".into())}</span>
            {move || {
                let (tone, key) = match alarm.get() {
                    FanAlarm::Ok => (Tone::Ok, "syn.fan_ok"),
                    FanAlarm::Stopped => (Tone::Err, "syn.fan_stopped"),
                    FanAlarm::Unknown => (Tone::Unknown, "syn.fan_unknown"),
                };
                status(tone, t(key).to_string())
            }}
        </li>
    }
    .into_any()
}

/// Hottest current reading of a sensor class.
fn now_of(fc: &FanCurves, class: SensorClass) -> Option<f32> {
    fc.readings.iter().filter(|r| r.class == class).filter_map(|r| r.temperature_c).fold(None::<f32>, |a, b| Some(a.map_or(b, |a| a.max(b))))
}

/// One sensor class of the thermal table: reading and shutdown limit follow the curves.
fn thermal_row(rows: Memo<Vec<(SensorClass, Option<f32>, u8)>>, class: SensorClass) -> AnyView {
    let row = Memo::new(move |_| rows.with(|r| r.iter().find(|x| x.0 == class).map(|x| (x.1, x.2))));
    let now = Memo::new(move |_| row.get().and_then(|r| r.0));
    let limit = Memo::new(move |_| row.get().map_or(0, |r| r.1));
    let near = Memo::new(move |_| now.get().is_some_and(|n| n + 5.0 >= f32::from(limit.get())));
    view! {
        <div class="kv">
            <dt>{tr(class_key(class))}</dt>
            <dd class="tnum">
                <span class=move || if near.get() { "temp-val err" } else { "muted" }>{move || now.get().map(i18n::celsius).unwrap_or_else(|| "—".into())}</span>
                " / "
                <strong>{move || i18n::celsius(f32::from(limit.get()))}</strong>
            </dd>
        </div>
    }
    .into_any()
}

// ------------------------------------------------------------------ thermal

fn thermal_card(d: RwSignal<SynologySettings>, curves: Remote<FanCurves>) -> AnyView {
    let (on, set_on) = lens_bool(d, |s| s.thermal_shutdown, |s, v| s.thermal_shutdown = v);
    // rows keyed by sensor class; a poll only rewrites the readings that changed
    let rows = Memo::new(move |_| {
        let mode = d.with(|s| s.fan_mode);
        curves.data.with(|fc| {
            fc.as_ref()
                .and_then(|fc| mode_curves(fc, mode).map(|mc| shown_curves(mc, &fc.readings).into_iter().map(|c| (c.class, now_of(fc, c.class), c.shutdown_c)).collect::<Vec<_>>()))
                .unwrap_or_default()
        })
    });
    let table = move || {
        view! {
            <div class="sub-row muted small"><span>{tr("syn.sensor")}</span><span>{tr("syn.now_limit")}</span></div>
            <dl class="kvs">
                <For each=move || rows.with(|r| r.iter().map(|x| x.0).collect::<Vec<_>>()) key=|c| class_key(*c) children=move |c| thermal_row(rows, c) />
            </dl>
        }
        .into_any()
    };
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::THERMOMETER title="syn.thermal" />
            <div class="form">
                {switch("syn.thermal_on", Some("syn.thermal_help"), on, set_on)}
                {move || (!on.get()).then(|| view! {
                    {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {t("syn.thermal_off")} }.into_any())}
                })}
                {curves.view(4, table)}
            </div>
        </section>
    }
    .into_any()
}

// ------------------------------------------------------------------ LEDs and beeper

fn leds_card(d: RwSignal<SynologySettings>, running: bool) -> AnyView {
    let (leds, set_leds) = lens_bool(d, |s| s.disk_leds, |s, v| s.disk_leds = v);
    let (start, set_start) = lens_bool(d, |s| s.beep_on_start, |s, v| s.beep_on_start = v);
    let (err, set_err) = lens_bool(d, |s| s.beep_on_error, |s, v| s.beep_on_error = v);
    let bright = move || d.with(|s| s.led_brightness);
    let pct = move || format!("{}%", (u32::from(bright()) * 100).div_ceil(255));
    let rid = crate::form::fid();
    let beeping = RwSignal::new(false);
    let muting = RwSignal::new(false);
    let beep = move |_| {
        beeping.set(true);
        spawn_local(async move {
            let r = api::post_empty("/api/v1/modules/synology/beep", Some(&BeepRequest { kind: BeepKind::Short })).await;
            beeping.set(false);
            match r {
                Ok(()) => toast(Tone::Ok, t("syn.beep_sent").to_string()),
                Err(e) => toast(Tone::Err, e.text()),
            }
        });
    };
    let mute = move |_| {
        muting.set(true);
        spawn_local(async move {
            let r = api::post::<(), MuteResult>("/api/v1/modules/synology/mute", None).await;
            muting.set(false);
            match r {
                Ok(m) => toast(Tone::Ok, tf("syn.muted", &[("n", &m.acknowledged.to_string())])),
                Err(e) => toast(Tone::Err, e.text()),
            }
        });
    };
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::SUN title="syn.leds_beeper" />
            <div class="form">
                {switch("syn.disk_leds", Some("syn.disk_leds_help"), leds, set_leds)}
                <div class="field">
                    <label class="field-label" for=rid.clone()>{tr("syn.brightness")}<span class="tnum muted">" "{pct}</span></label>
                    <input id=rid class="range" type="range" min="1" max="255" step="1"
                        aria-valuetext=pct
                        prop:value=move || bright().to_string()
                        on:input=move |ev| { let v = event_target_value(&ev).parse::<u8>().unwrap_or(255).max(1); d.update(|s| s.led_brightness = v); } />
                    <span class="field-help">{tr("syn.brightness_help")}</span>
                </div>
                {switch("syn.beep_start", None, start, set_start)}
                {switch("syn.beep_error", Some("syn.beep_error_help"), err, set_err)}
                <div class="btn-row">
                    <button type="button" class="btn ghost" disabled=move || beeping.get() || !running on:click=beep>
                        {crate::form::spinner(beeping)}<Icon icon=icons::BELL_RING size=16 />{tr("syn.test_beep")}
                    </button>
                    <button type="button" class="btn ghost" disabled=move || muting.get() on:click=mute>
                        {crate::form::spinner(muting)}<Icon icon=icons::BELL_OFF size=16 />{tr("syn.mute")}
                    </button>
                </div>
                <p class="field-help">{tr("syn.mute_help")}</p>
            </div>
        </section>
    }
    .into_any()
}

// ------------------------------------------------------------------ buttons

fn buttons_card(d: RwSignal<SynologySettings>) -> AnyView {
    let (pb, set_pb) = lens(d, |s| pb_str(s.power_button).into(), |s, v| s.power_button = pb_of(&v));
    let (rb, set_rb) = lens(d, |s| rb_str(s.reset_button).into(), |s, v| s.reset_button = rb_of(&v));
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::SETTINGS_2 title="syn.buttons" />
            <div class="form">
                {select_field("syn.power_button", Some("syn.power_button_help"), POWER_BUTTON, pb, set_pb)}
                {select_field("reset_button", None, RESET_BUTTON, rb, set_rb)}
                <p class="sec-note">
                    <Icon icon=icons::LIFE_BUOY size=16 />
                    <span>{move || if d.with(|s| s.reset_button == ResetButtonAction::NetworkRescue) { t("syn.rescue_help") } else { t("syn.rescue_off") }}</span>
                </p>
            </div>
        </section>
    }
    .into_any()
}

// ------------------------------------------------------------------ power and disks

fn power_card(d: RwSignal<SynologySettings>) -> AnyView {
    let (standby, set_standby) =
        lens(d, |s| s.disk_standby_minutes.to_string(), |s, v| s.disk_standby_minutes = v.trim().parse::<u32>().map_or(0, |n| n.min(9999) as u16));
    let e_standby = Signal::derive(move || {
        d.with(|s| s.disk_standby_minutes > 330).then(|| tf("hw.v_range", &[("min", "0"), ("max", "330")]))
    });
    let (wol, set_wol) = lens_bool(d, |s| s.wol, |s, v| s.wol = v);
    let (ac, set_ac) = lens_bool(d, |s| s.ac_recovery, |s, v| s.ac_recovery = v);
    let ifaces = Remote::<Vec<NetInterface>>::new("/api/v1/network/interfaces");
    let macs = move || {
        ifaces.data.with(|v| {
            let list: Vec<(String, String)> = v
                .iter()
                .flatten()
                .filter(|i| i.physical && i.kind == LinkKind::Ethernet)
                .filter_map(|i| Some((i.name.clone(), i.mac.clone()?)))
                .collect();
            if list.is_empty() {
                return view! { <p class="muted small">{t("syn.no_macs")}</p> }.into_any();
            }
            view! {
                <ul class="syn-macs">
                    {list.into_iter().map(|(name, mac)| view! {
                        <li><span class="mono">{name}</span><span class="mono tnum">{mac.clone()}</span>{copy_button("sto.copy", mac)}</li>
                    }).collect_view()}
                </ul>
            }
            .into_any()
        })
    };
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::UNPLUG title="syn.power" />
            <div class="form">
                {text_field("syn.standby", standby, set_standby, e_standby, In { kind: "number", help: Some("syn.standby_help"), ..In::default() })}
                {switch("syn.ac_recovery", Some("syn.ac_recovery_help"), ac, set_ac)}
                {switch("syn.wol", Some("syn.wol_help"), wol, set_wol)}
                <div class="field">
                    <span class="field-label">{tr("syn.macs")}</span>
                    {ifaces.view(2, macs)}
                </div>
            </div>
        </section>
    }
    .into_any()
}

// ------------------------------------------------------------------ events

fn events_card(st: Remote<SynologyStatus>) -> AnyView {
    // built once: the boot lines follow the status, events are keyed rows (newest first; an
    // event never changes, so a new one only adds its row)
    let list = move || {
        let boot = Memo::new(move |_| st.data.with(|v| v.as_ref().and_then(|v| v.boot.as_ref()).map(|b| (b.boot_ms, b.previous_shutdown_clean))));
        let has_boot = Memo::new(move |_| boot.with(Option::is_some));
        let events = Memo::new(move |_| {
            st.data.with(|v| {
                let mut seen: Vec<(Option<u64>, &str)> = Vec::new();
                v.iter()
                    .flat_map(|v| v.events.iter().rev())
                    .map(|e| {
                        // identical events (same time and text) get a running number as key
                        let n = seen.iter().filter(|s| s.0 == e.at_ms && s.1 == e.text).count() as u32;
                        seen.push((e.at_ms, &e.text));
                        (e.at_ms, e.text.clone(), n)
                    })
                    .collect::<Vec<_>>()
            })
        });
        let none = Memo::new(move |_| events.with(Vec::is_empty));
        view! {
            {move || has_boot.get().then(|| view! {
                <dl class="kvs syn-boot">
                    <div class="kv"><dt>{tr("last_boot")}</dt><dd>{move || boot.get().map(|b| i18n::datetime(b.0)).unwrap_or_default()}</dd></div>
                    <div class="kv"><dt>{tr("ps.prev_clean")}</dt><dd>{move || match boot.get().and_then(|b| b.1) {
                        Some(true) => t("ps.clean_yes"),
                        Some(false) => t("ps.clean_no"),
                        None => "—",
                    }}</dd></div>
                </dl>
            })}
            <ul class="events">
                {move || none.get().then(|| view! { <li class="muted">{tr("hw.no_events")}</li> })}
                <For each=move || events.get() key=|e| e.clone() let:e>
                    <li><span class="mono small muted">{e.0.map(i18n::short_when).unwrap_or_else(|| "—".into())}</span><span lang="en">{e.1}</span></li>
                </For>
            </ul>
        }
        .into_any()
    };
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::HISTORY title="hw.events" />
            {st.view(4, list)}
        </section>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curve() -> FanCurve {
        FanCurve {
            class: SensorClass::Disk,
            points: vec![
                FanPoint { threshold_c: 0, duty_pct: 20 },
                FanPoint { threshold_c: 46, duty_pct: 30 },
                FanPoint { threshold_c: 51, duty_pct: 50 },
                FanPoint { threshold_c: 58, duty_pct: 99 },
            ],
            shutdown_c: 61,
        }
    }

    #[test]
    fn duty_follows_the_steps() {
        let c = curve();
        assert_eq!(duty_at(&c, 34.0), 20);
        assert_eq!(duty_at(&c, 46.0), 30);
        assert_eq!(duty_at(&c, 50.9), 30);
        assert_eq!(duty_at(&c, 60.0), 99);
    }

    #[test]
    fn chart_marks_readings() {
        let c = curve();
        let r = vec![SensorReading { class: SensorClass::Disk, sensor: "bay<1>".into(), temperature_c: Some(34.0) }];
        let svg = curve_svg(&[&c], &r, Some(30));
        assert!(svg.starts_with("<svg") && svg.ends_with("</svg>"));
        assert!(svg.contains("<circle"));
        assert!(svg.contains("bay&lt;1&gt;"));
        assert!(svg.contains("cv-duty"));
    }

    #[test]
    fn ranges_like_synohwd() {
        let mut s = SynologySettings::default();
        assert_eq!(range_error(&s), None);
        s.fan_period_s = 4;
        assert_eq!(range_error(&s), Some("fan_period_s"));
        s.fan_period_s = 300;
        s.disk_standby_minutes = 331;
        assert_eq!(range_error(&s), Some("disk_standby_minutes"));
        s.disk_standby_minutes = 330;
        s.led_brightness = 0;
        assert_eq!(range_error(&s), Some("led_brightness"));
        s.led_brightness = 1;
        s.led_reduced_level = 8;
        assert_eq!(range_error(&s), Some("led_reduced_level"));
        s.led_reduced_level = 7;
        s.led_schedule = "0".repeat(167);
        assert_eq!(range_error(&s), Some("led_schedule"));
        s.led_schedule = "2".repeat(168);
        assert_eq!(range_error(&s), None);
    }
}
