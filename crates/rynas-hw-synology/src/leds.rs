//! Bay identify: the disk LEDs are LED class devices (`/sys/class/leds/syno:green:diskN`,
//! `syno:orange:diskN`, drivers leds-synology-lp3943 / leds-atmega1608), not microcontroller
//! commands. synohwd's LED loop rewrites a bay only when the state it wants changes, so rynas
//! can borrow the LEDs of a bay: green off, orange blinking (kernel `timer` trigger, else a
//! software blink), then restore exactly the triggers and brightness it found.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rynas_hw::ControlError;

pub const LEDS: &str = "/sys/class/leds";
/// Blink half-period.
const BLINK_MS: u64 = 250;

/// Current trigger and the available ones from a `trigger` attribute (`none [ata1] timer`).
/// A file without brackets (a fake sysfs in tests) is its own current value.
pub fn parse_trigger(text: &str) -> (String, Vec<String>) {
    let mut current = None;
    let mut all = Vec::new();
    for w in text.split_whitespace() {
        if let Some(inner) = w.strip_prefix('[').and_then(|x| x.strip_suffix(']')) {
            current = Some(inner.to_string());
            all.push(inner.to_string());
        } else {
            all.push(w.to_string());
        }
    }
    let current = current.unwrap_or_else(|| {
        if all.len() == 1 {
            all[0].clone()
        } else {
            "none".into()
        }
    });
    (current, all)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Saved {
    trigger: String,
    brightness: String,
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn write(p: &Path, v: &str) -> Result<(), String> {
    std::fs::write(p, v).map_err(|e| format!("{}: {e}", p.display()))
}

fn save(led: &Path) -> Saved {
    Saved {
        trigger: parse_trigger(&read(&led.join("trigger"))).0,
        brightness: read(&led.join("brightness")),
    }
}

/// Puts a LED back: the trigger first (writing 0 to `brightness` would detach it), then the
/// brightness when it is fixed or non-zero (a non-zero value keeps the trigger: synohwd's dimming).
fn restore(led: &Path, s: &Saved) {
    let _ = write(&led.join("trigger"), &s.trigger);
    let fixed = s.trigger == "none";
    let lit = s.brightness.parse::<u32>().is_ok_and(|b| b > 0);
    if fixed || lit {
        let _ = write(&led.join("brightness"), &s.brightness);
    }
}

/// Blinking bays and their deadlines (at most one entry per bay: M1).
#[derive(Clone, Default)]
pub struct Identifier {
    root: PathBuf,
    active: Arc<Mutex<HashMap<u32, Instant>>>,
}

impl Identifier {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            active: Arc::default(),
        }
    }

    fn led(&self, color: &str, bay: u32) -> PathBuf {
        rynas_hw::under(&self.root, &format!("{LEDS}/syno:{color}:disk{bay}"))
    }

    /// The bays blinking now.
    pub fn blinking(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.active.lock().unwrap().keys().copied().collect();
        v.sort();
        v
    }

    /// Blinks bay `bay` for `duration` and restores it; returns at once. A second call
    /// while it blinks moves the deadline.
    pub fn identify(&self, bay: u32, duration: Duration) -> Result<(), ControlError> {
        let (green, orange) = (self.led("green", bay), self.led("orange", bay));
        if !orange.is_dir() && !green.is_dir() {
            return Err(ControlError::NotFound(format!("bay {bay} has no LED")));
        }
        let deadline = Instant::now() + duration;
        {
            let mut a = self.active.lock().unwrap();
            if let Some(d) = a.get_mut(&bay) {
                *d = deadline;
                return Ok(());
            }
            a.insert(bay, deadline);
        }
        let saved_green = green.is_dir().then(|| save(&green));
        let saved_orange = orange.is_dir().then(|| save(&orange));
        // the LED that blinks: orange if the bay has one
        let blink = if orange.is_dir() { &orange } else { &green };
        let other = (orange.is_dir() && green.is_dir()).then_some(&green);
        if let Some(o) = other {
            let _ = write(&o.join("trigger"), "none");
            let _ = write(&o.join("brightness"), "0");
        }
        let (_, triggers) = parse_trigger(&read(&blink.join("trigger")));
        let hw_timer = triggers.iter().any(|t| t == "timer");
        let started = if hw_timer {
            write(&blink.join("trigger"), "timer")
                .and_then(|_| write(&blink.join("delay_on"), &BLINK_MS.to_string()))
                .and_then(|_| write(&blink.join("delay_off"), &BLINK_MS.to_string()))
        } else {
            write(&blink.join("trigger"), "none")
                .and_then(|_| write(&blink.join("brightness"), "255"))
        };
        let me = self.clone();
        let blink = blink.clone();
        let finish = move || {
            if let (Some(s), true) = (&saved_orange, orange.is_dir()) {
                restore(&orange, s);
            }
            if let (Some(s), true) = (&saved_green, green.is_dir()) {
                restore(&green, s);
            }
            me.active.lock().unwrap().remove(&bay);
        };
        if let Err(e) = started {
            finish();
            return Err(ControlError::Failed(e));
        }
        let me = self.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("identify-bay{bay}"))
            .stack_size(64 << 10)
            .spawn(move || {
                let mut on = true;
                loop {
                    let due = me.active.lock().unwrap().get(&bay).copied();
                    let Some(due) = due else { break };
                    let now = Instant::now();
                    if now >= due {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(BLINK_MS).min(due - now));
                    if !hw_timer {
                        on = !on;
                        let _ = write(&blink.join("brightness"), if on { "255" } else { "0" });
                    }
                }
                finish();
            });
        spawned.map(|_| ()).map_err(|e| {
            self.active.lock().unwrap().remove(&bay);
            ControlError::Failed(format!("identify thread: {e}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn led(root: &Path, color: &str, bay: u32, trigger: &str, brightness: &str) -> PathBuf {
        let d = root.join(format!("sys/class/leds/syno:{color}:disk{bay}"));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("trigger"), trigger).unwrap();
        std::fs::write(d.join("brightness"), brightness).unwrap();
        d
    }

    #[test]
    fn triggers() {
        assert_eq!(
            parse_trigger("none [ata3] timer heartbeat\n"),
            (
                "ata3".into(),
                vec![
                    "none".into(),
                    "ata3".into(),
                    "timer".into(),
                    "heartbeat".into()
                ]
            )
        );
        assert_eq!(parse_trigger("[none] timer").0, "none");
        assert_eq!(parse_trigger("timer").0, "timer");
        assert_eq!(parse_trigger("").0, "none");
    }

    fn wait_idle(id: &Identifier) {
        for _ in 0..100 {
            if id.blinking().is_empty() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("identify did not finish");
    }

    #[test]
    fn blink_and_restore_with_the_timer_trigger() {
        let d = tempfile::tempdir().unwrap();
        let g = led(d.path(), "green", 2, "none [ata2] timer", "255");
        let o = led(d.path(), "orange", 2, "[none] timer", "0");
        let id = Identifier::new(d.path());
        id.identify(2, Duration::from_millis(300)).unwrap();
        assert_eq!(id.blinking(), vec![2]);
        assert_eq!(read(&o.join("trigger")), "timer");
        assert_eq!(read(&o.join("delay_on")), "250");
        assert_eq!(read(&g.join("brightness")), "0");
        // a second call extends, never stacks
        id.identify(2, Duration::from_millis(300)).unwrap();
        wait_idle(&id);
        assert_eq!(read(&g.join("trigger")), "ata2");
        assert_eq!(read(&g.join("brightness")), "255");
        assert_eq!(read(&o.join("trigger")), "none");
        assert_eq!(read(&o.join("brightness")), "0");
    }

    #[test]
    fn software_blink_without_timer() {
        let d = tempfile::tempdir().unwrap();
        let g = led(d.path(), "green", 1, "[none]", "64");
        let o = led(d.path(), "orange", 1, "[none]", "0");
        let id = Identifier::new(d.path());
        id.identify(1, Duration::from_millis(200)).unwrap();
        wait_idle(&id);
        // dimmed fixed green comes back dimmed
        assert_eq!(read(&g.join("brightness")), "64");
        assert_eq!(read(&o.join("brightness")), "0");
        assert_eq!(read(&o.join("trigger")), "none");
    }

    #[test]
    fn unknown_bay() {
        let d = tempfile::tempdir().unwrap();
        let id = Identifier::new(d.path());
        assert!(matches!(
            id.identify(9, Duration::from_secs(1)),
            Err(ControlError::NotFound(_))
        ));
        assert!(id.blinking().is_empty());
    }
}
