//! Hardware › QNAP: LEDs, bays, buttons, buzzer and what is available on this machine.

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_hw_qnap::api::{BeepKind, BeepRequest, ButtonAction, MuteResult, QnapSettings, QnapSettingsState, QnapStatus, StatusLed, UsbLed};

use crate::api;
use crate::form::{Draft, lens, lens_bool, select_field, stale_notice, switch};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::toast::{saved_pending, toast};
use crate::ui::{self, EmptyState, Remote, Tone, WidgetHead, status};

const SETTINGS: &str = "/api/v1/modules/qnap/settings";
const POLL: Duration = Duration::from_secs(10);
const STATUS_LEDS: &[(&str, &str)] = &[
    ("auto", "qnap.led_auto"),
    ("green", "qnap.led_green"),
    ("green_blink", "qnap.led_green_blink"),
    ("red", "qnap.led_red"),
    ("red_blink", "qnap.led_red_blink"),
    ("alternate", "qnap.led_alternate"),
    ("off", "qnap.led_off"),
];
const BUTTONS: &[(&str, &str)] = &[("notify", "sch.f.notify"), ("log", "qnap.btn_log"), ("ignore", "qnap.btn_ignore")];
const USB_LEDS: &[(&str, &str)] = &[("off", "qnap.led_off"), ("on", "qnap.usb_on"), ("blink", "qnap.usb_blink")];

fn usb_str(u: UsbLed) -> &'static str {
    match u {
        UsbLed::Off => "off",
        UsbLed::On => "on",
        UsbLed::Blink => "blink",
    }
}

fn usb_of(s: &str) -> UsbLed {
    match s {
        "on" => UsbLed::On,
        "blink" => UsbLed::Blink,
        _ => UsbLed::Off,
    }
}

fn led_str(l: StatusLed) -> &'static str {
    match l {
        StatusLed::Auto => "auto",
        StatusLed::Green => "green",
        StatusLed::GreenBlink => "green_blink",
        StatusLed::Red => "red",
        StatusLed::RedBlink => "red_blink",
        StatusLed::Alternate => "alternate",
        StatusLed::Off => "off",
    }
}

fn led_of(s: &str) -> StatusLed {
    match s {
        "green" => StatusLed::Green,
        "green_blink" => StatusLed::GreenBlink,
        "red" => StatusLed::Red,
        "red_blink" => StatusLed::RedBlink,
        "alternate" => StatusLed::Alternate,
        "off" => StatusLed::Off,
        _ => StatusLed::Auto,
    }
}

fn btn_str(a: ButtonAction) -> &'static str {
    match a {
        ButtonAction::Notify => "notify",
        ButtonAction::Log => "log",
        ButtonAction::Ignore => "ignore",
    }
}

fn btn_of(s: &str) -> ButtonAction {
    match s {
        "log" => ButtonAction::Log,
        "ignore" => ButtonAction::Ignore,
        _ => ButtonAction::Notify,
    }
}

fn feature_key(id: &str) -> &'static str {
    match id {
        "status_led" => "qnap.feat_status_led",
        "disk_leds" => "qnap.feat_disk_leds",
        "usb_led" => "qnap.feat_usb_led",
        "reset_button" => "qnap.feat_reset_button",
        "usb_copy_button" => "qnap.feat_usb_copy_button",
        _ => "qnap.feat_buzzer",
    }
}

fn why_key(reason: &str) -> &'static str {
    match reason {
        "wrong_direction" => "qnap.why_wrong_direction",
        "led_control_off" => "qnap.why_led_control_off",
        "no_speaker" => "qnap.why_no_speaker",
        _ => "qnap.why_driver_missing",
    }
}

pub fn panel(active: Signal<bool>) -> AnyView {
    let settings = Remote::<QnapSettingsState>::with_config(SETTINGS);
    let st = Remote::<QnapStatus>::new("/api/v1/modules/qnap/status");
    Effect::new(move |prev: Option<()>| {
        crate::config::generation();
        if prev.is_some() {
            st.reload();
        }
    });
    if let Ok(h) = set_interval_with_handle(
        move || {
            if active.get_untracked() && !document().hidden() && settings.err.with_untracked(Option::is_none) {
                st.reload();
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
        let s = settings.data.get_untracked().unwrap_or_default();
        body(s, settings, st)
    })
    .into_any()
}

fn body(s: QnapSettingsState, settings: Remote<QnapSettingsState>, st: Remote<QnapStatus>) -> AnyView {
    let init = s.pending.clone().unwrap_or_else(|| s.applied.clone());
    let draft = Draft::new(init, move || settings.data.with(|s| s.as_ref().map(|s| s.pending.clone().unwrap_or_else(|| s.applied.clone()))));
    let d = draft.d;
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let dirty = draft.dirty();
    let save = move || {
        busy.set(true);
        error.set(None);
        let body = d.get_untracked();
        spawn_local(async move {
            let r = api::put::<_, QnapSettingsState>(SETTINGS, &body).await;
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
    let unverified = Memo::new(move |_| st.data.with(|s| s.as_ref().is_some_and(|s| !s.verified)));
    view! {
        {move || unverified.get().then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {t("qnap.not_verified")} }.into_any()))}
        <div class="grid">
            {leds_card(d, st)}
            {bays_card(st)}
            {buttons_card(d)}
            {buzzer_card(d, st)}
            {events_card(st)}
            {features_card(st)}
        </div>
        <div class="stage-bar" hidden=move || !dirty.get() && error.with(Option::is_none) role="region" aria-label=tr("qnap.unsaved")>
            <Icon icon=icons::INFO size=16 />
            <span class="stage-text">{tr("qnap.unsaved")}</span>
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

fn leds_card(d: RwSignal<QnapSettings>, st: Remote<QnapStatus>) -> AnyView {
    let (control, set_control) = lens_bool(d, |s| s.led_control, |s, v| s.led_control = v);
    let (mode, set_mode) = lens(d, |s| led_str(s.status_led).into(), |s, v| s.status_led = led_of(&v));
    let (disk, set_disk) = lens_bool(d, |s| s.disk_leds, |s, v| s.disk_leds = v);
    let (usb, set_usb) = lens(d, |s| usb_str(s.usb_led).into(), |s, v| s.usb_led = usb_of(&v));
    let shown = move || {
        st.data.with(|s| match s.as_ref().map(|s| s.status_led.as_str()) {
            Some("") | None => t("qnap.shown_none").to_string(),
            Some(w) => t(match w {
                "green" => "qnap.led_green",
                "green-blink" => "qnap.led_green_blink",
                "red" => "qnap.led_red",
                "red-blink" => "qnap.led_red_blink",
                "alternate" => "qnap.led_alternate",
                _ => "qnap.led_off",
            })
            .to_string(),
        })
    };
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::SETTINGS_2 title="qnap.leds" />
            <div class="form">
                {switch("qnap.led_control", Some("qnap.led_control_help"), control, set_control)}
                {select_field("hw.led", None, STATUS_LEDS, mode, set_mode)}
                {switch("qnap.disk_leds", None, disk, set_disk)}
                {select_field("qnap.usb_led", None, USB_LEDS, usb, set_usb)}
                <p class="muted small">{tr("qnap.shown")}": "{shown}</p>
            </div>
        </section>
    }
    .into_any()
}

fn bays_card(st: Remote<QnapStatus>) -> AnyView {
    let rows = move || {
        st.data.with(|s| {
            s.as_ref().map(|s| s.bays.clone()).unwrap_or_default().into_iter().map(|b| {
                let state = if b.device.is_empty() {
                    t("qnap.bay_empty").to_string()
                } else {
                    format!(
                        "{}: {}",
                        b.device,
                        t(match b.led.as_str() {
                            "on" => "qnap.bay_on",
                            "blink" => "qnap.bay_blink",
                            _ => "qnap.bay_off",
                        })
                    )
                };
                view! { <li><strong>{tf("qnap.bay_n", &[("n", &b.bay.to_string())])}</strong>" "<span class="muted">{state}</span></li> }
            }).collect_view()
        })
    };
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::BOX title="qnap.bays" />
            <ul class="plain-list">{rows}</ul>
        </section>
    }
    .into_any()
}

fn buttons_card(d: RwSignal<QnapSettings>) -> AnyView {
    let (reset, set_reset) = lens(d, |s| btn_str(s.reset_button).into(), |s, v| s.reset_button = btn_of(&v));
    let (usb, set_usb) = lens(d, |s| btn_str(s.usb_copy_button).into(), |s, v| s.usb_copy_button = btn_of(&v));
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::SETTINGS_2 title="qnap.buttons" />
            <div class="form">
                {select_field("reset_button", None, BUTTONS, reset, set_reset)}
                {select_field("qnap.usb_copy", None, BUTTONS, usb, set_usb)}
                <p class="sec-note"><Icon icon=icons::LOCK size=16 /><span>{tr("qnap.buttons_help")}</span></p>
            </div>
        </section>
    }
    .into_any()
}

fn buzzer_card(d: RwSignal<QnapSettings>, st: Remote<QnapStatus>) -> AnyView {
    let (failure, set_failure) = lens_bool(d, |s| s.beep_on_disk_failure, |s, v| s.beep_on_disk_failure = v);
    let busy = RwSignal::new(false);
    let alarm = Memo::new(move |_| st.data.with(|s| s.as_ref().is_some_and(|s| s.alarm)));
    let buzzer_ok = Memo::new(move |_| {
        st.data.with(|s| s.as_ref().is_some_and(|s| s.features.iter().any(|f| f.id == "buzzer" && f.available)))
    });
    let beep = move |_| {
        busy.set(true);
        spawn_local(async move {
            let r = api::post_empty("/api/v1/modules/qnap/beep", Some(&BeepRequest { kind: BeepKind::Short })).await;
            busy.set(false);
            match r {
                Ok(()) => toast(Tone::Ok, t("qnap.beep_sent").to_string()),
                Err(e) => toast(Tone::Err, e.text()),
            }
        });
    };
    let mute = move |_| {
        spawn_local(async move {
            match api::post::<_, MuteResult>("/api/v1/modules/qnap/mute", None::<&()>).await {
                Ok(_) => st.reload(),
                Err(e) => toast(Tone::Err, e.text()),
            }
        });
    };
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::BELL_RING title="qnap.buzzer" />
            <div class="form">
                {switch("qnap.beep_failure", Some("qnap.beep_failure_help"), failure, set_failure)}
                {move || alarm.get().then(|| status(Tone::Err, t("qnap.alarm_on").to_string()))}
                <div class="row">
                    <button type="button" class="btn ghost" disabled=move || busy.get() || !buzzer_ok.get() on:click=beep>
                        {crate::form::spinner(busy)}<Icon icon=icons::BELL_RING size=16 />{tr("qnap.test_beep")}
                    </button>
                    <button type="button" class="btn ghost" disabled=move || !alarm.get() on:click=mute>{tr("qnap.mute")}</button>
                </div>
            </div>
        </section>
    }
    .into_any()
}

fn events_card(st: Remote<QnapStatus>) -> AnyView {
    let rows = move || {
        let events = st.data.with(|s| s.as_ref().map(|s| s.events.clone()).unwrap_or_default());
        if events.is_empty() {
            return view! { <p class="muted small">{tr("qnap.events_none")}</p> }.into_any();
        }
        events
            .into_iter()
            .rev()
            .map(|e| {
                let name = t(if e.button == "reset" { "reset_button" } else { "qnap.usb_copy" });
                let held = tf("qnap.held", &[("s", &format!("{:.1}", f64::from(e.held_ms) / 1000.0))]);
                view! { <p class="small">{i18n::short_when(e.at * 1000)}" · "{name}" · "{held}</p> }
            })
            .collect_view()
            .into_any()
    };
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::INFO title="qnap.events" />
            {rows}
        </section>
    }
    .into_any()
}

fn features_card(st: Remote<QnapStatus>) -> AnyView {
    let rows = move || {
        st.data.with(|s| {
            s.as_ref().map(|s| s.features.clone()).unwrap_or_default().into_iter().map(|f| {
                let (tone, text) = if f.available {
                    (Tone::Ok, t("qnap.available").to_string())
                } else {
                    (Tone::Warn, t(why_key(&f.reason)).to_string())
                };
                view! { <li><strong>{t(feature_key(&f.id))}</strong>" "{status(tone, text)}</li> }
            }).collect_view()
        })
    };
    view! {
        <section class="card widget w-12">
            <WidgetHead icon=icons::CPU title="qnap.features" />
            <ul class="plain-list">{rows}</ul>
        </section>
    }
    .into_any()
}
