//! The one thing that needs raw Super I/O access: the power-recovery setting (no driver
//! exposes it). rynasd's seccomp filter forbids `ioperm`, so it runs as a short-lived worker,
//! `rynasctl qnap power-recovery`, in a transient unit (`rynas_tasks::Outside`).
//!
//! What the vendor's HAL does (`sio_sys_set_power_recovery_mode`, `..._get_...` and
//! `comm_sys_{get,set}_power_recovery_mode_{from,to}_cmos`), and only that:
//! - enter with 0x87, 0x87 on port 0x2E, select logical device 0x0A, read register 0xF4, write
//!   it back changing bits 2:1 only (mask 0xF9), leave with 0xAA;
//! - keep the BIOS's copy in CMOS byte 0x61: bits 7:6 only, encoded by whether the byte's low
//!   five bits equal 1. Written through the kernel's `cmos_nvram`, which takes the RTC lock.
//!
//! Added for safety, reads only: the chip's device and manufacturer ids are checked after
//! entering; both registers are read back. The vendor's modes: 0 last state (bits 00), 2 on
//! (10), 3 off (11).
//!
//! **Order is the guard**: identification and the driver's chip check come first, and the
//! functions opening the ports and the CMOS are only called with their proof.

use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::api::{PowerRecovery, PowerRecoveryState};
use crate::control::{self, Confirmed};
use crate::gpio::{CdevGpio, Direction, Gpio};
use crate::model::{self, Line, PowerRecoveryMap};

const ENTER: u8 = 0x87;
const EXIT: u8 = 0xAA;
const REG_LDN: u8 = 0x07;
const REG_DEVID: u8 = 0x20;
const REG_MANID: u8 = 0x23;
/// `cmos_nvram` starts at CMOS byte 14 (`NVRAM_OFFSET` in `rtc-cmos.c`).
const NVMEM_START: u8 = 14;

pub trait Ports {
    fn outb(&mut self, port: u16, value: u8);
    fn inb(&mut self, port: u16) -> u8;
}

pub trait Cmos {
    fn read(&mut self, offset: u8) -> Result<u8, String>;
    fn write(&mut self, offset: u8, value: u8) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Read,
    Set(PowerRecovery),
}

fn bits_of(m: PowerRecovery) -> u8 {
    match m {
        PowerRecovery::LastState => 0b00,
        PowerRecovery::PowerOn => 0b10,
        PowerRecovery::PowerOff => 0b11,
    }
}

fn mode_of_bits(bits: u8) -> Option<PowerRecovery> {
    match bits {
        0b00 => Some(PowerRecovery::LastState),
        0b10 => Some(PowerRecovery::PowerOn),
        0b11 => Some(PowerRecovery::PowerOff),
        _ => None,
    }
}

/// The vendor's encoding of the two high CMOS bits depends on the byte's low five.
fn cmos_initialized(byte: u8) -> bool {
    byte & 0x1F == 1
}

fn cmos_with(byte: u8, m: PowerRecovery) -> u8 {
    let base = byte & 0x3F;
    let high = if cmos_initialized(byte) {
        bits_of(m) << 6
    } else {
        match m {
            PowerRecovery::LastState => 0x80,
            PowerRecovery::PowerOn => 0x40,
            PowerRecovery::PowerOff => 0x00,
        }
    };
    base | high
}

fn mode_of_cmos(byte: u8) -> Option<PowerRecovery> {
    let high = byte >> 6;
    if cmos_initialized(byte) {
        mode_of_bits(high)
    } else {
        match high {
            0b10 => Some(PowerRecovery::LastState),
            0b01 => Some(PowerRecovery::PowerOn),
            0b00 => Some(PowerRecovery::PowerOff),
            _ => None,
        }
    }
}

fn reg_read(io: &mut dyn Ports, map: &PowerRecoveryMap, reg: u8) -> u8 {
    io.outb(map.sio_port, reg);
    io.inb(map.sio_port + 1)
}

/// `f` inside the chip's configuration mode with the logical device selected; refused when
/// the chip does not answer with the model's ids. Always leaves configuration mode.
fn in_chip<T>(
    io: &mut dyn Ports,
    map: &PowerRecoveryMap,
    f: impl FnOnce(&mut dyn Ports) -> T,
) -> Result<T, String> {
    io.outb(map.sio_port, ENTER);
    io.outb(map.sio_port, ENTER);
    let word = |io: &mut dyn Ports, reg: u8| {
        (u16::from(reg_read(io, map, reg)) << 8) | u16::from(reg_read(io, map, reg + 1))
    };
    let ids = (word(io, REG_DEVID), word(io, REG_MANID));
    if ids != map.sio_ids {
        io.outb(map.sio_port, EXIT);
        return Err(format!("the Super I/O answers {:04x}:{:04x}: not touched", ids.0, ids.1));
    }
    io.outb(map.sio_port, REG_LDN);
    io.outb(map.sio_port + 1, map.ldn);
    let r = f(io);
    io.outb(map.sio_port, EXIT);
    Ok(r)
}

fn read_state(io: &mut dyn Ports, cmos: &mut dyn Cmos, map: &PowerRecoveryMap) -> Result<PowerRecoveryState, String> {
    let reg = in_chip(io, map, |io| reg_read(io, map, map.reg))?;
    let byte = cmos.read(map.cmos_offset)?;
    let sio_bits = (reg & 0x06) >> 1;
    // the vendor's reader: the chip decides, the CMOS copy only for a value without a meaning
    let mode = mode_of_bits(sio_bits).or_else(|| mode_of_cmos(byte));
    Ok(PowerRecoveryState { mode, sio_bits, cmos: byte })
}

/// Reads or changes the setting. Identification and the driver's confirmation come first;
/// `open_ports` and `open_cmos` run only after them.
pub fn power_recovery(
    root: &Path,
    gpio: &mut dyn Gpio,
    request: Request,
    open_ports: impl FnOnce(&Confirmed) -> Result<Box<dyn Ports>, String>,
    open_cmos: impl FnOnce(&Confirmed) -> Result<Box<dyn Cmos>, String>,
) -> Result<PowerRecoveryState, String> {
    let m = model::identify(root).ok_or("this machine is not an identified model")?;
    let proof = control::confirm(root, m, gpio).map_err(|e| format!("chip not confirmed: {e:?}"))?;
    let map = m.power_recovery.ok_or("this model has no power recovery setting")?;
    // the CMOS first: without it the setting would be half written
    let mut cmos = open_cmos(&proof)?;
    let byte = cmos.read(map.cmos_offset)?;
    let mut ports = open_ports(&proof)?;
    let io = ports.as_mut();
    let Request::Set(mode) = request else {
        return read_state(io, cmos.as_mut(), &map);
    };
    let bits = bits_of(mode);
    in_chip(io, &map, |io| {
        let current = reg_read(io, &map, map.reg);
        io.outb(map.sio_port, map.reg);
        io.outb(map.sio_port + 1, (current & 0xF9) | (bits << 1));
    })?;
    let state = in_chip(io, &map, |io| reg_read(io, &map, map.reg))?;
    if (state & 0x06) >> 1 != bits {
        return Err(format!("register {:#x} reads {state:#04x} after the write", map.reg));
    }
    let new = cmos_with(byte, mode);
    cmos.write(map.cmos_offset, new)?;
    let back = cmos.read(map.cmos_offset)?;
    if back != new {
        return Err(format!("CMOS byte {:#x} reads {back:#04x} after writing {new:#04x}", map.cmos_offset));
    }
    read_state(io, cmos.as_mut(), &map)
}

/// The I/O ports of the Super I/O. Needs a proof to exist.
struct RealPorts {
    base: u16,
}

impl RealPorts {
    #[cfg(target_arch = "x86_64")]
    fn open(_proof: &Confirmed, map: &PowerRecoveryMap) -> Result<Self, String> {
        // SAFETY: asks for access to the two ports of the confirmed chip, nothing else
        if unsafe { libc::ioperm(map.sio_port.into(), 2, 1) } != 0 {
            return Err(format!("ioperm: {}", std::io::Error::last_os_error()));
        }
        Ok(Self { base: map.sio_port })
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn open(_proof: &Confirmed, _map: &PowerRecoveryMap) -> Result<Self, String> {
        Err("port access exists only on x86-64".into())
    }
}

#[cfg(target_arch = "x86_64")]
impl Ports for RealPorts {
    fn outb(&mut self, port: u16, value: u8) {
        debug_assert!(port == self.base || port == self.base + 1);
        // SAFETY: `open` was granted these two ports; a port write has no memory effect
        unsafe { core::arch::asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags)) }
    }

    fn inb(&mut self, port: u16) -> u8 {
        debug_assert!(port == self.base || port == self.base + 1);
        let value: u8;
        // SAFETY: `open` was granted these two ports
        unsafe { core::arch::asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack, preserves_flags)) }
        value
    }
}

#[cfg(not(target_arch = "x86_64"))]
impl Ports for RealPorts {
    fn outb(&mut self, _port: u16, _value: u8) {}

    fn inb(&mut self, _port: u16) -> u8 {
        let _ = self.base;
        0xFF
    }
}

impl Drop for RealPorts {
    fn drop(&mut self) {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: gives the same ports back
        unsafe {
            libc::ioperm(self.base.into(), 2, 0);
        }
    }
}

/// CMOS through the kernel's `cmos_nvram` (`/sys/bus/nvmem/devices/cmos_nvramN/nvmem`).
struct NvmemCmos(std::fs::File);

impl NvmemCmos {
    fn open(root: &Path) -> Result<Self, String> {
        let dir = rynas_hw::under(root, "/sys/bus/nvmem/devices");
        let found = std::fs::read_dir(&dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?
            .flatten()
            .find(|e| e.file_name().to_string_lossy().starts_with("cmos_nvram"))
            .ok_or("the kernel exposes no cmos_nvram")?;
        let path = found.path().join("nvmem");
        let f = std::fs::File::options()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self(f))
    }
}

impl Cmos for NvmemCmos {
    fn read(&mut self, offset: u8) -> Result<u8, String> {
        let mut b = [0u8; 1];
        self.0
            .read_exact_at(&mut b, (offset - NVMEM_START).into())
            .map_err(|e| format!("CMOS read: {e}"))?;
        Ok(b[0])
    }

    fn write(&mut self, offset: u8, value: u8) -> Result<(), String> {
        self.0
            .write_all_at(&[value], (offset - NVMEM_START).into())
            .map_err(|e| format!("CMOS write: {e}"))
    }
}

fn parse_request(word: &str) -> Option<Request> {
    match word {
        "read" => Some(Request::Read),
        "last_state" => Some(Request::Set(PowerRecovery::LastState)),
        "power_on" => Some(Request::Set(PowerRecovery::PowerOn)),
        "power_off" => Some(Request::Set(PowerRecovery::PowerOff)),
        _ => None,
    }
}

fn line_named(m: &model::Model, name: &str) -> Option<Line> {
    match name {
        "status-green" => Some(m.status_green),
        "status-red" => Some(m.status_red),
        "usb" => Some(m.usb_led),
        "bay1" => m.bays.first().map(|b| b.err_led),
        "bay2" => m.bays.get(1).map(|b| b.err_led),
        _ => None,
    }
}

/// `rynasctl qnap led-test`: lights one LED through the same as-is request rynasd uses (no
/// direction or drive-mode change), then puts back the level it found.
fn led_test(root: &Path, gpio: &mut dyn Gpio, name: &str, seconds: u64) -> Result<String, String> {
    let m = model::identify(root).ok_or("this machine is not an identified model")?;
    control::confirm(root, m, gpio).map_err(|e| format!("chip not confirmed: {e:?}"))?;
    let line = line_named(m, name).ok_or("unknown LED: status-green, status-red, usb, bay1, bay2")?;
    let chip = control::label_of(line);
    if gpio.direction(&chip, line.offset)? != Direction::Output {
        return Err(format!("{name} is not an output: left alone"));
    }
    let found = gpio.read(&chip, line.offset)?;
    gpio.write(&chip, line.offset, control::level(line, true))?;
    std::thread::sleep(std::time::Duration::from_secs(seconds));
    gpio.write(&chip, line.offset, found)?;
    Ok(format!("{name}: lit for {seconds} s, level put back to {}", u8::from(found)))
}

fn run_worker(args: &[String]) -> Result<String, String> {
    const USAGE: &str = "usage: rynasctl qnap power-recovery read|last_state|power_on|power_off\n       rynasctl qnap led-test <led> <seconds>";
    let root = Path::new("/");
    match args {
        [cmd, word] if cmd == "power-recovery" => {
            let request = parse_request(word).ok_or(USAGE)?;
            let state = power_recovery(
                root,
                &mut CdevGpio::default(),
                request,
                |proof| {
                    let map = proof.model().power_recovery.ok_or("no power recovery on this model")?;
                    RealPorts::open(proof, &map).map(|p| Box::new(p) as Box<dyn Ports>)
                },
                |_| NvmemCmos::open(root).map(|c| Box::new(c) as Box<dyn Cmos>),
            )?;
            serde_json::to_string(&state).map_err(|e| e.to_string())
        }
        [cmd, led, secs] if cmd == "led-test" => {
            let seconds = secs.parse::<u64>().ok().filter(|s| *s <= 30).ok_or("seconds: 0 to 30")?;
            led_test(root, &mut CdevGpio::default(), led, seconds)
        }
        _ => Err(USAGE.into()),
    }
}

/// The body of `rynasctl qnap ...`: the process's exit code.
pub fn worker_main(args: &[String]) -> i32 {
    match run_worker(args) {
        Ok(out) => {
            println!("{out}");
            0
        }
        Err(e) => {
            eprintln!("rynasctl qnap: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::tests::fake_gpio;
    use crate::model::testing::fake_hs251;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct Chip {
        index: u8,
        entered: u8,
        ldn: u8,
        f4: u8,
        ids: [u8; 4],
        log: Vec<(u16, u8)>,
    }

    /// The chip's configuration registers, with every port write logged.
    struct FakePorts(Rc<RefCell<Chip>>);

    impl Ports for FakePorts {
        fn outb(&mut self, port: u16, v: u8) {
            let mut c = self.0.borrow_mut();
            c.log.push((port, v));
            if port == 0x2E {
                match v {
                    0x87 => c.entered += 1,
                    0xAA => c.entered = 0,
                    _ => c.index = v,
                }
                return;
            }
            assert_eq!(c.entered, 2, "a register written outside configuration mode");
            match c.index {
                0x07 => c.ldn = v,
                0xF4 => {
                    assert_eq!(c.ldn, 0x0A);
                    c.f4 = v
                }
                r => panic!("unexpected write to register {r:#x}"),
            }
        }

        fn inb(&mut self, _port: u16) -> u8 {
            let c = self.0.borrow();
            match c.index {
                0x20 => c.ids[0],
                0x21 => c.ids[1],
                0x23 => c.ids[2],
                0x24 => c.ids[3],
                0xF4 => c.f4,
                r => panic!("unexpected read of register {r:#x}"),
            }
        }
    }

    #[derive(Default)]
    struct FakeCmosInner {
        byte: u8,
        writes: Vec<u8>,
    }

    struct FakeCmos(Rc<RefCell<FakeCmosInner>>);

    impl Cmos for FakeCmos {
        fn read(&mut self, offset: u8) -> Result<u8, String> {
            assert_eq!(offset, 0x61);
            Ok(self.0.borrow().byte)
        }

        fn write(&mut self, _offset: u8, v: u8) -> Result<(), String> {
            let mut c = self.0.borrow_mut();
            c.byte = v;
            c.writes.push(v);
            Ok(())
        }
    }

    fn chip(f4: u8) -> Rc<RefCell<Chip>> {
        Rc::new(RefCell::new(Chip { f4, ids: [0x10, 0x07, 0x19, 0x34], ..Chip::default() }))
    }

    fn cmos(byte: u8) -> Rc<RefCell<FakeCmosInner>> {
        Rc::new(RefCell::new(FakeCmosInner { byte, writes: Vec::new() }))
    }

    fn hs251() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        fake_hs251(d.path(), Some("f71869a"));
        d
    }

    fn run(
        root: &Path,
        request: Request,
        c: &Rc<RefCell<Chip>>,
        m: &Rc<RefCell<FakeCmosInner>>,
    ) -> Result<PowerRecoveryState, String> {
        let (cc, mm) = (c.clone(), m.clone());
        power_recovery(
            root,
            &mut fake_gpio(),
            request,
            move |_| Ok(Box::new(FakePorts(cc)) as Box<dyn Ports>),
            move |_| Ok(Box::new(FakeCmos(mm)) as Box<dyn Cmos>),
        )
    }

    #[test]
    fn the_vendor_sequence_and_only_bits_2_1_change() {
        let d = hs251();
        let (c, m) = (chip(0xFF), cmos(0x3F));
        let s = run(d.path(), Request::Set(PowerRecovery::LastState), &c, &m).unwrap();
        assert_eq!(c.borrow().f4, 0xF9, "other bits kept");
        assert_eq!(s.mode, Some(PowerRecovery::LastState));
        let log = &c.borrow().log;
        assert_eq!(&log[..2], &[(0x2E, 0x87), (0x2E, 0x87)]);
        assert_eq!(*log.last().unwrap(), (0x2E, 0xAA));
        assert!(log.windows(2).any(|w| w == [(0x2E, 0x07), (0x2F, 0x0A)]), "logical device 0x0A");
        assert_eq!(c.borrow().entered, 0, "left configuration mode");
        let (c, m) = (chip(0x00), cmos(0x3F));
        let s = run(d.path(), Request::Set(PowerRecovery::PowerOn), &c, &m).unwrap();
        assert_eq!((c.borrow().f4, s.mode), (0x04, Some(PowerRecovery::PowerOn)));
        let (c, m) = (chip(0x00), cmos(0x3F));
        run(d.path(), Request::Set(PowerRecovery::PowerOff), &c, &m).unwrap();
        assert_eq!(c.borrow().f4, 0x06);
    }

    #[test]
    fn cmos_copy_follows_the_vendor_encoding() {
        // low five bits == 1: 00 / 10 / 11, the other bits kept
        assert_eq!(cmos_with(0x21, PowerRecovery::LastState), 0x21);
        assert_eq!(cmos_with(0x21, PowerRecovery::PowerOn), 0xA1);
        assert_eq!(cmos_with(0x21, PowerRecovery::PowerOff), 0xE1);
        // anything else: 10 / 01 / 00
        assert_eq!(cmos_with(0x3F, PowerRecovery::LastState), 0xBF);
        assert_eq!(cmos_with(0x3F, PowerRecovery::PowerOn), 0x7F);
        assert_eq!(cmos_with(0x3F, PowerRecovery::PowerOff), 0x3F);
        for m in [PowerRecovery::LastState, PowerRecovery::PowerOn, PowerRecovery::PowerOff] {
            assert_eq!(mode_of_cmos(cmos_with(0x21, m)), Some(m));
            assert_eq!(mode_of_cmos(cmos_with(0x3F, m)), Some(m));
        }
    }

    #[test]
    fn reading_writes_nothing() {
        let d = hs251();
        let (c, m) = (chip(0b100), cmos(0x21));
        let s = run(d.path(), Request::Read, &c, &m).unwrap();
        assert_eq!((s.mode, s.sio_bits), (Some(PowerRecovery::PowerOn), 0b10));
        assert_eq!(c.borrow().f4, 0b100);
        assert!(m.borrow().writes.is_empty());
        // a value without a meaning in the chip: the BIOS's copy decides, like the vendor's
        let (c, m) = (chip(0b010), cmos(0xE1));
        assert_eq!(run(d.path(), Request::Read, &c, &m).unwrap().mode, Some(PowerRecovery::PowerOff));
    }

    #[test]
    fn the_ports_are_never_opened_on_other_hardware() {
        let opened = RefCell::new(0);
        let attempt = |root: &Path, gpio: &mut dyn Gpio| {
            power_recovery(
                root,
                gpio,
                Request::Set(PowerRecovery::PowerOn),
                |_| {
                    *opened.borrow_mut() += 1;
                    Err("opened".to_string())
                },
                |_| {
                    *opened.borrow_mut() += 1;
                    Err("opened".to_string())
                },
            )
        };
        // nothing identified
        let empty = tempfile::tempdir().unwrap();
        assert!(attempt(empty.path(), &mut fake_gpio()).is_err());
        // the signature matches, the driver says another chip
        let other = tempfile::tempdir().unwrap();
        fake_hs251(other.path(), Some("f71889fg"));
        assert!(attempt(other.path(), &mut fake_gpio()).is_err());
        // the signature matches, no driver loaded
        let none = tempfile::tempdir().unwrap();
        fake_hs251(none.path(), None);
        assert!(attempt(none.path(), &mut fake_gpio()).is_err());
        // one PCI device more
        let extra = hs251();
        std::fs::create_dir_all(extra.path().join("sys/bus/pci/devices/0000:05:00.0")).unwrap();
        assert!(attempt(extra.path(), &mut fake_gpio()).is_err());
        assert_eq!(*opened.borrow(), 0, "not one port, not the CMOS");
    }

    #[test]
    fn another_chip_on_the_port_is_left_alone() {
        let d = hs251();
        let (c, m) = (chip(0x00), cmos(0x21));
        c.borrow_mut().ids = [0x09, 0x09, 0x19, 0x34];
        let e = run(d.path(), Request::Set(PowerRecovery::PowerOn), &c, &m).unwrap_err();
        assert!(e.contains("not touched"), "{e}");
        assert_eq!(c.borrow().f4, 0x00);
        assert!(m.borrow().writes.is_empty());
        assert_eq!(*c.borrow().log.last().unwrap(), (0x2E, 0xAA), "still leaves configuration mode");
    }

    #[test]
    fn no_cmos_means_nothing_is_written() {
        let d = hs251();
        let c = chip(0x00);
        let cc = c.clone();
        let r = power_recovery(
            d.path(),
            &mut fake_gpio(),
            Request::Set(PowerRecovery::PowerOn),
            move |_| Ok(Box::new(FakePorts(cc)) as Box<dyn Ports>),
            |_| Err("the kernel exposes no cmos_nvram".to_string()),
        );
        assert!(r.is_err());
        assert!(c.borrow().log.is_empty());
    }

    #[test]
    fn led_test_lights_and_puts_back() {
        let d = hs251();
        let g = fake_gpio();
        let mut g2 = g.clone();
        led_test(d.path(), &mut g2, "status-green", 0).unwrap();
        assert_eq!(g.writes(), vec![
            ("gpio-f7188x-6".to_string(), 2, false),
            ("gpio-f7188x-6".to_string(), 2, true),
        ]);
        assert!(led_test(d.path(), &mut g2, "nothing", 0).is_err());
        let empty = tempfile::tempdir().unwrap();
        assert!(led_test(empty.path(), &mut g2, "status-green", 0).is_err());
    }

    #[test]
    fn worker_arguments() {
        assert_eq!(parse_request("read"), Some(Request::Read));
        assert_eq!(parse_request("power_on"), Some(Request::Set(PowerRecovery::PowerOn)));
        assert_eq!(parse_request("nope"), None);
        assert_ne!(worker_main(&["nope".to_string()]), 0);
    }
}
