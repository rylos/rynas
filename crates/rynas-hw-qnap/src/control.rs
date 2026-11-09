//! LEDs, buttons and the alarm. Everything here works on a [`Confirmed`] machine: the only way
//! to build one is [`confirm`], which needs the model table's signature and the chip named by
//! its own driver. The worker polls the buttons (10 Hz: gpio-f7188x has no interrupts) and
//! software-blinks the LEDs; a line is only ever touched if it is in the direction the model
//! says (output for an LED, input for a button).

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rynas_api_types::storage::HealthVerdict;

use crate::api::{BayLed, BeepKind, ButtonAction, ButtonEvent, Feature, QnapSettings, QnapStatus, StatusLed, UsbLed};
use crate::beep::{self, Speaker};
use crate::disks::BayState;
use crate::gpio::{Direction, Gpio};
use crate::model::{self, Chip, Line, Model};

/// Button poll and the vendor's blink step (the status and USB LEDs toggle every 125 ms).
const POLL: Duration = Duration::from_millis(125);
/// Ticks per slow toggle (the vendor's alternate green/red: every 500 ms).
const SLOW_TICKS: u64 = 4;
/// Button polls a level must hold to count (debounce).
const STABLE: u8 = 2;
/// A sounding alarm repeats after this long.
const ALARM_REPEAT: Duration = Duration::from_secs(300);
const MAX_EVENTS: usize = 20;
/// The GPIO banks' chip label.
const CHIP_PREFIX: &str = "gpio-f7188x-";

fn chip_label(l: Line) -> String {
    format!("{CHIP_PREFIX}{}", l.bank)
}

/// Proof that this machine is the model and its chip was confirmed; only [`confirm`] makes one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Confirmed {
    model: &'static Model,
}

impl Confirmed {
    pub fn model(&self) -> &'static Model {
        self.model
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmError {
    /// The hwmon driver is not loaded, so the chip is unknown.
    ChipUnknown,
    /// The driver names another chip.
    ChipMismatch,
    /// gpio-f7188x is loaded but its banks have another layout.
    BanksMismatch,
}

/// The chip check on an identified model. `gpio`: `Ok` with no bank at all means the GPIO
/// driver is not loaded (the machine is still confirmed, the lines are unavailable).
pub fn confirm(root: &Path, m: &'static Model, gpio: &mut dyn Gpio) -> Result<Confirmed, ConfirmError> {
    match model::chip(root, m) {
        Chip::Confirmed => {}
        Chip::Unknown => return Err(ConfirmError::ChipUnknown),
        Chip::Mismatch => return Err(ConfirmError::ChipMismatch),
    }
    let banks = banks(gpio);
    if !banks.is_empty() && banks != m.gpio_banks {
        return Err(ConfirmError::BanksMismatch);
    }
    Ok(Confirmed { model: m })
}

/// The lines of each `gpio-f7188x-<n>` chip, by `n`; empty when none exists (or unreadable).
fn banks(gpio: &mut dyn Gpio) -> Vec<u32> {
    let Ok(chips) = gpio.chips() else { return Vec::new() };
    let mut banks: Vec<(u32, u32)> = chips
        .iter()
        .filter_map(|c| Some((c.label.strip_prefix(CHIP_PREFIX)?.parse().ok()?, c.lines)))
        .collect();
    banks.sort();
    banks.into_iter().map(|(_, n)| n).collect()
}

/// A logical state as the electrical level of the line.
fn level(l: Line, on: bool) -> bool {
    on != l.active_low
}

/// What the LEDs should show.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Outputs {
    pub status_green: bool,
    pub status_red: bool,
    pub bays: Vec<bool>,
    pub usb: bool,
    /// The word for the status document.
    pub status_word: &'static str,
    pub any_blink: bool,
}

/// Disk state of a bay as its LED: `(on, blinks)`. Failed: steady, warning: blinks.
pub fn bay_led(state: &BayState) -> (bool, bool) {
    match state.health {
        HealthVerdict::Failing => (true, false),
        HealthVerdict::Warning => (true, true),
        HealthVerdict::Healthy | HealthVerdict::Unknown => (false, false),
    }
}

/// The LEDs for these settings and disks at the blink phases (`fast`: 125 ms, `slow`: 500 ms).
pub fn outputs(s: &QnapSettings, bays: &[BayState], fast: bool, slow: bool) -> Outputs {
    let failing = bays.iter().any(|b| b.health == HealthVerdict::Failing);
    let warning = bays.iter().any(|b| b.health == HealthVerdict::Warning);
    let mode = match s.status_led {
        StatusLed::Auto if failing => StatusLed::Red,
        StatusLed::Auto if warning => StatusLed::RedBlink,
        StatusLed::Auto => StatusLed::Green,
        m => m,
    };
    let (status_green, status_red, status_word, status_blinks) = match mode {
        StatusLed::Auto | StatusLed::Green => (true, false, "green", false),
        StatusLed::GreenBlink => (fast, false, "green-blink", true),
        StatusLed::Red => (false, true, "red", false),
        StatusLed::RedBlink => (false, fast, "red-blink", true),
        StatusLed::Alternate => (slow, !slow, "alternate", true),
        StatusLed::Off => (false, false, "off", false),
    };
    let mut any_blink = status_blinks;
    let disks = bays
        .iter()
        .map(|b| {
            let (on, blinks) = if s.disk_leds { bay_led(b) } else { (false, false) };
            any_blink |= on && blinks;
            on && (!blinks || slow)
        })
        .collect();
    let usb = match s.usb_led {
        UsbLed::Off => false,
        UsbLed::On => true,
        UsbLed::Blink => {
            any_blink = true;
            fast
        }
    };
    Outputs { status_green, status_red, bays: disks, usb, status_word, any_blink }
}

#[derive(Debug, Default)]
struct Button {
    /// The pressed level seen for [`STABLE`] polls, once known.
    pressed: Option<bool>,
    seen: Option<bool>,
    count: u8,
    since: Option<Instant>,
}

/// A press has been released: how long it was held. The state of the button decides: a button
/// found held at start is ignored until it is released.
fn poll_button(b: &mut Button, raw_pressed: bool, now: Instant) -> Option<u32> {
    if b.seen == Some(raw_pressed) {
        b.count = b.count.saturating_add(1);
    } else {
        b.seen = Some(raw_pressed);
        b.count = 1;
    }
    if b.count < STABLE {
        return None;
    }
    match b.pressed {
        None => {
            b.pressed = Some(raw_pressed);
            None
        }
        Some(p) if p == raw_pressed => None,
        Some(_) => {
            b.pressed = Some(raw_pressed);
            if raw_pressed {
                b.since = Some(now);
                None
            } else {
                b.since.take().map(|t| now.duration_since(t).as_millis().min(u32::MAX as u128) as u32)
            }
        }
    }
}

struct Inner {
    gpio: Box<dyn Gpio>,
    settings: QnapSettings,
    bays: Vec<BayState>,
    fast: bool,
    slow: bool,
    /// Last written, with the lines that could not be used.
    applied: Option<Outputs>,
    led_lines: Vec<(Line, bool)>,
    reset: Button,
    usb_copy: Button,
    events: VecDeque<ButtonEvent>,
    alarm: bool,
    alarm_at: Option<Instant>,
    gpio_loaded: bool,
    wrong_direction: Vec<Line>,
}

/// One hardware controller per confirmed machine.
pub struct Controller {
    confirmed: Confirmed,
    speaker: Arc<dyn Speaker>,
    notify: Box<dyn Fn(&ButtonEvent) + Send + Sync>,
    inner: Mutex<Inner>,
    tick_no: AtomicU64,
    beeping: Arc<AtomicBool>,
}

impl Controller {
    pub fn new(
        confirmed: Confirmed,
        mut gpio: Box<dyn Gpio>,
        speaker: Arc<dyn Speaker>,
        notify: Box<dyn Fn(&ButtonEvent) + Send + Sync>,
    ) -> Self {
        let m = confirmed.model();
        let gpio_loaded = !banks(gpio.as_mut()).is_empty();
        let bays = m
            .bays
            .iter()
            .map(|b| BayState { bay: b.bay, device: String::new(), health: HealthVerdict::Unknown })
            .collect();
        let mut wrong_direction = Vec::new();
        let mut led_lines = Vec::new();
        if gpio_loaded {
            let mut leds = vec![m.status_green, m.status_red, m.usb_led];
            leds.extend(m.bays.iter().map(|b| b.err_led));
            for l in leds {
                let ok = gpio.direction(&chip_label(l), l.offset) == Ok(Direction::Output);
                led_lines.push((l, ok));
                if !ok {
                    wrong_direction.push(l);
                }
            }
            for l in [m.reset_button, m.usb_copy_button] {
                if gpio.direction(&chip_label(l), l.offset) != Ok(Direction::Input) {
                    wrong_direction.push(l);
                }
            }
        }
        Self {
            confirmed,
            speaker,
            notify,
            inner: Mutex::new(Inner {
                gpio,
                settings: QnapSettings::default(),
                bays,
                fast: false,
                slow: false,
                applied: None,
                led_lines,
                reset: Button::default(),
                usb_copy: Button::default(),
                events: VecDeque::new(),
                alarm: false,
                alarm_at: None,
                gpio_loaded,
                wrong_direction,
            }),
            tick_no: AtomicU64::new(0),
            beeping: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn model(&self) -> &'static Model {
        self.confirmed.model()
    }

    pub fn settings(&self) -> QnapSettings {
        self.inner.lock().unwrap().settings.clone()
    }

    /// New settings (the configuration apply, the start of rynasd).
    pub fn apply(&self, settings: QnapSettings) {
        let mut g = self.inner.lock().unwrap();
        let was_on = g.settings.led_control;
        g.settings = settings;
        // control just turned off: the LEDs go to their state before rynas drove them, unknown;
        // leaving them lit is honest, turning them off would be a new write
        if was_on && !g.settings.led_control {
            g.applied = None;
        }
        self.write_outputs(&mut g);
    }

    /// New disk state (every minute): LEDs, and a new failure starts the alarm.
    pub fn set_bays(&self, bays: Vec<BayState>) {
        let mut g = self.inner.lock().unwrap();
        let newly_failed = bays.iter().any(|b| {
            b.health == HealthVerdict::Failing
                && !g.bays.iter().any(|o| o.bay == b.bay && o.health == HealthVerdict::Failing)
        });
        let any_failing = bays.iter().any(|b| b.health == HealthVerdict::Failing);
        if newly_failed && g.settings.beep_on_disk_failure {
            g.alarm = true;
            g.alarm_at = None;
        }
        if !any_failing {
            g.alarm = false;
        }
        g.bays = bays;
        self.write_outputs(&mut g);
    }

    fn write_outputs(&self, g: &mut Inner) {
        if !g.settings.led_control || !g.gpio_loaded {
            return;
        }
        let want = outputs(&g.settings, &g.bays, g.fast, g.slow);
        let m = self.confirmed.model();
        let prev = g.applied.take();
        let same = |a: &Outputs, f: fn(&Outputs) -> bool| prev.as_ref().map(f) == Some(f(a));
        let mut writes: Vec<(Line, bool)> = Vec::new();
        if !same(&want, |o| o.status_green) {
            writes.push((m.status_green, want.status_green));
        }
        if !same(&want, |o| o.status_red) {
            writes.push((m.status_red, want.status_red));
        }
        if !same(&want, |o| o.usb) {
            writes.push((m.usb_led, want.usb));
        }
        for (i, b) in m.bays.iter().enumerate() {
            let on = want.bays.get(i).copied().unwrap_or(false);
            if prev.as_ref().and_then(|p| p.bays.get(i).copied()) != Some(on) {
                writes.push((b.err_led, on));
            }
        }
        let usable: Vec<Line> = g.led_lines.iter().filter(|(_, ok)| *ok).map(|(l, _)| *l).collect();
        for (l, on) in writes {
            if !usable.contains(&l) {
                continue;
            }
            if let Err(e) = g.gpio.write(&chip_label(l), l.offset, level(l, on)) {
                tracing::warn!("qnap: LED write: {e}");
            }
        }
        g.applied = Some(want);
    }

    /// One poll: buttons, blink, the alarm's repeat. The worker calls it every [`POLL`].
    pub fn tick(&self, now: Instant) {
        let n = self.tick_no.fetch_add(1, Ordering::Relaxed);
        let m = self.confirmed.model();
        let mut events = Vec::new();
        {
            let mut g = self.inner.lock().unwrap();
            if g.gpio_loaded {
                let reset_action = g.settings.reset_button;
                let usb_action = g.settings.usb_copy_button;
                if let Some(ev) = self.poll(&mut g, m.reset_button, true, now, "reset", reset_action) {
                    events.push(ev);
                }
                if let Some(ev) = self.poll(&mut g, m.usb_copy_button, false, now, "usb_copy", usb_action) {
                    events.push(ev);
                }
                g.fast = !g.fast;
                if n.is_multiple_of(SLOW_TICKS) {
                    g.slow = !g.slow;
                }
                if g.applied.as_ref().is_some_and(|o| o.any_blink) {
                    self.write_outputs(&mut g);
                }
            }
            if g.alarm {
                let due = g.alarm_at.is_none_or(|t| now.duration_since(t) >= ALARM_REPEAT);
                if due {
                    g.alarm_at = Some(now);
                    drop(g);
                    // the vendor's broken-drive alert (mode 0xE); disk failure itself has no
                    // pattern in the vendor code
                    self.beep_async(BeepKind::RapidThree);
                }
            }
        }
        for e in events {
            (self.notify)(&e);
        }
    }

    fn poll(
        &self,
        g: &mut Inner,
        line: Line,
        reset: bool,
        now: Instant,
        name: &str,
        action: ButtonAction,
    ) -> Option<ButtonEvent> {
        if g.wrong_direction.contains(&line) {
            return None;
        }
        let high = match g.gpio.read(&chip_label(line), line.offset) {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("qnap: button read: {e}");
                return None;
            }
        };
        let pressed = high != line.active_low;
        let b = if reset { &mut g.reset } else { &mut g.usb_copy };
        let held_ms = poll_button(b, pressed, now)?;
        if action == ButtonAction::Ignore {
            return None;
        }
        let event = ButtonEvent {
            button: name.into(),
            at: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
            held_ms,
        };
        g.events.push_back(event.clone());
        while g.events.len() > MAX_EVENTS {
            g.events.pop_front();
        }
        (action == ButtonAction::Notify).then_some(event)
    }

    /// One pattern, in the background; a second one while it plays is dropped.
    pub fn beep_async(&self, kind: BeepKind) {
        if !self.speaker.available() || self.beeping.swap(true, Ordering::AcqRel) {
            return;
        }
        let (speaker, hz, busy) = (self.speaker.clone(), self.model().beep_hz, self.beeping.clone());
        let spawned = std::thread::Builder::new().name("qnap-beep".into()).stack_size(64 << 10).spawn(move || {
            play(speaker.as_ref(), kind, hz);
            busy.store(false, Ordering::Release);
        });
        if let Err(e) = spawned {
            tracing::warn!("qnap: beep thread: {e}");
            self.beeping.store(false, Ordering::Release);
        }
    }

    /// Stops a sounding alarm; whether one was sounding.
    pub fn mute(&self) -> bool {
        let mut g = self.inner.lock().unwrap();
        std::mem::take(&mut g.alarm)
    }

    pub fn beeper_available(&self) -> bool {
        self.speaker.available()
    }

    pub fn status(&self) -> QnapStatus {
        let g = self.inner.lock().unwrap();
        let m = self.model();
        let led_feature = |id: &str, lines: &[Line]| {
            let reason = if !g.gpio_loaded {
                "driver_missing"
            } else if lines.iter().any(|l| g.wrong_direction.contains(l)) {
                "wrong_direction"
            } else if !g.settings.led_control {
                "led_control_off"
            } else {
                ""
            };
            feature(id, reason)
        };
        let button_feature = |id: &str, l: Line| {
            let reason = if !g.gpio_loaded {
                "driver_missing"
            } else if g.wrong_direction.contains(&l) {
                "wrong_direction"
            } else {
                ""
            };
            feature(id, reason)
        };
        let bay_lines: Vec<Line> = m.bays.iter().map(|b| b.err_led).collect();
        let mut features = vec![
            led_feature("status_led", &[m.status_green, m.status_red]),
            led_feature("disk_leds", &bay_lines),
            led_feature("usb_led", &[m.usb_led]),
            button_feature("reset_button", m.reset_button),
            button_feature("usb_copy_button", m.usb_copy_button),
        ];
        features.push(feature("buzzer", if self.speaker.available() { "" } else { "no_speaker" }));
        let want = outputs(&g.settings, &g.bays, g.fast, g.slow);
        let driven = g.settings.led_control && g.gpio_loaded;
        let bays = g
            .bays
            .iter()
            .map(|b| {
                let (on, blinks) = if g.settings.disk_leds { bay_led(b) } else { (false, false) };
                BayLed {
                    bay: b.bay,
                    device: b.device.clone(),
                    led: if !on {
                        "off"
                    } else if blinks {
                        "blink"
                    } else {
                        "on"
                    }
                    .into(),
                }
            })
            .collect();
        QnapStatus {
            model: m.display.into(),
            verified: m.verified,
            features,
            status_led: if driven { want.status_word.into() } else { String::new() },
            bays,
            usb_led: if driven { usb_word(g.settings.usb_led).into() } else { String::new() },
            events: g.events.iter().cloned().collect(),
            alarm: g.alarm,
        }
    }
}

fn usb_word(u: UsbLed) -> &'static str {
    match u {
        UsbLed::Off => "off",
        UsbLed::On => "on",
        UsbLed::Blink => "blink",
    }
}

fn feature(id: &str, reason: &str) -> Feature {
    Feature { id: id.into(), available: reason.is_empty(), reason: reason.into() }
}

fn play(speaker: &dyn Speaker, kind: BeepKind, hz: u32) {
    for (on, off) in beep::pattern(kind) {
        if let Err(e) = speaker.tone(hz, *on) {
            tracing::warn!("qnap: beep: {e}");
            return;
        }
        if *off > 0 {
            std::thread::sleep(Duration::from_millis((*off).into()));
        }
    }
}

/// The worker: polls until the process ends.
pub fn spawn(ctl: Arc<Controller>) {
    let run = move || {
        loop {
            ctl.tick(Instant::now());
            std::thread::sleep(POLL);
        }
    };
    if let Err(e) = std::thread::Builder::new().name("qnap-hw".into()).stack_size(128 << 10).spawn(run) {
        tracing::error!("qnap: worker thread: {e}");
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::gpio::{ChipInfo, MemoryGpio};
    use crate::model::{MODELS, testing::fake_hs251};
    use std::sync::Mutex as M;

    pub struct Rec(pub M<Vec<(u32, u32)>>);
    impl Speaker for Rec {
        fn tone(&self, hz: u32, ms: u32) -> Result<(), String> {
            self.0.lock().unwrap().push((hz, ms));
            Ok(())
        }
    }

    fn fake_gpio() -> MemoryGpio {
        let chips = MODELS[0]
            .gpio_banks
            .iter()
            .enumerate()
            .map(|(i, n)| ChipInfo { label: format!("gpio-f7188x-{i}"), lines: *n })
            .collect();
        let g = MemoryGpio::new(chips);
        let m = &MODELS[0];
        for l in [m.status_green, m.status_red, m.usb_led, m.bays[0].err_led, m.bays[1].err_led] {
            g.set_direction(&chip_label(l), l.offset, Direction::Output);
            g.set_level(&chip_label(l), l.offset, true);
        }
        g
    }

    fn ctl(gpio: &MemoryGpio) -> (Controller, Arc<Rec>, Arc<M<Vec<ButtonEvent>>>) {
        let d = tempfile::tempdir().unwrap();
        fake_hs251(d.path(), Some("f71869a"));
        let mut g = gpio.clone();
        let c = confirm(d.path(), &MODELS[0], &mut g).unwrap();
        let rec = Arc::new(Rec(M::new(Vec::new())));
        let notified = Arc::new(M::new(Vec::new()));
        let n = notified.clone();
        let ctl = Controller::new(
            c,
            Box::new(gpio.clone()),
            rec.clone(),
            Box::new(move |e| n.lock().unwrap().push(e.clone())),
        );
        (ctl, rec, notified)
    }

    fn on(s: &mut QnapSettings) {
        s.led_control = true;
        s.status_led = StatusLed::Green;
    }

    fn bay(n: u32, health: HealthVerdict) -> BayState {
        BayState { bay: n, device: format!("sd{n}"), health }
    }

    #[test]
    fn wrong_chip_or_layout_never_confirms() {
        let d = tempfile::tempdir().unwrap();
        fake_hs251(d.path(), Some("f71869"));
        let mut g = fake_gpio();
        assert_eq!(confirm(d.path(), &MODELS[0], &mut g), Err(ConfirmError::ChipMismatch));
        let d = tempfile::tempdir().unwrap();
        fake_hs251(d.path(), None);
        assert_eq!(confirm(d.path(), &MODELS[0], &mut g), Err(ConfirmError::ChipUnknown));
        let d = tempfile::tempdir().unwrap();
        fake_hs251(d.path(), Some("f71869a"));
        let mut other = MemoryGpio::new(vec![ChipInfo { label: "gpio-f7188x-0".into(), lines: 8 }]);
        assert_eq!(confirm(d.path(), &MODELS[0], &mut other), Err(ConfirmError::BanksMismatch));
        assert!(g.writes().is_empty() && other.writes().is_empty());
    }

    #[test]
    fn nothing_written_until_led_control_is_on() {
        let gpio = fake_gpio();
        let (c, ..) = ctl(&gpio);
        c.apply(QnapSettings::default());
        c.set_bays(vec![bay(1, HealthVerdict::Failing), bay(2, HealthVerdict::Healthy)]);
        c.tick(Instant::now());
        assert!(gpio.writes().is_empty(), "led_control is off by default");
        assert_eq!(c.status().status_led, "");
        assert!(c.status().features.iter().any(|f| f.id == "status_led" && f.reason == "led_control_off"));
    }

    #[test]
    fn active_low_status_led() {
        let gpio = fake_gpio();
        let (c, ..) = ctl(&gpio);
        let mut s = QnapSettings::default();
        on(&mut s);
        c.apply(s);
        // green is lit: its line is low, the red line high
        assert_eq!(gpio.level("gpio-f7188x-6", 2), Some(false));
        assert_eq!(gpio.level("gpio-f7188x-6", 3), Some(true));
        assert_eq!(c.status().status_led, "green");
        let writes = gpio.writes().len();
        c.apply({ let mut s = QnapSettings::default(); on(&mut s); s });
        assert_eq!(gpio.writes().len(), writes, "unchanged: no new write");
    }

    #[test]
    fn disk_state_drives_the_bay_leds_and_status() {
        let gpio = fake_gpio();
        let (c, ..) = ctl(&gpio);
        c.apply(QnapSettings { led_control: true, ..Default::default() });
        c.set_bays(vec![bay(1, HealthVerdict::Failing), bay(2, HealthVerdict::Healthy)]);
        assert_eq!(gpio.level("gpio-f7188x-7", 0), Some(false), "bay 1 error LED lit (active low)");
        assert_eq!(gpio.level("gpio-f7188x-7", 1), Some(true), "bay 2 dark");
        // Auto: a failed disk turns the status LED red
        assert_eq!(c.status().status_led, "red");
        assert_eq!(gpio.level("gpio-f7188x-6", 3), Some(false));
        assert_eq!(c.status().bays[0].led, "on");
        // a warning blinks
        let o = outputs(&QnapSettings::default(), &[bay(1, HealthVerdict::Warning)], false, true);
        assert!(o.any_blink && o.bays[0] && o.status_word == "red-blink");
        assert!(!outputs(&QnapSettings::default(), &[bay(1, HealthVerdict::Warning)], true, false).bays[0]);
        // the status LED blinks on the fast phase, the alternate mode on the slow one
        let s = QnapSettings { status_led: StatusLed::Alternate, ..Default::default() };
        assert!(outputs(&s, &[], false, true).status_green && outputs(&s, &[], true, false).status_red);
        let u = QnapSettings { usb_led: UsbLed::Blink, ..Default::default() };
        assert!(outputs(&u, &[], true, false).usb && !outputs(&u, &[], false, true).usb);
        c.set_bays(vec![bay(1, HealthVerdict::Healthy), bay(2, HealthVerdict::Healthy)]);
        assert_eq!(gpio.level("gpio-f7188x-7", 0), Some(true), "fixed: LED off again");
    }

    #[test]
    fn a_line_in_the_wrong_direction_is_never_driven() {
        let gpio = fake_gpio();
        gpio.set_direction("gpio-f7188x-7", 0, Direction::Input);
        let (c, ..) = ctl(&gpio);
        c.apply(QnapSettings { led_control: true, ..Default::default() });
        c.set_bays(vec![bay(1, HealthVerdict::Failing), bay(2, HealthVerdict::Failing)]);
        assert!(gpio.writes().iter().all(|(ch, l, _)| !(ch == "gpio-f7188x-7" && *l == 0)));
        assert!(gpio.writes().iter().any(|(ch, l, _)| ch == "gpio-f7188x-7" && *l == 1));
        assert!(c.status().features.iter().any(|f| f.id == "disk_leds" && f.reason == "wrong_direction"));
    }

    #[test]
    fn button_press_and_debounce() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut b = Button::default();
        // released at start (unpressed), stable, then a press held 300 ms
        for ms in [0, 100] {
            assert_eq!(poll_button(&mut b, false, at(ms)), None);
        }
        assert_eq!(poll_button(&mut b, true, at(200)), None);
        assert_eq!(poll_button(&mut b, true, at(300)), None);
        assert_eq!(poll_button(&mut b, false, at(400)), None);
        assert_eq!(poll_button(&mut b, false, at(500)), Some(200));
        // a one-poll glitch is no press
        let mut b = Button::default();
        for ms in [0, 100] {
            poll_button(&mut b, false, at(ms));
        }
        assert_eq!(poll_button(&mut b, true, at(200)), None);
        assert_eq!(poll_button(&mut b, false, at(300)), None);
        assert_eq!(poll_button(&mut b, false, at(400)), None);
        // held at start: ignored until released
        let mut b = Button::default();
        for ms in [0, 100, 200] {
            assert_eq!(poll_button(&mut b, true, at(ms)), None);
        }
        for ms in [300, 400] {
            assert_eq!(poll_button(&mut b, false, at(ms)), None);
        }
    }

    #[test]
    fn button_events_read_active_low_and_notify_only_when_asked() {
        let gpio = fake_gpio();
        let m = &MODELS[0];
        let label = chip_label(m.reset_button);
        gpio.set_direction(&label, m.reset_button.offset, Direction::Input);
        gpio.set_direction(&chip_label(m.usb_copy_button), m.usb_copy_button.offset, Direction::Input);
        let (c, _, notified) = ctl(&gpio);
        c.apply(QnapSettings::default());
        let t = Instant::now();
        // released (high) until the state is known, then pressed (low), released
        for (i, high) in [true, true, false, false, true, true].iter().enumerate() {
            gpio.set_level(&label, m.reset_button.offset, *high);
            c.tick(t + Duration::from_millis(100 * i as u64));
        }
        assert_eq!(notified.lock().unwrap().len(), 1);
        assert_eq!(c.status().events[0].button, "reset");
        // ignore: not recorded
        let (c, _, notified) = ctl(&gpio);
        c.apply(QnapSettings { reset_button: ButtonAction::Ignore, ..Default::default() });
        for (i, high) in [true, true, false, false, true, true].iter().enumerate() {
            gpio.set_level(&label, m.reset_button.offset, *high);
            c.tick(t + Duration::from_millis(100 * i as u64));
        }
        assert!(notified.lock().unwrap().is_empty() && c.status().events.is_empty());
        // a button press never writes a line
        assert!(gpio.writes().is_empty());
    }

    #[test]
    fn alarm_starts_on_failure_and_mutes() {
        let gpio = fake_gpio();
        let (c, rec, _) = ctl(&gpio);
        c.apply(QnapSettings::default());
        let t = Instant::now();
        c.set_bays(vec![bay(1, HealthVerdict::Healthy), bay(2, HealthVerdict::Healthy)]);
        assert!(!c.status().alarm);
        c.set_bays(vec![bay(1, HealthVerdict::Failing), bay(2, HealthVerdict::Healthy)]);
        assert!(c.status().alarm);
        c.tick(t);
        for _ in 0..100 {
            if !rec.0.lock().unwrap().is_empty() && !c.beeping.load(Ordering::Acquire) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // the vendor's broken-drive pattern, the tone from the vendor's divisor
        assert_eq!(rec.0.lock().unwrap().first(), Some(&(800, 500)));
        assert!(c.mute());
        assert!(!c.status().alarm && !c.mute());
        // setting off: no alarm
        let gpio = fake_gpio();
        let (c, ..) = ctl(&gpio);
        c.apply(QnapSettings { beep_on_disk_failure: false, ..Default::default() });
        c.set_bays(vec![bay(1, HealthVerdict::Failing)]);
        assert!(!c.status().alarm);
    }

    #[test]
    fn honest_status_without_the_gpio_driver() {
        let d = tempfile::tempdir().unwrap();
        fake_hs251(d.path(), Some("f71869a"));
        let mut empty = MemoryGpio::new(Vec::new());
        let c = confirm(d.path(), &MODELS[0], &mut empty).unwrap();
        let ctl = Controller::new(c, Box::new(empty.clone()), Arc::new(Rec(M::new(Vec::new()))), Box::new(|_| {}));
        ctl.apply(QnapSettings { led_control: true, ..Default::default() });
        ctl.tick(Instant::now());
        assert!(empty.writes().is_empty());
        let s = ctl.status();
        assert!(!s.verified && s.features.iter().any(|f| f.id == "status_led" && f.reason == "driver_missing"));
    }
}
