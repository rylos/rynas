//! The buzzer. The vendor's HAL (`libuLinux_hal.so`, `sub_83340` behind `se_sys_set_buzzer`)
//! drives the buzzer of this class of model as the PC speaker: PIT channel 2 mode 3
//! (0xB6 to port 0x43, divisor 0x05D3 = 1491, about 800 Hz) gated by port 0x61 bits 0-1. The
//! Linux interface for exactly that is the `pcspkr` driver, an input device taking
//! `EV_SND`/`SND_TONE`: no port access here. A short beep is 500 ms, a long one 1 s; the
//! patterns below follow the vendor's mode table.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::api::BeepKind;

/// `(on_ms, off_ms)` steps; the last `off_ms` is not waited for.
pub fn pattern(kind: BeepKind) -> &'static [(u32, u32)] {
    const S: u32 = 500;
    const L: u32 = 1000;
    match kind {
        BeepKind::Short => &[(S, 0)],
        BeepKind::Long => &[(L, 0)],
        BeepKind::ThreeShort => &[(S, 1000), (S, 1000), (S, 0)],
        BeepKind::RapidThree => &[(S, 125), (S, 125), (S, 0)],
        BeepKind::TwoLong => &[(L, 2000), (L, 0)],
        BeepKind::ThreeLong => &[(L, 2000), (L, 2000), (L, 0)],
        BeepKind::LongTwoShort => &[(L, 500), (S, 250), (S, 0)],
    }
}

pub trait Speaker: Send + Sync {
    /// Starts a tone; it lasts until [`Speaker::stop`].
    fn start(&self, hz: u32) -> Result<(), String>;
    /// Silence. Harmless when nothing is sounding.
    fn stop(&self);
    /// Whether a speaker device exists.
    fn available(&self) -> bool {
        true
    }
}

/// `/dev/input/eventN` of the `PC Speaker` input device, found through sysfs.
pub fn find_device(root: &Path) -> Option<PathBuf> {
    let rd = std::fs::read_dir(rynas_hw::under(root, "/sys/class/input")).ok()?;
    rd.flatten().find_map(|e| {
        let n = e.file_name().into_string().ok()?;
        n.starts_with("event").then_some(())?;
        let name = std::fs::read_to_string(e.path().join("device/name")).ok()?;
        (name.trim() == "PC Speaker").then(|| PathBuf::from(format!("/dev/input/{n}")))
    })
}

/// The real speaker: `EV_SND`/`SND_TONE` on the input device (the tone stops at value 0).
/// `pcspkr` has no timeout and no close callback: a tone already sounding when the process is
/// killed goes on until something writes 0, so the controller silences at start and at
/// shutdown, and the only remaining case is SIGKILL or a crash in the middle of a tone, which
/// lasts until the next start of rynasd.
pub struct PcSpeaker {
    device: Option<PathBuf>,
    file: std::sync::Mutex<Option<std::fs::File>>,
}

impl PcSpeaker {
    pub fn new(root: &Path) -> Self {
        Self { device: find_device(root), file: std::sync::Mutex::new(None) }
    }

    fn write_tone(&self, hz: u32) -> Result<(), String> {
        let path = self.device.as_ref().ok_or("no PC speaker device")?;
        let mut held = self.file.lock().unwrap();
        if held.is_none() {
            let f = std::fs::File::options()
                .write(true)
                .open(path)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            *held = Some(f);
        }
        let mut f = held.as_ref().unwrap();
        f.write_all(&event_bytes(hz)).map_err(|e| format!("PC speaker: {e}"))
    }
}

/// A `struct input_event` of type `EV_SND`, code `SND_TONE`.
fn event_bytes(hz: u32) -> [u8; 24] {
    const EV_SND: u16 = 0x12;
    const SND_TONE: u16 = 0x02;
    let mut b = [0u8; 24];
    b[16..18].copy_from_slice(&EV_SND.to_ne_bytes());
    b[18..20].copy_from_slice(&SND_TONE.to_ne_bytes());
    b[20..24].copy_from_slice(&hz.to_ne_bytes());
    b
}

impl Speaker for PcSpeaker {
    fn start(&self, hz: u32) -> Result<(), String> {
        self.write_tone(hz)
    }

    fn stop(&self) {
        if self.device.is_some()
            && let Err(e) = self.write_tone(0)
        {
            tracing::warn!("qnap: silencing the speaker: {e}");
        }
    }

    fn available(&self) -> bool {
        self.device.is_some()
    }
}

/// `rynasd --dry-system`: never the host's speaker.
pub struct SilentSpeaker;

impl Speaker for SilentSpeaker {
    fn start(&self, _hz: u32) -> Result<(), String> {
        Ok(())
    }

    fn stop(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_follow_the_vendor_modes() {
        // modes 0 / 1: one short / one long
        assert_eq!(pattern(BeepKind::Short), &[(500, 0)]);
        assert_eq!(pattern(BeepKind::Long), &[(1000, 0)]);
        // mode 14: three short with 125 ms gaps
        assert_eq!(pattern(BeepKind::RapidThree), &[(500, 125), (500, 125), (500, 0)]);
        // modes 12, 13: a long, 500 ms, a short, 250 ms, a short
        assert_eq!(pattern(BeepKind::LongTwoShort), &[(1000, 500), (500, 250), (500, 0)]);
        assert_eq!(pattern(BeepKind::ThreeLong).len(), 3);
    }

    #[test]
    fn the_divisor_is_the_tone() {
        // pcspkr: count = PIT_TICK_RATE / value, the vendor's divisor 0x05D3
        assert_eq!(1_193_182 / 800, 0x05D3);
    }

    #[test]
    fn input_event_layout() {
        let b = event_bytes(800);
        assert_eq!(u16::from_ne_bytes([b[16], b[17]]), 0x12);
        assert_eq!(u16::from_ne_bytes([b[18], b[19]]), 0x02);
        assert_eq!(u32::from_ne_bytes([b[20], b[21], b[22], b[23]]), 800);
    }

    #[test]
    fn finds_the_speaker_by_name() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(find_device(d.path()), None);
        for (e, name) in [("event3", "Power Button"), ("event7", "PC Speaker")] {
            let p = d.path().join(format!("sys/class/input/{e}/device"));
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("name"), format!("{name}\n")).unwrap();
        }
        assert_eq!(find_device(d.path()).unwrap(), Path::new("/dev/input/event7"));
    }
}
