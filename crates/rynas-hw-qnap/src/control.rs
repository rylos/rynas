//! LEDs, buttons and the alarm. Everything here works on a [`Confirmed`] machine: the only way
//! to build one is [`confirm`], which needs the model table's signature and the chip named by
//! its own driver. The worker polls the buttons (8 Hz: gpio-f7188x has no interrupts) and
//! software-blinks the LEDs. A line is only touched if it is in the direction the model says
//! (output for an LED, input for a button), and rynas never changes a direction.
//!
//! LED writes need `led_control` **and**, on a model not verified on hardware, the user's own
//! confirmation of the mapping ([`Controller::confirm_mapping`]).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
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
/// Ticks between attempts at writes that failed.
const RETRY_TICKS: u64 = 8;
/// Button polls a level must hold to count (debounce).
const STABLE: u8 = 2;
/// A sounding alarm repeats after this long.
const ALARM_REPEAT: Duration = Duration::from_secs(300);
/// At most one notification per button in this time: a line that bounces must not flood.
const NOTIFY_MIN: Duration = Duration::from_secs(60);
const MAX_EVENTS: usize = 20;
/// The GPIO banks' chip label.
const CHIP_PREFIX: &str = "gpio-f7188x-";
/// Below rynasd's state directory: the model the user confirmed the mapping of.
const CONFIRM_FILE: &str = "qnap-mapping-confirmed";
/// Below rynasd's state directory: the disks whose alarm was muted (`bay:device` per line).
const MUTE_FILE: &str = "qnap-alarm-muted";

fn chip_label(l: Line) -> String {
    format!("{CHIP_PREFIX}{}", l.bank)
}

/// Proof that this machine is the model and its chip was confirmed; only [`confirm`] makes one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Confirmed {
    model: &'static Model,
    gpio_loaded: bool,
}

impl Confirmed {
    pub fn model(&self) -> &'static Model {
        self.model
    }

    /// `gpio-f7188x` is loaded, with the banks of the model's chip.
    pub fn gpio_loaded(&self) -> bool {
        self.gpio_loaded
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmError {
    /// The hwmon driver is not loaded, so the chip is unknown.
    ChipUnknown,
    /// The driver names another chip, or a fan turns on a fanless model.
    ChipMismatch,
    /// gpio-f7188x is loaded but its banks have another layout.
    BanksMismatch,
    /// The GPIO chips cannot be listed: nothing can be said about them.
    GpioUnreadable,
}

/// The chip check on an identified model. An empty chip list means the GPIO driver is not
/// loaded (the machine is still confirmed, the lines are unavailable); a list that cannot be
/// read confirms nothing.
pub fn confirm(root: &Path, m: &'static Model, gpio: &mut dyn Gpio) -> Result<Confirmed, ConfirmError> {
    match model::chip(root, m) {
        Chip::Confirmed => {}
        Chip::Unknown => return Err(ConfirmError::ChipUnknown),
        Chip::Mismatch => return Err(ConfirmError::ChipMismatch),
    }
    let banks = banks(gpio).map_err(|_| ConfirmError::GpioUnreadable)?;
    if !banks.is_empty() && banks != m.gpio_banks {
        return Err(ConfirmError::BanksMismatch);
    }
    Ok(Confirmed { model: m, gpio_loaded: !banks.is_empty() })
}

/// The lines of each `gpio-f7188x-<n>` chip, by `n`; empty when none exists.
fn banks(gpio: &mut dyn Gpio) -> Result<Vec<u32>, String> {
    let mut banks: Vec<(u32, u32)> = gpio
        .chips()?
        .iter()
        .filter_map(|c| Some((c.label.strip_prefix(CHIP_PREFIX)?.parse().ok()?, c.lines)))
        .collect();
    banks.sort();
    Ok(banks.into_iter().map(|(_, n)| n).collect())
}

/// A logical state as the electrical level of the line.
pub(crate) fn level(l: Line, on: bool) -> bool {
    on != l.active_low
}

/// The GPIO chip of a line.
pub(crate) fn label_of(l: Line) -> String {
    chip_label(l)
}

/// What the confirmation file holds: the model and this machine's MAC addresses, so that a
/// confirmation does not follow the disks to another machine. `None` when one is unreadable.
fn confirmation_text(root: &Path, m: &Model) -> Option<String> {
    let macs: Option<Vec<String>> = m.nics.iter().map(|n| model::nic_mac(root, n).map(|a| a.to_ascii_lowercase())).collect();
    Some(format!("{}\n{}\n", m.id, macs?.join(" ")))
}

/// The user confirmed the mapping on this machine (always true on a verified model).
pub fn mapping_confirmed_on(root: &Path, state_dir: &Path, m: &Model) -> bool {
    m.verified
        || (confirmation_text(root, m).is_some()
            && std::fs::read_to_string(state_dir.join(CONFIRM_FILE)).ok() == confirmation_text(root, m))
}

/// Stores the confirmation for this machine (the tests of the Super I/O worker).
#[cfg(test)]
pub fn write_confirmation(root: &Path, state_dir: &Path, m: &Model) -> Result<(), String> {
    let text = confirmation_text(root, m).ok_or("the machine's MAC addresses cannot be read")?;
    std::fs::write(state_dir.join(CONFIRM_FILE), text).map_err(|e| format!("{CONFIRM_FILE}: {e}"))
}

/// What the LEDs should show.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Outputs {
    pub status_green: bool,
    pub status_red: bool,
    pub bays: Vec<bool>,
    pub usb: bool,
    /// Raw levels of the four brightness lines.
    pub brightness: [bool; 4],
    /// The word for the status document.
    pub status_word: &'static str,
    pub any_blink: bool,
}

/// The vendor's brightness (`sio_sys_set_led_in_night_mode`): 100 % all four lines high, 50 %
/// lines 1 and 3 high, 0 % all low.
pub fn brightness_levels(percent: u8) -> [bool; 4] {
    match percent {
        0 => [false; 4],
        50 => [false, true, false, true],
        _ => [true; 4],
    }
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
    Outputs { status_green, status_red, bays: disks, usb, brightness: brightness_levels(s.led_brightness), status_word, any_blink }
}

#[derive(Debug, Default)]
struct Button {
    /// The pressed level seen for [`STABLE`] polls, once known.
    pressed: Option<bool>,
    seen: Option<bool>,
    count: u8,
    since: Option<Instant>,
    last_notified: Option<Instant>,
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
    mapping_ok: bool,
    bays: Vec<BayState>,
    fast: bool,
    slow: bool,
    /// Levels the hardware really has, line by line (a failed write leaves it out, so it is
    /// tried again).
    written: HashMap<Line, bool>,
    write_failed: bool,
    any_blink: bool,
    led_lines: Vec<Line>,
    reset: Button,
    usb_copy: Button,
    events: VecDeque<ButtonEvent>,
    alarm: bool,
    alarm_at: Option<Instant>,
    /// `bay:device` of the failures the user muted; survives a restart.
    muted: HashSet<String>,
    wrong_direction: Vec<Line>,
}

impl Inner {
    fn driving(&self, gpio_loaded: bool) -> bool {
        self.settings.led_control && self.mapping_ok && gpio_loaded
    }
}

fn mute_key(b: &BayState) -> String {
    format!("{}:{}", b.bay, b.device)
}

/// One hardware controller per confirmed machine.
pub struct Controller {
    confirmed: Confirmed,
    speaker: Arc<dyn Speaker>,
    notify: Box<dyn Fn(&ButtonEvent) + Send + Sync>,
    state_dir: Option<PathBuf>,
    /// What a confirmation on this machine stores (read once, at start).
    confirmation: Option<String>,
    inner: Mutex<Inner>,
    tick_no: AtomicU64,
    beeping: Arc<AtomicBool>,
    beep_stop: Arc<AtomicBool>,
    /// A Super I/O worker is running: no GPIO access at all.
    paused: AtomicBool,
    /// One worker at a time.
    worker_gate: Mutex<()>,
    /// `flock` held while polling, so that a worker cannot run behind rynasd's back.
    lock_path: Option<PathBuf>,
    lock: Mutex<Option<std::fs::File>>,
}

fn flock(path: &Path) -> Result<std::fs::File, String> {
    use std::os::fd::AsRawFd;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let f = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    // SAFETY: flock on a descriptor this function owns; it blocks while a worker holds it
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(format!("{}: {}", path.display(), std::io::Error::last_os_error()));
    }
    Ok(f)
}

impl Controller {
    /// `root`: the tree the machine was identified in. `state_dir`: where the mapping
    /// confirmation and the muted alarms are kept (`None` in tests that do not need them).
    /// `lock_path`: taken while polling (`None`: nothing else can use the chip). The speaker
    /// is silenced first: a previous process may have died in the middle of a tone.
    pub fn new(
        confirmed: Confirmed,
        root: &Path,
        mut gpio: Box<dyn Gpio>,
        speaker: Arc<dyn Speaker>,
        notify: Box<dyn Fn(&ButtonEvent) + Send + Sync>,
        state_dir: Option<PathBuf>,
        lock_path: Option<PathBuf>,
    ) -> Self {
        speaker.stop();
        let lock = lock_path.as_deref().and_then(|p| match flock(p) {
            Ok(f) => Some(f),
            Err(e) => {
                tracing::warn!("qnap: {e}");
                None
            }
        });
        let m = confirmed.model();
        let bays = m
            .bays
            .iter()
            .map(|b| BayState { bay: b.bay, device: String::new(), health: HealthVerdict::Unknown })
            .collect();
        let mut wrong_direction = Vec::new();
        let mut led_lines = Vec::new();
        if confirmed.gpio_loaded() {
            // only the LEDs the case has: a missing one's line is never written
            let mut leds = vec![m.status_green, m.status_red];
            if m.usb_led_fitted {
                leds.push(m.usb_led);
            }
            leds.extend(m.brightness);
            if m.disk_leds_fitted {
                leds.extend(m.bays.iter().map(|b| b.err_led));
            }
            for l in leds {
                if gpio.direction(&chip_label(l), l.offset) == Ok(Direction::Output) {
                    led_lines.push(l);
                } else {
                    wrong_direction.push(l);
                }
            }
            let buttons: &[Line] = if m.usb_copy_button_fitted { &[m.reset_button, m.usb_copy_button] } else { &[m.reset_button] };
            for &l in buttons {
                if gpio.direction(&chip_label(l), l.offset) != Ok(Direction::Input) {
                    wrong_direction.push(l);
                }
            }
        }
        let read_state = |file: &str| state_dir.as_ref().and_then(|d| std::fs::read_to_string(d.join(file)).ok());
        let mapping_ok = m.verified || state_dir.as_ref().is_some_and(|d| mapping_confirmed_on(root, d, m));
        let muted = read_state(MUTE_FILE)
            .map(|t| t.lines().map(str::to_string).collect())
            .unwrap_or_default();
        Self {
            confirmed,
            speaker,
            notify,
            state_dir,
            confirmation: confirmation_text(root, m),
            inner: Mutex::new(Inner {
                gpio,
                settings: QnapSettings::default(),
                mapping_ok,
                bays,
                fast: false,
                slow: false,
                written: HashMap::new(),
                write_failed: false,
                any_blink: false,
                led_lines,
                reset: Button::default(),
                usb_copy: Button::default(),
                events: VecDeque::new(),
                alarm: false,
                alarm_at: None,
                muted,
                wrong_direction,
            }),
            tick_no: AtomicU64::new(0),
            beeping: Arc::new(AtomicBool::new(false)),
            beep_stop: Arc::new(AtomicBool::new(false)),
            paused: AtomicBool::new(false),
            worker_gate: Mutex::new(()),
            lock_path,
            lock: Mutex::new(lock),
        }
    }

    pub fn model(&self) -> &'static Model {
        self.confirmed.model()
    }

    pub fn settings(&self) -> QnapSettings {
        self.inner.lock().unwrap().settings.clone()
    }

    /// Runs a Super I/O worker: `f` runs with no GPIO access from this process (the chip's
    /// configuration mode is one state) and with the lock free for the worker. Status, mute
    /// and everything else keep working meanwhile: the state mutex is not held.
    pub fn pause_for_worker<T>(&self, f: impl FnOnce() -> T) -> T {
        let _one = self.worker_gate.lock().unwrap();
        self.paused.store(true, Ordering::Release);
        // waits for an access already under way; those after it see the flag
        drop(self.inner.lock().unwrap());
        drop(self.lock.lock().unwrap().take());
        let r = f();
        if let Some(p) = &self.lock_path {
            match flock(p) {
                Ok(l) => *self.lock.lock().unwrap() = Some(l),
                Err(e) => tracing::warn!("qnap: {e}"),
            }
        }
        self.paused.store(false, Ordering::Release);
        let mut g = self.inner.lock().unwrap();
        self.write_outputs(&mut g);
        r
    }

    /// The user's confirmation of the mapping: `typed` must be the model's short name.
    pub fn confirm_mapping(&self, typed: &str) -> Result<(), String> {
        let m = self.model();
        if typed.trim() != m.short {
            return Err(format!("type {} to confirm", m.short));
        }
        if let Some(d) = &self.state_dir {
            let text = self.confirmation.as_ref().ok_or("the machine's MAC addresses cannot be read")?;
            std::fs::write(d.join(CONFIRM_FILE), text).map_err(|e| format!("{CONFIRM_FILE}: {e}"))?;
        }
        let mut g = self.inner.lock().unwrap();
        g.mapping_ok = true;
        self.write_outputs(&mut g);
        Ok(())
    }

    /// Takes the confirmation back: the LED lines are released.
    pub fn revoke_mapping(&self) -> Result<(), String> {
        if let Some(d) = &self.state_dir {
            match std::fs::remove_file(d.join(CONFIRM_FILE)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{CONFIRM_FILE}: {e}")),
            }
        }
        let mut g = self.inner.lock().unwrap();
        g.mapping_ok = self.model().verified;
        self.release_if_stopped(&mut g);
        Ok(())
    }

    pub fn mapping_confirmed(&self) -> bool {
        self.inner.lock().unwrap().mapping_ok
    }

    /// New settings (the configuration apply, the start of rynasd).
    pub fn apply(&self, settings: QnapSettings) {
        let mut g = self.inner.lock().unwrap();
        g.settings = settings;
        self.update_alarm(&mut g);
        self.release_if_stopped(&mut g);
        self.write_outputs(&mut g);
    }

    /// Control went off: the lines go back (their level stays, nothing is written).
    fn release_if_stopped(&self, g: &mut Inner) {
        if g.driving(self.confirmed.gpio_loaded()) || g.written.is_empty() {
            return;
        }
        g.written.clear();
        g.any_blink = false;
        g.write_failed = false;
        for l in g.led_lines.clone() {
            g.gpio.release(&chip_label(l), l.offset);
        }
    }

    /// New disk state from the storage service. A disk with no verdict keeps its last one (a
    /// disk that fell asleep has not recovered).
    pub fn set_bays(&self, mut bays: Vec<BayState>) {
        let mut g = self.inner.lock().unwrap();
        for b in &mut bays {
            if b.health == HealthVerdict::Unknown
                && let Some(o) = g.bays.iter().find(|o| o.bay == b.bay && o.device == b.device)
            {
                b.health = o.health;
            }
        }
        g.bays = bays;
        let before = g.muted.len();
        // a mute goes only on a verdict: the disk recovered or another one is in the bay. No
        // verdict yet (the first check after a restart) says nothing.
        let bays = g.bays.clone();
        g.muted.retain(|k| {
            bays.iter().find(|b| k.starts_with(&format!("{}:", b.bay))).is_none_or(|b| {
                *k == mute_key(b) && matches!(b.health, HealthVerdict::Failing | HealthVerdict::Unknown)
            })
        });
        if g.muted.len() != before {
            self.persist_muted(&g);
        }
        self.update_alarm(&mut g);
        self.write_outputs(&mut g);
    }

    /// The alarm sounds for a failed disk nobody muted, while the setting is on.
    fn update_alarm(&self, g: &mut Inner) {
        let wanted = g.settings.beep_on_disk_failure
            && g.bays.iter().any(|b| b.health == HealthVerdict::Failing && !g.muted.contains(&mute_key(b)));
        if wanted && !g.alarm {
            g.alarm_at = None;
        }
        if !wanted && g.alarm {
            self.beep_stop.store(true, Ordering::Release);
        }
        g.alarm = wanted;
    }

    fn persist_muted(&self, g: &Inner) {
        let Some(d) = &self.state_dir else { return };
        let mut list: Vec<&str> = g.muted.iter().map(String::as_str).collect();
        list.sort_unstable();
        if let Err(e) = std::fs::write(d.join(MUTE_FILE), list.join("\n")) {
            tracing::warn!("qnap: {MUTE_FILE}: {e}");
        }
    }

    fn write_outputs(&self, g: &mut Inner) {
        if !g.driving(self.confirmed.gpio_loaded()) || self.paused.load(Ordering::Acquire) {
            return;
        }
        let want = outputs(&g.settings, &g.bays, g.fast, g.slow);
        let m = self.confirmed.model();
        let mut lines: Vec<(Line, bool)> = vec![
            (m.status_green, level(m.status_green, want.status_green)),
            (m.status_red, level(m.status_red, want.status_red)),
            (m.usb_led, level(m.usb_led, want.usb)),
        ];
        for (l, high) in m.brightness.iter().zip(want.brightness) {
            lines.push((*l, high));
        }
        for (i, b) in m.bays.iter().enumerate() {
            let on = want.bays.get(i).copied().unwrap_or(false);
            lines.push((b.err_led, level(b.err_led, on)));
        }
        g.any_blink = want.any_blink;
        let mut failed = false;
        for (l, high) in lines {
            if !g.led_lines.contains(&l) || g.written.get(&l) == Some(&high) {
                continue;
            }
            match g.gpio.write(&chip_label(l), l.offset, high) {
                Ok(()) => {
                    g.written.insert(l, high);
                }
                Err(e) => {
                    g.written.remove(&l);
                    if !g.write_failed {
                        tracing::warn!("qnap: LED write: {e}");
                    }
                    failed = true;
                }
            }
        }
        g.write_failed = failed;
    }

    /// One poll: buttons, blink, retries, the alarm's repeat. The worker calls it every [`POLL`].
    pub fn tick(&self, now: Instant) {
        let n = self.tick_no.fetch_add(1, Ordering::Relaxed);
        let m = self.confirmed.model();
        let mut events = Vec::new();
        let mut sound = false;
        {
            let mut g = self.inner.lock().unwrap();
            if self.confirmed.gpio_loaded() && !self.paused.load(Ordering::Acquire) {
                let reset_action = g.settings.reset_button;
                let usb_action = g.settings.usb_copy_button;
                if let Some(ev) = self.poll(&mut g, m.reset_button, true, now, "reset", reset_action) {
                    events.push(ev);
                }
                if m.usb_copy_button_fitted
                    && let Some(ev) = self.poll(&mut g, m.usb_copy_button, false, now, "usb_copy", usb_action)
                {
                    events.push(ev);
                }
                g.fast = !g.fast;
                if n.is_multiple_of(SLOW_TICKS) {
                    g.slow = !g.slow;
                }
                if g.any_blink || (g.write_failed && n.is_multiple_of(RETRY_TICKS)) {
                    self.write_outputs(&mut g);
                }
            }
            if g.alarm && g.alarm_at.is_none_or(|t| now.duration_since(t) >= ALARM_REPEAT) {
                g.alarm_at = Some(now);
                sound = true;
            }
        }
        if sound {
            // the vendor's broken-drive alert (mode 0xE); disk failure itself has no pattern in
            // the vendor code
            self.beep_async(BeepKind::RapidThree);
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
        let notify = action == ButtonAction::Notify && b.last_notified.is_none_or(|t| now.duration_since(t) >= NOTIFY_MIN);
        if notify {
            b.last_notified = Some(now);
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
        notify.then_some(event)
    }

    /// One pattern, in the background; a second one while it plays is dropped.
    pub fn beep_async(&self, kind: BeepKind) {
        if !self.speaker.available() || self.beeping.swap(true, Ordering::AcqRel) {
            return;
        }
        self.beep_stop.store(false, Ordering::Release);
        let (speaker, hz, busy, stop) =
            (self.speaker.clone(), self.model().beep_hz, self.beeping.clone(), self.beep_stop.clone());
        let spawned = std::thread::Builder::new().name("qnap-beep".into()).stack_size(64 << 10).spawn(move || {
            play(speaker.as_ref(), kind, hz, &stop);
            busy.store(false, Ordering::Release);
        });
        if let Err(e) = spawned {
            tracing::warn!("qnap: beep thread: {e}");
            self.beeping.store(false, Ordering::Release);
        }
    }

    /// Stops a sounding alarm, also in the middle of a pattern, until the failed disks change;
    /// whether one was sounding.
    pub fn mute(&self) -> bool {
        let mut g = self.inner.lock().unwrap();
        let keys: Vec<String> =
            g.bays.iter().filter(|b| b.health == HealthVerdict::Failing).map(mute_key).collect();
        g.muted.extend(keys);
        self.persist_muted(&g);
        self.beep_stop.store(true, Ordering::Release);
        std::mem::take(&mut g.alarm)
    }

    /// rynasd is stopping: no tone may outlive it.
    pub fn shutdown(&self) {
        self.beep_stop.store(true, Ordering::Release);
        self.speaker.stop();
    }

    pub fn beeper_available(&self) -> bool {
        self.speaker.available()
    }

    pub fn status(&self) -> QnapStatus {
        let g = self.inner.lock().unwrap();
        let m = self.model();
        let loaded = self.confirmed.gpio_loaded();
        let led_feature = |id: &str, lines: &[Line]| {
            let reason = if !loaded {
                "driver_missing"
            } else if lines.iter().any(|l| g.wrong_direction.contains(l)) {
                "wrong_direction"
            } else if !g.mapping_ok {
                "mapping_unconfirmed"
            } else if !g.settings.led_control {
                "led_control_off"
            } else if g.write_failed && lines.iter().any(|l| !g.written.contains_key(l)) {
                "write_failed"
            } else {
                ""
            };
            feature(id, reason)
        };
        let button_feature = |id: &str, l: Line| {
            let reason = if !loaded {
                "driver_missing"
            } else if g.wrong_direction.contains(&l) {
                "wrong_direction"
            } else {
                ""
            };
            feature(id, reason)
        };
        let bay_lines: Vec<Line> = m.bays.iter().map(|b| b.err_led).collect();
        let features = vec![
            led_feature("status_led", &[m.status_green, m.status_red]),
            led_feature("led_brightness", &m.brightness),
            if m.disk_leds_fitted { led_feature("disk_leds", &bay_lines) } else { feature("disk_leds", "not_fitted") },
            if m.usb_led_fitted { led_feature("usb_led", &[m.usb_led]) } else { feature("usb_led", "not_fitted") },
            button_feature("reset_button", m.reset_button),
            if m.usb_copy_button_fitted { button_feature("usb_copy_button", m.usb_copy_button) } else { feature("usb_copy_button", "not_fitted") },
            feature("buzzer", if self.speaker.available() { "" } else { "no_speaker" }),
        ];
        let want = outputs(&g.settings, &g.bays, g.fast, g.slow);
        let driven = g.driving(loaded);
        let bays = g
            .bays
            .iter()
            .map(|b| {
                let (on, blinks) = if g.settings.disk_leds { bay_led(b) } else { (false, false) };
                let line = m.bays.iter().find(|x| x.bay == b.bay).map(|x| x.err_led);
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
                    driven: driven && line.is_some_and(|l| g.written.contains_key(&l)),
                }
            })
            .collect();
        let usable = |l: Line| !g.wrong_direction.contains(&l);
        QnapStatus {
            model: m.display.into(),
            short_model: m.short.into(),
            verified: m.verified,
            mapping_confirmed: g.mapping_ok,
            features,
            status_led: if driven && usable(m.status_green) && usable(m.status_red) {
                want.status_word.into()
            } else {
                String::new()
            },
            bays,
            usb_led: if driven && usable(m.usb_led) { usb_word(g.settings.usb_led).into() } else { String::new() },
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

/// Waits `ms`, in short steps: false when told to stop.
fn wait(ms: u32, stop: &AtomicBool) -> bool {
    const STEP: u32 = 20;
    let mut left = ms;
    while left > 0 {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        let d = left.min(STEP);
        std::thread::sleep(Duration::from_millis(d.into()));
        left -= d;
    }
    !stop.load(Ordering::Acquire)
}

fn play(speaker: &dyn Speaker, kind: BeepKind, hz: u32, stop: &AtomicBool) {
    for (on, off) in beep::pattern(kind) {
        if let Err(e) = speaker.start(hz) {
            tracing::warn!("qnap: beep: {e}");
            return;
        }
        let go_on = wait(*on, stop);
        speaker.stop();
        if !go_on || !wait(*off, stop) {
            return;
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

    /// Records `(hz)` of each start, and 0 for each stop.
    #[derive(Default)]
    pub struct Rec(pub M<Vec<u32>>);
    impl Speaker for Rec {
        fn start(&self, hz: u32) -> Result<(), String> {
            self.0.lock().unwrap().push(hz);
            Ok(())
        }
        fn stop(&self) {
            self.0.lock().unwrap().push(0);
        }
    }

    pub fn fake_gpio() -> MemoryGpio {
        let chips = MODELS[0]
            .gpio_banks
            .iter()
            .enumerate()
            .map(|(i, n)| ChipInfo { label: format!("gpio-f7188x-{i}"), lines: *n })
            .collect();
        let g = MemoryGpio::new(chips);
        let m = &MODELS[0];
        let mut out = vec![m.status_green, m.status_red, m.usb_led, m.bays[0].err_led, m.bays[1].err_led];
        out.extend(m.brightness);
        for l in out {
            g.set_direction(&chip_label(l), l.offset, Direction::Output);
            g.set_level(&chip_label(l), l.offset, true);
        }
        g
    }

    type Notified = Arc<M<Vec<ButtonEvent>>>;

    /// The HS-251 as if its case had every LED the vendor's model file names and the mapping
    /// were still untested, so that the controller's handling of bay and USB LEDs and of the
    /// user's confirmation is tested.
    static ALL_LEDS: Model =
        Model { disk_leds_fitted: true, usb_led_fitted: true, usb_copy_button_fitted: true, verified: false, ..MODELS[0] };

    fn ctl_in(gpio: &MemoryGpio, state: Option<PathBuf>) -> (Controller, Arc<Rec>, Notified) {
        ctl_for(&ALL_LEDS, gpio, state)
    }

    fn ctl_for(m: &'static Model, gpio: &MemoryGpio, state: Option<PathBuf>) -> (Controller, Arc<Rec>, Notified) {
        let d = tempfile::tempdir().unwrap();
        fake_hs251(d.path(), Some("f71869a"));
        let mut g = gpio.clone();
        let c = confirm(d.path(), m, &mut g).unwrap();
        let rec = Arc::new(Rec::default());
        let notified: Notified = Arc::new(M::new(Vec::new()));
        let n = notified.clone();
        let ctl = Controller::new(
            c,
            d.path(),
            Box::new(gpio.clone()),
            rec.clone(),
            Box::new(move |e| n.lock().unwrap().push(e.clone())),
            state,
            None,
        );
        (ctl, rec, notified)
    }

    fn ctl(gpio: &MemoryGpio) -> (Controller, Arc<Rec>, Notified) {
        ctl_in(gpio, None)
    }

    /// LED control on, mapping confirmed.
    fn driving(gpio: &MemoryGpio, s: QnapSettings) -> (Controller, Arc<Rec>, Notified) {
        let r = ctl(gpio);
        r.0.confirm_mapping("HS-251").unwrap();
        r.0.apply(QnapSettings { led_control: true, ..s });
        r
    }

    fn bay(n: u32, health: HealthVerdict) -> BayState {
        BayState { bay: n, device: format!("sd{n}"), health }
    }

    fn wait_idle(c: &Controller) {
        for _ in 0..200 {
            if !c.beeping.load(Ordering::Acquire) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("beep did not end");
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
    fn an_unreadable_chip_list_confirms_nothing() {
        let d = tempfile::tempdir().unwrap();
        fake_hs251(d.path(), Some("f71869a"));
        let mut g = fake_gpio();
        g.set_fail_chips(true);
        assert_eq!(confirm(d.path(), &MODELS[0], &mut g), Err(ConfirmError::GpioUnreadable));
    }

    #[test]
    fn nothing_written_until_control_is_on_and_confirmed() {
        let gpio = fake_gpio();
        let (c, ..) = ctl(&gpio);
        c.apply(QnapSettings::default());
        c.set_bays(vec![bay(1, HealthVerdict::Failing), bay(2, HealthVerdict::Healthy)]);
        c.tick(Instant::now());
        assert!(gpio.writes().is_empty(), "led_control is off by default");
        // the switch alone is not enough on an unverified model
        c.apply(QnapSettings { led_control: true, ..Default::default() });
        c.tick(Instant::now());
        assert!(gpio.writes().is_empty(), "no confirmation of the mapping yet");
        assert_eq!(c.status().status_led, "");
        assert!(c.status().features.iter().any(|f| f.id == "status_led" && f.reason == "mapping_unconfirmed"));
        // the UI must not claim the LED is lit
        assert!(!c.status().bays[0].driven);
        assert!(c.confirm_mapping("HS-253").is_err() && gpio.writes().is_empty());
        c.confirm_mapping("HS-251").unwrap();
        assert!(!gpio.writes().is_empty() && c.status().mapping_confirmed);
        assert!(c.status().bays[0].driven);
    }

    #[test]
    fn the_confirmation_survives_a_restart_and_can_be_taken_back() {
        let st = tempfile::tempdir().unwrap();
        let gpio = fake_gpio();
        let (c, ..) = ctl_in(&gpio, Some(st.path().into()));
        assert!(!c.mapping_confirmed());
        c.confirm_mapping("HS-251").unwrap();
        let (c2, ..) = ctl_in(&gpio, Some(st.path().into()));
        assert!(c2.mapping_confirmed());
        c2.revoke_mapping().unwrap();
        let (c3, ..) = ctl_in(&gpio, Some(st.path().into()));
        assert!(!c3.mapping_confirmed());
    }

    #[test]
    fn turning_control_off_releases_the_lines() {
        let gpio = fake_gpio();
        let (c, ..) = driving(&gpio, QnapSettings::default());
        assert!(!gpio.writes().is_empty() && gpio.released().is_empty());
        let writes = gpio.writes().len();
        c.apply(QnapSettings::default());
        assert!(gpio.released().iter().any(|(ch, l)| ch == "gpio-f7188x-6" && *l == 2));
        assert_eq!(gpio.writes().len(), writes, "off writes nothing");
    }

    #[test]
    fn active_low_status_led() {
        let gpio = fake_gpio();
        let (c, ..) = driving(&gpio, QnapSettings { status_led: StatusLed::Green, ..Default::default() });
        // green is lit: its line is low, the red line high
        assert_eq!(gpio.level("gpio-f7188x-6", 2), Some(false));
        assert_eq!(gpio.level("gpio-f7188x-6", 3), Some(true));
        assert_eq!(c.status().status_led, "green");
        let writes = gpio.writes().len();
        c.apply(QnapSettings { led_control: true, status_led: StatusLed::Green, ..Default::default() });
        assert_eq!(gpio.writes().len(), writes, "unchanged: no new write");
    }

    #[test]
    fn brightness_follows_the_vendor_levels() {
        assert_eq!(brightness_levels(100), [true; 4]);
        assert_eq!(brightness_levels(50), [false, true, false, true]);
        assert_eq!(brightness_levels(0), [false; 4]);
        let gpio = fake_gpio();
        let (c, ..) = driving(&gpio, QnapSettings { led_brightness: 50, ..Default::default() });
        let lv = |l: u32| gpio.level("gpio-f7188x-5", l);
        assert_eq!((lv(0), lv(1), lv(2), lv(3)), (Some(false), Some(true), Some(false), Some(true)));
        c.apply(QnapSettings { led_control: true, led_brightness: 0, ..Default::default() });
        assert_eq!((lv(0), lv(1), lv(2), lv(3)), (Some(false), Some(false), Some(false), Some(false)));
        assert!(c.status().features.iter().any(|f| f.id == "led_brightness" && f.available));
    }

    #[test]
    fn disk_state_drives_the_bay_leds_and_status() {
        let gpio = fake_gpio();
        let (c, ..) = driving(&gpio, QnapSettings::default());
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
        let (c, ..) = driving(&gpio, QnapSettings::default());
        c.set_bays(vec![bay(1, HealthVerdict::Failing), bay(2, HealthVerdict::Failing)]);
        assert!(gpio.writes().iter().all(|(ch, l, _)| !(ch == "gpio-f7188x-7" && *l == 0)));
        assert!(gpio.writes().iter().any(|(ch, l, _)| ch == "gpio-f7188x-7" && *l == 1));
        assert!(c.status().features.iter().any(|f| f.id == "disk_leds" && f.reason == "wrong_direction"));
        assert!(!c.status().bays[0].driven && c.status().bays[1].driven);
    }

    /// The real HS-251 has no bay or USB LED: their lines are never written, and the status
    /// says why.
    #[test]
    fn a_missing_led_is_never_written() {
        let gpio = fake_gpio();
        let (c, ..) = ctl_for(&MODELS[0], &gpio, None);
        c.confirm_mapping("HS-251").unwrap();
        c.apply(QnapSettings { led_control: true, disk_leds: true, usb_led: UsbLed::On, ..Default::default() });
        c.set_bays(vec![bay(1, HealthVerdict::Failing), bay(2, HealthVerdict::Failing)]);
        let m = &MODELS[0];
        let missing = [m.usb_led, m.bays[0].err_led, m.bays[1].err_led];
        assert!(gpio.writes().iter().all(|(ch, l, _)| !missing.iter().any(|x| chip_label(*x) == *ch && x.offset == *l)));
        assert!(gpio.writes().iter().any(|(ch, l, _)| chip_label(m.status_red) == *ch && m.status_red.offset == *l), "the status LED is still driven");
        let f = c.status().features;
        assert!(f.iter().any(|f| f.id == "disk_leds" && f.reason == "not_fitted"));
        assert!(f.iter().any(|f| f.id == "usb_led" && f.reason == "not_fitted"));
    }

    #[test]
    fn a_failed_write_is_tried_again_and_reported() {
        let gpio = fake_gpio();
        gpio.set_fail_write(true);
        let (c, ..) = driving(&gpio, QnapSettings::default());
        assert!(c.status().features.iter().any(|f| f.id == "status_led" && f.reason == "write_failed"));
        assert!(!c.status().bays[0].driven);
        gpio.set_fail_write(false);
        let t = Instant::now();
        for i in 0..RETRY_TICKS + 1 {
            c.tick(t + Duration::from_millis(125 * i));
        }
        assert_eq!(gpio.level("gpio-f7188x-6", 2), Some(false));
        assert!(c.status().features.iter().any(|f| f.id == "status_led" && f.available));
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

    fn press(gpio: &MemoryGpio, c: &Controller, label: &str, line: u32, t: Instant) {
        for (i, high) in [true, true, false, false, true, true].iter().enumerate() {
            gpio.set_level(label, line, *high);
            c.tick(t + Duration::from_millis(125 * i as u64));
        }
    }

    #[test]
    fn button_events_read_active_low_and_notify_only_when_asked() {
        let gpio = fake_gpio();
        let m = &MODELS[0];
        let label = chip_label(m.reset_button);
        gpio.set_direction(&label, m.reset_button.offset, Direction::Input);
        gpio.set_direction(&chip_label(m.usb_copy_button), m.usb_copy_button.offset, Direction::Input);
        // listed by default, no notification
        let (c, _, notified) = ctl(&gpio);
        c.apply(QnapSettings::default());
        let t = Instant::now();
        press(&gpio, &c, &label, m.reset_button.offset, t);
        assert!(notified.lock().unwrap().is_empty());
        assert_eq!(c.status().events[0].button, "reset");
        // ignore: not recorded
        let (c, _, notified) = ctl(&gpio);
        c.apply(QnapSettings { reset_button: ButtonAction::Ignore, ..Default::default() });
        press(&gpio, &c, &label, m.reset_button.offset, t);
        assert!(notified.lock().unwrap().is_empty() && c.status().events.is_empty());
        // a button press never writes a line
        assert!(gpio.writes().is_empty());
    }

    #[test]
    fn a_bouncing_button_notifies_once_a_minute() {
        let gpio = fake_gpio();
        let m = &MODELS[0];
        let label = chip_label(m.reset_button);
        gpio.set_direction(&label, m.reset_button.offset, Direction::Input);
        gpio.set_direction(&chip_label(m.usb_copy_button), m.usb_copy_button.offset, Direction::Input);
        let (c, _, notified) = ctl(&gpio);
        c.apply(QnapSettings { reset_button: ButtonAction::Notify, ..Default::default() });
        let t = Instant::now();
        for n in 0..5u64 {
            press(&gpio, &c, &label, m.reset_button.offset, t + Duration::from_secs(n));
        }
        assert_eq!(notified.lock().unwrap().len(), 1);
        assert_eq!(c.status().events.len(), 5, "all of them are still listed");
        press(&gpio, &c, &label, m.reset_button.offset, t + NOTIFY_MIN + Duration::from_secs(10));
        assert_eq!(notified.lock().unwrap().len(), 2);
    }

    #[test]
    fn alarm_starts_on_failure_and_mute_interrupts_it() {
        let gpio = fake_gpio();
        let (c, rec, _) = ctl(&gpio);
        c.apply(QnapSettings::default());
        let t = Instant::now();
        c.set_bays(vec![bay(1, HealthVerdict::Healthy), bay(2, HealthVerdict::Healthy)]);
        assert!(!c.status().alarm);
        c.set_bays(vec![bay(1, HealthVerdict::Failing), bay(2, HealthVerdict::Healthy)]);
        assert!(c.status().alarm);
        rec.0.lock().unwrap().clear();
        c.tick(t);
        for _ in 0..200 {
            if !rec.0.lock().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // the tone from the vendor's divisor
        assert_eq!(rec.0.lock().unwrap().first(), Some(&800));
        assert!(c.mute());
        wait_idle(&c);
        // stopped well before the three tones of the pattern: one start, one silence
        assert_eq!(*rec.0.lock().unwrap(), vec![800, 0]);
        assert!(!c.status().alarm && !c.mute());
    }

    #[test]
    fn mute_survives_a_restart_until_the_failure_changes() {
        let st = tempfile::tempdir().unwrap();
        let gpio = fake_gpio();
        let (c, ..) = ctl_in(&gpio, Some(st.path().into()));
        c.apply(QnapSettings::default());
        c.set_bays(vec![bay(1, HealthVerdict::Failing)]);
        assert!(c.status().alarm);
        c.mute();
        let (c, ..) = ctl_in(&gpio, Some(st.path().into()));
        c.apply(QnapSettings::default());
        c.set_bays(vec![bay(1, HealthVerdict::Failing)]);
        assert!(!c.status().alarm, "still muted after a restart");
        // another disk failing is a new alarm
        c.set_bays(vec![bay(1, HealthVerdict::Failing), bay(2, HealthVerdict::Failing)]);
        assert!(c.status().alarm);
        // bay 1 recovered: its mute is forgotten, a new failure sounds again
        c.mute();
        c.set_bays(vec![bay(1, HealthVerdict::Healthy), bay(2, HealthVerdict::Failing)]);
        assert!(!c.status().alarm);
        c.set_bays(vec![bay(1, HealthVerdict::Failing), bay(2, HealthVerdict::Failing)]);
        assert!(c.status().alarm);
    }

    #[test]
    fn a_mute_outlives_a_first_check_with_no_verdict() {
        let st = tempfile::tempdir().unwrap();
        let gpio = fake_gpio();
        let (c, ..) = ctl_in(&gpio, Some(st.path().into()));
        c.apply(QnapSettings::default());
        c.set_bays(vec![bay(1, HealthVerdict::Failing)]);
        c.mute();
        // restart, and the first storage check has no verdict for the disk yet
        let (c, ..) = ctl_in(&gpio, Some(st.path().into()));
        c.apply(QnapSettings::default());
        c.set_bays(vec![bay(1, HealthVerdict::Unknown)]);
        c.set_bays(vec![bay(1, HealthVerdict::Failing)]);
        assert!(!c.status().alarm, "the verdict came later: still muted");
        // another disk in the bay is a new story
        c.set_bays(vec![BayState { bay: 1, device: "sdz".into(), health: HealthVerdict::Failing }]);
        assert!(c.status().alarm);
    }

    #[test]
    fn the_confirmation_is_bound_to_the_machine() {
        let st = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        fake_hs251(d.path(), Some("f71869a"));
        // a model still untested: a verified one needs no confirmation
        let m = &ALL_LEDS;
        assert!(!mapping_confirmed_on(d.path(), st.path(), m));
        write_confirmation(d.path(), st.path(), m).unwrap();
        assert!(mapping_confirmed_on(d.path(), st.path(), m));
        // the disks moved to another machine of the same model
        std::fs::write(d.path().join("sys/class/net/eth0/address"), "00:08:9b:12:34:56\n").unwrap();
        assert!(!mapping_confirmed_on(d.path(), st.path(), m));
    }

    #[test]
    fn a_worker_runs_with_no_gpio_access_and_status_keeps_working() {
        let gpio = fake_gpio();
        let m = &MODELS[0];
        gpio.set_direction(&chip_label(m.reset_button), m.reset_button.offset, Direction::Input);
        gpio.set_direction(&chip_label(m.usb_copy_button), m.usb_copy_button.offset, Direction::Input);
        let (c, ..) = driving(&gpio, QnapSettings::default());
        let writes = gpio.writes().len();
        let after = c.pause_for_worker(|| {
            // during the worker: a status and a settings change, neither touches a line
            assert!(c.status().mapping_confirmed);
            c.apply(QnapSettings { led_control: true, status_led: StatusLed::Red, ..Default::default() });
            c.tick(Instant::now());
            gpio.writes().len()
        });
        assert_eq!(after, writes, "nothing written while the worker runs");
        assert_eq!(c.status().status_led, "red");
        assert_eq!(gpio.level("gpio-f7188x-6", 3), Some(false), "the pending change goes out afterwards");
    }

    #[test]
    fn no_verdict_is_not_a_recovery() {
        let gpio = fake_gpio();
        let (c, ..) = ctl(&gpio);
        c.apply(QnapSettings::default());
        c.set_bays(vec![bay(1, HealthVerdict::Failing)]);
        assert!(c.status().alarm);
        c.set_bays(vec![bay(1, HealthVerdict::Unknown)]);
        assert!(c.status().alarm, "an unreadable disk keeps its last verdict");
        assert_eq!(c.status().bays[0].led, "on");
    }

    #[test]
    fn turning_the_setting_off_stops_the_alarm() {
        let gpio = fake_gpio();
        let (c, ..) = ctl(&gpio);
        c.apply(QnapSettings::default());
        c.set_bays(vec![bay(1, HealthVerdict::Failing)]);
        assert!(c.status().alarm);
        c.apply(QnapSettings { beep_on_disk_failure: false, ..Default::default() });
        assert!(!c.status().alarm);
    }

    #[test]
    fn silenced_at_start_and_at_shutdown() {
        let gpio = fake_gpio();
        let (c, rec, _) = ctl(&gpio);
        assert_eq!(*rec.0.lock().unwrap(), vec![0], "a previous process may have left a tone on");
        c.shutdown();
        assert_eq!(*rec.0.lock().unwrap(), vec![0, 0]);
    }

    #[test]
    fn honest_status_without_the_gpio_driver() {
        let d = tempfile::tempdir().unwrap();
        fake_hs251(d.path(), Some("f71869a"));
        let mut empty = MemoryGpio::new(Vec::new());
        let c = confirm(d.path(), &MODELS[0], &mut empty).unwrap();
        assert!(!c.gpio_loaded());
        let ctl = Controller::new(c, d.path(), Box::new(empty.clone()), Arc::new(Rec::default()), Box::new(|_| {}), None, None);
        ctl.confirm_mapping("HS-251").unwrap();
        ctl.apply(QnapSettings { led_control: true, ..Default::default() });
        ctl.tick(Instant::now());
        assert!(empty.writes().is_empty());
        let s = ctl.status();
        assert!(s.features.iter().any(|f| f.id == "status_led" && f.reason == "driver_missing"));
    }
}
