//! The chassis drawing, shared by Storage › Overview and Hardware › Synology (guidelines 2.1):
//! the drive bays side by side as on the unit, the M.2 slots below them, and the optional parts
//! of a hardware provider (front status LED, identify per bay, disk LEDs switched off).
//!
//! The drawing is built once. Slots are rows keyed by their number (`<For>`) and every value a
//! tile shows is its own memo, so a status update only touches the text and classes that
//! changed (S3): hover, focus and an identify blink survive every poll.

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::M2Slot;
use rynas_api_types::storage::{DiskSummary, HealthVerdict};
use rynas_api_types::IdentifyRequest;

use crate::api;
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::pages::dashboard::led_text;
use crate::pages::storage::{asleep, health, m2_label};
use crate::toast::toast;
use crate::ui::{Tone, temp_tone};

/// Identify blink of the API's default duration.
const IDENTIFY_S: u64 = 30;

/// What a tile shows of the drive in a slot (from the storage inventory).
#[derive(Clone, PartialEq, Debug)]
pub struct Disk {
    pub name: String,
    pub size: u64,
    pub model: String,
    pub health: HealthVerdict,
    pub temperature_c: Option<f32>,
}

/// One drive slot: a bay or an M.2 slot.
#[derive(Clone, PartialEq, Debug)]
pub struct Slot {
    /// Row key: the bay number, the M.2 slot number (1000 + position when unknown).
    pub key: u32,
    /// Printed on the tile: "1", "M.2 1".
    pub title: String,
    /// Spoken: "Bay 1", "M.2 1".
    pub name: String,
    pub m2: bool,
    pub present: bool,
    pub powered: bool,
    pub asleep: bool,
    pub celsius: Option<f32>,
    /// The provider's device name, shown when no drive summary matches it.
    pub device: Option<String>,
    pub disk: Option<Disk>,
}

fn disk_of(dev: Option<&String>, disks: &[DiskSummary]) -> Option<Disk> {
    let d = disks.iter().find(|d| Some(&d.name) == dev)?;
    Some(Disk { name: d.name.clone(), size: d.size, model: d.model.clone(), health: d.health, temperature_c: d.temperature_c })
}

/// A bay, with the drive the inventory knows in it.
pub fn bay_slot(number: u32, present: bool, powered: bool, asleep: bool, celsius: Option<f32>, device: Option<String>, disks: &[DiskSummary]) -> Slot {
    Slot {
        key: number,
        title: number.to_string(),
        name: format!("{} {number}", t("sto.bay")),
        m2: false,
        present,
        powered,
        asleep,
        celsius,
        disk: disk_of(device.as_ref(), disks),
        device,
    }
}

/// The M.2 slots of a provider, with their drives.
pub fn m2_slots(m2: &[M2Slot], disks: &[DiskSummary]) -> Vec<Slot> {
    m2.iter()
        .enumerate()
        .map(|(i, s)| {
            let power = disks.iter().find(|d| Some(&d.name) == s.device.as_ref()).map(|d| d.power_state);
            Slot {
                key: s.number.unwrap_or(1000 + i as u32),
                title: m2_label(s.number),
                name: m2_label(s.number),
                m2: true,
                present: s.present,
                powered: s.present,
                // NVMe drives have no provider sleep flag: the disk's own power state
                asleep: power.is_some_and(asleep),
                celsius: s.celsius,
                disk: disk_of(s.device.as_ref(), disks),
                device: s.device.clone(),
            }
        })
        .collect()
}

/// Identify buttons and the disk-LED switch of a provider that drives the bay LEDs (Synology).
#[derive(Clone, Copy)]
pub struct Identify {
    /// The provider's daemon runs (identify needs it).
    pub running: Signal<bool>,
    /// Disk activity LEDs enabled: when off, the green bay LEDs stay dark.
    pub disk_leds: Signal<bool>,
}

#[derive(Clone, Copy)]
pub struct Chassis {
    pub model: Signal<String>,
    pub bays: Memo<Vec<Slot>>,
    pub m2: Memo<Vec<Slot>>,
    /// Front status LED as a word ("green", "orange-blink", "off"); no LED drawn when None.
    pub status_led: Signal<Option<String>>,
    /// A tile with a known drive is a button that calls this with the drive's name.
    pub on_select: Option<Callback<String>>,
    pub identify: Option<Identify>,
}

/// State class and text key of a slot.
fn state_of(s: &Slot) -> (&'static str, &'static str) {
    if !s.present {
        ("vacant", "sto.empty_bay")
    } else if !s.powered {
        ("off", "sto.not_powered")
    } else if s.asleep {
        ("asleep", "sto.asleep")
    } else {
        ("active", "sto.active")
    }
}

fn label_of(s: &Slot) -> String {
    let what = match (&s.disk, &s.device) {
        _ if !s.present => String::new(),
        (Some(d), _) => format!(", {} {}, {}", d.model, i18n::bytes_si(d.size), t(health(d.health).1)),
        (None, Some(dev)) => format!(", {dev}"),
        (None, None) => String::new(),
    };
    let temp = temp_of(s).map(|c| format!(", {}", i18n::celsius(c))).unwrap_or_default();
    format!("{}: {}{what}{temp}", s.name, t(state_of(s).1))
}

fn temp_of(s: &Slot) -> Option<f32> {
    if s.present { s.celsius.or(s.disk.as_ref().and_then(|d| d.temperature_c)) } else { None }
}

/// The drawing (built once; everything inside follows the signals).
pub fn chassis(c: Chassis) -> AnyView {
    let led = Memo::new(move |_| c.status_led.get());
    let has_led = Memo::new(move |_| led.with(Option::is_some));
    let led_cls = Memo::new(move |_| format!("led-dot {}", led.get().unwrap_or_default()));
    let led_title = move || format!("{}: {}", t("hw.led"), led_text(&led.get().unwrap_or_default()));
    let cols = Memo::new(move |_| format!("--bays:{};--m2:{}", c.bays.with(Vec::len).max(1), c.m2.with(Vec::len).max(1)));
    let has_m2 = Memo::new(move |_| c.m2.with(|v| !v.is_empty()));
    view! {
        <div class="chassis-wrap">
            <div class="chassis" style=move || cols.get()>
                <div class="chassis-brand">
                    <div class="chassis-top">
                        <span class="chassis-model">{move || c.model.get()}</span>
                        {move || has_led.get().then(|| view! {
                            <span class="chassis-led" title=led_title>
                                <span class="chassis-led-label">{tr("hw.led_status_short")}</span>
                                <span class=move || led_cls.get() aria-hidden="true"></span>
                            </span>
                        })}
                    </div>
                    <span class="chassis-led" title=tr("hw.power_led")>
                        <span class="chassis-led-label">{tr("hw.led_power_short")}</span>
                        <span class="led-dot green" aria-hidden="true"></span>
                    </span>
                </div>
                <div class="chassis-body">
                    <div class="bays" role="list" aria-label=tr("sto.bays")>{slot_list(c.bays, c)}</div>
                    {move || has_m2.get().then(|| view! {
                        <div class="bays m2-slots" role="list" aria-label=tr("sto.m2_slots")>{slot_list(c.m2, c)}</div>
                    })}
                </div>
            </div>
        </div>
    }
    .into_any()
}

/// The tiles of one list, keyed by slot (one compiled copy for bays and M.2 slots).
fn slot_list(slots: Memo<Vec<Slot>>, c: Chassis) -> AnyView {
    view! {
        <For each=move || slots.with(|v| v.iter().map(|s| s.key).collect::<Vec<u32>>()) key=|k| *k children=move |k| tile(k, slots, c) />
    }
    .into_any()
}

/// A memo of one value of a slot.
fn field<T: Clone + PartialEq + Default + Send + Sync + 'static>(slot: Memo<Option<Slot>>, f: fn(&Slot) -> T) -> Memo<T> {
    Memo::new(move |_| slot.with(|s| s.as_ref().map(f).unwrap_or_default()))
}

/// A text span shown while `text` has a value (created once, its text follows the memo).
fn opt_text(text: Memo<Option<String>>, class: &'static str, titled: bool) -> AnyView {
    let has = Memo::new(move |_| text.with(Option::is_some));
    let value = move || text.get().unwrap_or_default();
    (move || has.get().then(|| view! { <span class=class title=move || titled.then(value)>{value}</span> })).into_any()
}

fn tile(k: u32, slots: Memo<Vec<Slot>>, c: Chassis) -> AnyView {
    let slot = Memo::new(move |_| slots.with(|v| v.iter().find(|s| s.key == k).cloned()));
    let m2 = slot.with_untracked(|s| s.as_ref().is_some_and(|s| s.m2));
    let title = field(slot, |s| s.title.clone());
    let state = field(slot, state_of);
    let present = field(slot, |s| s.present);
    let sleeping = field(slot, |s| s.present && s.powered && s.asleep);
    let no_power = field(slot, |s| s.present && !s.powered);
    let selectable = field(slot, |s| s.disk.as_ref().filter(|_| s.present).map(|d| d.name.clone()));
    let dev = field(slot, |s| if s.present { s.disk.as_ref().map(|d| d.name.clone()).or_else(|| s.device.clone()) } else { None });
    let size = field(slot, |s| s.disk.as_ref().filter(|_| s.present).map(|d| i18n::bytes_si(d.size)));
    let model = field(slot, |s| s.disk.as_ref().filter(|_| s.present).map(|d| d.model.clone()));
    let verdict = field(slot, |s| s.disk.as_ref().filter(|_| s.present).map(|d| d.health));
    let temp = field(slot, temp_of);
    let has_temp = Memo::new(move |_| temp.with(Option::is_some));
    let tid = if m2 { "nvme" } else { "bay" };
    let temp_cls = Memo::new(move |_| format!("bay-temp {}", temp_tone(tid, temp.get().unwrap_or(0.0)).class()));
    let label = Memo::new(move |_| slot.with(|s| s.as_ref().map(label_of).unwrap_or_default()));
    let blinking = RwSignal::new(false);
    let class = Memo::new(move |_| {
        let mut cls = format!("bay {}", state.get().0);
        if m2 {
            cls.push_str(" m2");
        }
        // the green LED is dark when disk LEDs are disabled; orange still shows a bay without power
        if c.identify.is_some_and(|i| present.get() && !no_power.get() && !i.disk_leds.get()) {
            cls.push_str(" leds-off");
        }
        if blinking.get() {
            cls.push_str(" identifying");
        }
        cls
    });

    let body = move || {
        let identify = c.identify.filter(|_| !m2).map(|i| {
            move || {
                present.get().then(|| {
                    view! {
                        <button type="button" class="btn ghost sm bay-id" disabled=move || blinking.get() || !i.running.get()
                            on:click=move |ev| { ev.stop_propagation(); start_identify(k, blinking) }>
                            <Icon icon=icons::EYE size=14 />{move || if blinking.get() { t("hw.identifying") } else { t("hw.identify") }}
                        </button>
                    }
                })
            }
        });
        view! {
            <div class="bay-top"><span class="bay-num">{move || title.get()}</span><span class="bay-led" aria-hidden="true"></span></div>
            {(!m2).then(|| view! { <div class="bay-grill" aria-hidden="true"></div> })}
            <div class="bay-info">
                {opt_text(dev, "bay-dev mono", false)}
                {opt_text(size, "bay-size", false)}
                {opt_text(model, "bay-model", true)}
                <span class="bay-state">
                    {move || sleeping.get().then(|| view! { <Icon icon=icons::MOON_STAR size=12 /> })}
                    {move || t(state.get().1)}
                </span>
                {c.identify.is_some().then_some(move || no_power.get().then(|| view! { <span class="bay-state">{tr("hw.bay_no_power")}</span> }))}
                {move || verdict.get().map(|h| {
                    let (tone, key) = health(h);
                    view! { <span class=format!("bay-health {}", tone.class())><Icon icon=tone.icon() size=12 />{tr(key)}</span> }
                })}
                {move || has_temp.get().then(|| view! {
                    // providers keep the last reading of a sleeping disk (it is not woken up)
                    <span class=move || temp_cls.get() title=move || sleeping.get().then(|| t("hw.temp_last"))>
                        <Icon icon=icons::THERMOMETER size=12 />{move || temp.get().map(i18n::celsius).unwrap_or_default()}
                    </span>
                })}
                {identify}
            </div>
        }
    };

    match c.on_select {
        Some(select) => (move || match selectable.get() {
            Some(name) => view! {
                <div role="listitem">
                    <button class=move || class.get() type="button" aria-label=move || label.get() on:click=move |_| select.run(name.clone())>{body()}</button>
                </div>
            }
            .into_any(),
            None => view! { <div role="listitem" aria-label=move || label.get()><div class=move || class.get()>{body()}</div></div> }.into_any(),
        })
        .into_any(),
        None => view! { <div role="listitem" aria-label=move || label.get()><div class=move || class.get()>{body()}</div></div> }.into_any(),
    }
}

/// Blink the bay's LED for the API's default time.
fn start_identify(n: u32, blinking: RwSignal<bool>) {
    blinking.set(true);
    spawn_local(async move {
        let r = api::post_empty(&format!("/api/v1/hardware/bays/{n}/identify"), Some(&IdentifyRequest { duration_s: None })).await;
        match r {
            Ok(()) => {
                toast(Tone::Ok, tf("hw.identify_on", &[("n", &n.to_string()), ("s", &IDENTIFY_S.to_string())]));
                set_timeout(
                    move || {
                        let _ = blinking.try_set(false);
                    },
                    Duration::from_secs(IDENTIFY_S),
                );
            }
            Err(e) => {
                let _ = blinking.try_set(false);
                toast(Tone::Err, e.text());
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot() -> Slot {
        Slot { key: 1, title: "1".into(), name: "Bay 1".into(), m2: false, present: true, powered: true, asleep: false, celsius: None, device: Some("sda".into()), disk: None }
    }

    #[test]
    fn states() {
        let mut s = slot();
        assert_eq!(state_of(&s).0, "active");
        s.asleep = true;
        assert_eq!(state_of(&s).0, "asleep");
        s.powered = false;
        assert_eq!(state_of(&s).0, "off");
        s.present = false;
        assert_eq!(state_of(&s).0, "vacant");
        assert_eq!(temp_of(&s), None);
    }

    #[test]
    fn m2_keys_are_stable() {
        let m2 = vec![
            M2Slot { number: Some(2), present: true, device: Some("nvme1n1".into()), celsius: Some(40.0) },
            M2Slot { number: None, present: false, device: None, celsius: None },
        ];
        let s = m2_slots(&m2, &[]);
        assert_eq!(s.iter().map(|s| s.key).collect::<Vec<_>>(), vec![2, 1001]);
        assert!(s[0].m2 && s[0].present && !s[1].present);
    }
}
