//! GPIO lines through the character device (`/dev/gpiochip*`, uAPI v2, kernel >= 5.10): chips
//! are found by label, so no GPIO numbers are hardcoded (gpio-f7188x registers each bank with
//! a dynamic base). The backends are behind [`Gpio`]: the real one, and [`MemoryGpio`] for
//! tests and the dry instance. Requests are held until [`Gpio::release`].
//!
//! Lines are requested with **no direction flag** ("as is", `gpiolib-cdev.c`): the kernel then
//! neither rewrites the direction register nor forces push-pull (requesting `OUTPUT` writes
//! `regbase + 3` and survives the process). Only the data register is written, which is all
//! the vendor's HAL does on these lines; a write works because the line is already an output,
//! which `direction` checks. **Only `control` and `superio` call this, after `confirm`.**

use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChipInfo {
    pub label: String,
    pub lines: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Input,
    Output,
}

pub trait Gpio: Send {
    fn chips(&mut self) -> Result<Vec<ChipInfo>, String>;
    fn direction(&mut self, chip: &str, line: u32) -> Result<Direction, String>;
    /// Electrical level of an input line.
    fn read(&mut self, chip: &str, line: u32) -> Result<bool, String>;
    /// Drives an output line to an electrical level.
    fn write(&mut self, chip: &str, line: u32, high: bool) -> Result<(), String>;
    /// Gives a line back (its level stays).
    fn release(&mut self, chip: &str, line: u32);
}

const fn ior(nr: u64, size: u64) -> libc::c_ulong {
    ((2u64 << 30) | (size << 16) | (0xB4u64 << 8) | nr) as libc::c_ulong
}
const fn iowr(nr: u64, size: u64) -> libc::c_ulong {
    ((3u64 << 30) | (size << 16) | (0xB4u64 << 8) | nr) as libc::c_ulong
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ChipInfoRaw {
    name: [u8; 32],
    label: [u8; 32],
    lines: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Attr {
    id: u32,
    padding: u32,
    value: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct LineInfo {
    name: [u8; 32],
    consumer: [u8; 32],
    offset: u32,
    num_attrs: u32,
    flags: u64,
    attrs: [Attr; 10],
    padding: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ConfigAttr {
    attr: Attr,
    mask: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct LineConfig {
    flags: u64,
    num_attrs: u32,
    padding: [u32; 5],
    attrs: [ConfigAttr; 10],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct LineRequest {
    offsets: [u32; 64],
    consumer: [u8; 32],
    config: LineConfig,
    num_lines: u32,
    event_buffer_size: u32,
    padding: [u32; 5],
    fd: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LineValues {
    bits: u64,
    mask: u64,
}

const GET_CHIPINFO: libc::c_ulong = ior(0x01, size_of::<ChipInfoRaw>() as u64);
const GET_LINEINFO: libc::c_ulong = iowr(0x05, size_of::<LineInfo>() as u64);
const GET_LINE: libc::c_ulong = iowr(0x07, size_of::<LineRequest>() as u64);
const GET_VALUES: libc::c_ulong = iowr(0x0E, size_of::<LineValues>() as u64);
const SET_VALUES: libc::c_ulong = iowr(0x0F, size_of::<LineValues>() as u64);
const FLAG_OUTPUT: u64 = 1 << 3;

/// SAFETY of the callers: `T` is the `repr(C)` structure the kernel expects for `req`.
unsafe fn ioctl_ptr<T>(fd: i32, req: libc::c_ulong, arg: *mut T) -> std::io::Result<()> {
    if unsafe { libc::ioctl(fd, req, arg) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|c| *c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

fn zero<T: Copy>() -> T {
    // SAFETY: only instantiated with the plain-data structures above
    unsafe { std::mem::zeroed() }
}

struct Chip {
    label: String,
    lines: u32,
    file: std::fs::File,
    held: HashMap<u32, OwnedFd>,
}

/// The real backend. Chips are opened once (the banks do not change while rynasd runs).
#[derive(Default)]
pub struct CdevGpio {
    chips: Vec<Chip>,
    opened: bool,
}

/// The request of one line, as is: no direction, no drive mode, no output value.
fn line_request(line: u32) -> LineRequest {
    let mut req: LineRequest = zero();
    req.offsets[0] = line;
    let name = b"rynas-qnap";
    req.consumer[..name.len()].copy_from_slice(name);
    req.num_lines = 1;
    req
}

impl CdevGpio {
    fn open(&mut self) -> Result<(), String> {
        if self.opened {
            return Ok(());
        }
        let mut names: Vec<String> = std::fs::read_dir("/dev")
            .map_err(|e| format!("/dev: {e}"))?
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("gpiochip"))
            .collect();
        names.sort();
        // all of them or none: a failure halfway must not leave a partial list behind
        let mut chips = Vec::with_capacity(names.len());
        for n in names {
            let path = format!("/dev/{n}");
            let file = std::fs::File::options()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|e| format!("{path}: {e}"))?;
            let mut info: ChipInfoRaw = zero();
            // SAFETY: GET_CHIPINFO fills a gpiochip_info
            unsafe { ioctl_ptr(file.as_raw_fd(), GET_CHIPINFO, &mut info) }
                .map_err(|e| format!("{path}: {e}"))?;
            chips.push(Chip {
                label: cstr(&info.label),
                lines: info.lines,
                file,
                held: HashMap::new(),
            });
        }
        self.chips = chips;
        self.opened = true;
        Ok(())
    }

    fn chip(&mut self, label: &str) -> Result<&mut Chip, String> {
        self.open()?;
        self.chips
            .iter_mut()
            .find(|c| c.label == label)
            .ok_or_else(|| format!("no GPIO chip labelled {label}"))
    }

    fn request(chip: &mut Chip, line: u32) -> Result<i32, String> {
        if let Some(fd) = chip.held.get(&line) {
            return Ok(fd.as_raw_fd());
        }
        let mut req = line_request(line);
        // SAFETY: GET_LINE reads and fills a gpio_v2_line_request
        unsafe { ioctl_ptr(chip.file.as_raw_fd(), GET_LINE, &mut req) }
            .map_err(|e| format!("{} line {line}: {e}", chip.label))?;
        // SAFETY: the kernel returned a new file descriptor that nothing else owns
        let fd = unsafe { OwnedFd::from_raw_fd(req.fd) };
        let raw = fd.as_raw_fd();
        chip.held.insert(line, fd);
        Ok(raw)
    }
}

impl Gpio for CdevGpio {
    fn chips(&mut self) -> Result<Vec<ChipInfo>, String> {
        self.open()?;
        Ok(self
            .chips
            .iter()
            .map(|c| ChipInfo { label: c.label.clone(), lines: c.lines })
            .collect())
    }

    fn direction(&mut self, chip: &str, line: u32) -> Result<Direction, String> {
        let c = self.chip(chip)?;
        let mut info: LineInfo = zero();
        info.offset = line;
        // SAFETY: GET_LINEINFO reads and fills a gpio_v2_line_info
        unsafe { ioctl_ptr(c.file.as_raw_fd(), GET_LINEINFO, &mut info) }
            .map_err(|e| format!("{chip} line {line}: {e}"))?;
        Ok(if info.flags & FLAG_OUTPUT != 0 { Direction::Output } else { Direction::Input })
    }

    fn read(&mut self, chip: &str, line: u32) -> Result<bool, String> {
        let c = self.chip(chip)?;
        let fd = Self::request(c, line)?;
        let mut v = LineValues { bits: 0, mask: 1 };
        // SAFETY: GET_VALUES reads and fills a gpio_v2_line_values
        unsafe { ioctl_ptr(fd, GET_VALUES, &mut v) }.map_err(|e| format!("{chip} line {line}: {e}"))?;
        Ok(v.bits & 1 != 0)
    }

    fn write(&mut self, chip: &str, line: u32, high: bool) -> Result<(), String> {
        let c = self.chip(chip)?;
        let fd = Self::request(c, line)?;
        let mut v = LineValues { bits: u64::from(high), mask: 1 };
        // SAFETY: SET_VALUES reads a gpio_v2_line_values
        unsafe { ioctl_ptr(fd, SET_VALUES, &mut v) }.map_err(|e| format!("{chip} line {line}: {e}"))
    }

    fn release(&mut self, chip: &str, line: u32) {
        if let Ok(c) = self.chip(chip) {
            c.held.remove(&line);
        }
    }
}

/// Lines in memory: tests, and `rynasd --dry-system` (never the host's GPIO).
#[derive(Clone, Default)]
pub struct MemoryGpio(Arc<Mutex<Mem>>);

#[derive(Default)]
struct Mem {
    chips: Vec<ChipInfo>,
    direction: HashMap<(String, u32), Direction>,
    level: HashMap<(String, u32), bool>,
    writes: Vec<(String, u32, bool)>,
    released: Vec<(String, u32)>,
    fail_write: bool,
    fail_chips: bool,
}

impl MemoryGpio {
    pub fn new(chips: Vec<ChipInfo>) -> Self {
        Self(Arc::new(Mutex::new(Mem { chips, ..Mem::default() })))
    }

    pub fn set_direction(&self, chip: &str, line: u32, d: Direction) {
        self.0.lock().unwrap().direction.insert((chip.into(), line), d);
    }

    pub fn set_level(&self, chip: &str, line: u32, high: bool) {
        self.0.lock().unwrap().level.insert((chip.into(), line), high);
    }

    pub fn level(&self, chip: &str, line: u32) -> Option<bool> {
        self.0.lock().unwrap().level.get(&(chip.to_string(), line)).copied()
    }

    /// Writes fail from now on (or work again).
    pub fn set_fail_write(&self, fail: bool) {
        self.0.lock().unwrap().fail_write = fail;
    }

    /// The chip list becomes unreadable (or readable again).
    pub fn set_fail_chips(&self, fail: bool) {
        self.0.lock().unwrap().fail_chips = fail;
    }

    /// Lines given back so far.
    pub fn released(&self) -> Vec<(String, u32)> {
        self.0.lock().unwrap().released.clone()
    }

    /// Every write so far: `(chip, line, level)`.
    pub fn writes(&self) -> Vec<(String, u32, bool)> {
        self.0.lock().unwrap().writes.clone()
    }
}

impl Gpio for MemoryGpio {
    fn chips(&mut self) -> Result<Vec<ChipInfo>, String> {
        let g = self.0.lock().unwrap();
        if g.fail_chips {
            return Err("permission denied".into());
        }
        Ok(g.chips.clone())
    }

    fn direction(&mut self, chip: &str, line: u32) -> Result<Direction, String> {
        let g = self.0.lock().unwrap();
        if !g.chips.iter().any(|c| c.label == chip && line < c.lines) {
            return Err(format!("no line {line} on {chip}"));
        }
        Ok(g.direction.get(&(chip.to_string(), line)).copied().unwrap_or(Direction::Input))
    }

    fn read(&mut self, chip: &str, line: u32) -> Result<bool, String> {
        Ok(self.0.lock().unwrap().level.get(&(chip.to_string(), line)).copied().unwrap_or(true))
    }

    fn write(&mut self, chip: &str, line: u32, high: bool) -> Result<(), String> {
        let mut g = self.0.lock().unwrap();
        if g.fail_write {
            return Err("write failed".into());
        }
        g.level.insert((chip.to_string(), line), high);
        g.writes.push((chip.to_string(), line, high));
        Ok(())
    }

    fn release(&mut self, chip: &str, line: u32) {
        self.0.lock().unwrap().released.push((chip.to_string(), line));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sizes the kernel's uAPI defines (`include/uapi/linux/gpio.h`): the ioctl numbers
    /// are built from them.
    #[test]
    fn uapi_sizes() {
        assert_eq!(size_of::<ChipInfoRaw>(), 68);
        assert_eq!(size_of::<LineInfo>(), 256);
        assert_eq!(size_of::<LineRequest>(), 592);
        assert_eq!(size_of::<LineValues>(), 16);
        assert_eq!(GET_LINE, 0xC250_B407);
        assert_eq!(GET_LINEINFO, 0xC100_B405);
        assert_eq!(GET_CHIPINFO, 0x8044_B401);
    }

    /// No direction flag and no drive flag: the kernel leaves the line's registers alone.
    #[test]
    fn lines_are_requested_as_is() {
        let r = line_request(3);
        assert_eq!((r.offsets[0], r.num_lines, r.config.flags, r.config.num_attrs), (3, 1, 0, 0));
    }
}
