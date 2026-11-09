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
//! - activation of the GPIO logical device (`sub_95A80`: LDN 6, register 0x30 = 1), which
//!   `gpio-f7188x` does not do, only on an explicit request.
//!
//! Added for safety, reads only: the chip's ids after entering; the selected logical device
//! before and after every write; the whole register read back.
//!
//! **Order is the guard**: identification, the driver's chip check and the user's typed
//! confirmation come first; the functions opening the ports and the CMOS are only called with
//! that proof. Port access is refused while rynasd polls the GPIO banks ([`POLL_LOCK`]).
//!
//! The race that cannot be removed: the configuration mode is one state of the chip and the
//! kernel's `request_muxed_region` cannot be taken from user space. `gpio-f7188x` only enters it
//! when a line is used, and rynasd's are stopped for the worker's whole run; `f71882fg` only at
//! its init; `fintek-cir` at probe, suspend and when its device is opened. A `gpioinfo` or
//! `gpioget` by an administrator in the same milliseconds can still interleave. The checks make
//! that fail with an error rather than a silent wrong write, within the few port accesses
//! between the last check and the write.

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
const REG_ACTIVATE: u8 = 0x30;
/// EuP registers of logical device 0x0A (`sio_sys_get_eup_state`): read only here.
const REG_EUP_INIT: u8 = 0xE0;
const REG_EUP: u8 = 0xE1;
/// `cmos_nvram` starts at CMOS byte 14 (`NVRAM_OFFSET` in `rtc-cmos.c`).
const NVMEM_START: u8 = 14;
/// Held by rynasd while it polls: a worker may run only when it is free.
pub const POLL_LOCK: &str = "/run/rynas/qnap-superio.lock";
const STATE_DIR: &str = "/var/lib/rynas";

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
    ActivateGpio,
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

/// The selected logical device is still `ldn`: someone else may have changed it.
fn ensure_ldn(io: &mut dyn Ports, map: &PowerRecoveryMap, ldn: u8) -> Result<(), String> {
    let now = reg_read(io, map, REG_LDN);
    if now != ldn {
        return Err(format!("the selected logical device became {now:#x}: nothing more is written"));
    }
    Ok(())
}

/// Writes one register of `ldn`, the logical device checked right before and right after.
fn write_reg(io: &mut dyn Ports, map: &PowerRecoveryMap, ldn: u8, reg: u8, value: u8) -> Result<(), String> {
    ensure_ldn(io, map, ldn)?;
    io.outb(map.sio_port, reg);
    io.outb(map.sio_port + 1, value);
    ensure_ldn(io, map, ldn)
}

/// `f` inside the chip's configuration mode with `ldn` selected; refused when the chip does
/// not answer with the model's ids. Always leaves configuration mode.
fn in_chip<T>(
    io: &mut dyn Ports,
    map: &PowerRecoveryMap,
    ldn: u8,
    f: impl FnOnce(&mut dyn Ports) -> Result<T, String>,
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
    io.outb(map.sio_port + 1, ldn);
    let r = f(io);
    io.outb(map.sio_port, EXIT);
    r
}

/// The vendor's EuP reader: register 0xE0 bit 7 when the BIOS initialized the CMOS byte,
/// otherwise register 0xE1 bits 3:2 both set.
fn eup_on(init: u8, eup: u8, cmos: u8) -> bool {
    if cmos_initialized(cmos) { init & 0x80 != 0 } else { eup & 0x0C == 0x0C }
}

fn read_state(io: &mut dyn Ports, cmos: &mut dyn Cmos, map: &PowerRecoveryMap) -> Result<PowerRecoveryState, String> {
    let (reg, init, eup) = in_chip(io, map, map.ldn, |io| {
        Ok((reg_read(io, map, map.reg), reg_read(io, map, REG_EUP_INIT), reg_read(io, map, REG_EUP)))
    })?;
    let activate = in_chip(io, map, map.gpio_ldn, |io| Ok(reg_read(io, map, REG_ACTIVATE)))?;
    let byte = cmos.read(map.cmos_offset)?;
    let sio_bits = (reg & 0x06) >> 1;
    // the vendor's reader: the chip decides, the CMOS copy only for a value without a meaning
    let mode = mode_of_bits(sio_bits).or_else(|| mode_of_cmos(byte));
    Ok(PowerRecoveryState {
        mode,
        sio_bits,
        cmos: byte,
        eup: eup_on(init, eup, byte),
        gpio_active: activate & 1 != 0,
    })
}

/// Changes bits 2:1 and returns the register's previous and new value.
fn write_mode(io: &mut dyn Ports, map: &PowerRecoveryMap, bits: u8) -> Result<(u8, u8), String> {
    in_chip(io, map, map.ldn, |io| {
        ensure_ldn(io, map, map.ldn)?;
        let current = reg_read(io, map, map.reg);
        let expected = (current & 0xF9) | (bits << 1);
        write_reg(io, map, map.ldn, map.reg, expected)?;
        Ok((current, expected))
    })
}

/// Puts the whole register back after a failure further on.
fn restore_reg(io: &mut dyn Ports, map: &PowerRecoveryMap, value: u8) -> Result<(), String> {
    in_chip(io, map, map.ldn, |io| write_reg(io, map, map.ldn, map.reg, value))
}

fn set_mode(
    io: &mut dyn Ports,
    cmos: &mut dyn Cmos,
    map: &PowerRecoveryMap,
    byte: u8,
    mode: PowerRecovery,
) -> Result<PowerRecoveryState, String> {
    let before = read_state(io, cmos, map)?;
    if before.eup {
        return Err("EuP is on: the power recovery setting has no effect until it is off".into());
    }
    let (previous, expected) = write_mode(io, map, bits_of(mode))?;
    let now = in_chip(io, map, map.ldn, |io| Ok(reg_read(io, map, map.reg)))?;
    if now != expected {
        let undo = restore_reg(io, map, previous).map_or_else(|e| format!(", not restored: {e}"), |_| ", restored".into());
        return Err(format!("register {:#x} reads {now:#04x}, expected {expected:#04x}{undo}", map.reg));
    }
    let new = cmos_with(byte, mode);
    let written = cmos.write(map.cmos_offset, new).and_then(|_| cmos.read(map.cmos_offset)).and_then(|back| {
        if back == new { Ok(()) } else { Err(format!("reads {back:#04x}, expected {new:#04x}")) }
    });
    if let Err(e) = written {
        let undo = restore_reg(io, map, previous).map_or_else(|e| format!("; chip not restored: {e}"), |_| "; chip restored".into());
        let _ = cmos.write(map.cmos_offset, byte);
        return Err(format!("CMOS byte {:#x}: {e}{undo}", map.cmos_offset));
    }
    read_state(io, cmos, map)
}

/// The vendor's activation of the GPIO logical device; no write when it is already active.
fn activate_gpio(io: &mut dyn Ports, cmos: &mut dyn Cmos, map: &PowerRecoveryMap) -> Result<PowerRecoveryState, String> {
    in_chip(io, map, map.gpio_ldn, |io| {
        ensure_ldn(io, map, map.gpio_ldn)?;
        if reg_read(io, map, REG_ACTIVATE) & 1 == 0 {
            write_reg(io, map, map.gpio_ldn, REG_ACTIVATE, 1)?;
        }
        Ok(())
    })?;
    let state = read_state(io, cmos, map)?;
    if !state.gpio_active {
        return Err("the GPIO logical device did not activate".into());
    }
    Ok(state)
}

/// Reads or changes the setting. Identification, the driver's confirmation and the user's
/// typed confirmation come first; `open_ports` and `open_cmos` run only after them.
pub fn power_recovery(
    root: &Path,
    state_dir: &Path,
    gpio: &mut dyn Gpio,
    request: Request,
    open_ports: impl FnOnce(&Confirmed) -> Result<Box<dyn Ports>, String>,
    open_cmos: impl FnOnce(&Confirmed) -> Result<Box<dyn Cmos>, String>,
) -> Result<PowerRecoveryState, String> {
    let m = model::identify(root).ok_or("this machine is not an identified model")?;
    let proof = control::confirm(root, m, gpio).map_err(|e| format!("chip not confirmed: {e:?}"))?;
    let map = m.power_recovery.ok_or("this model has no power recovery setting")?;
    if !control::mapping_confirmed_on(root, state_dir, m) {
        return Err("the model is not confirmed on this machine (Hardware > Front panel)".into());
    }
    // the CMOS first: without it the setting would be half written
    let mut cmos = open_cmos(&proof)?;
    let byte = cmos.read(map.cmos_offset)?;
    let mut ports = open_ports(&proof)?;
    let io = ports.as_mut();
    match request {
        Request::Read => read_state(io, cmos.as_mut(), &map),
        Request::Set(mode) => set_mode(io, cmos.as_mut(), &map, byte, mode),
        Request::ActivateGpio => activate_gpio(io, cmos.as_mut(), &map),
    }
}

/// Refuses a worker while rynasd is polling (it holds this lock except while it runs one).
fn take_poll_lock(path: &Path) -> Result<std::fs::File, String> {
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
    // SAFETY: flock on a descriptor this function owns
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("rynasd is polling this chip: use Hardware > Front panel".into());
    }
    Ok(f)
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
        "activate-gpio" => Some(Request::ActivateGpio),
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
/// direction or drive-mode change), then puts back the level it found. It needs no typed
/// confirmation: this is the test that confirmation is the outcome of. It uses the kernel's
/// GPIO driver, not the ports, so the driver's own locking applies.
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
    const USAGE: &str = "usage: rynasctl qnap power-recovery read|last_state|power_on|power_off|activate-gpio\n       rynasctl qnap led-test <led> <seconds>";
    let root = Path::new("/");
    match args {
        [cmd, word] if cmd == "power-recovery" => {
            let request = parse_request(word).ok_or(USAGE)?;
            // held to the end of this process: rynasd is either stopped or paused for it
            let _lock = take_poll_lock(Path::new(POLL_LOCK))?;
            let state = power_recovery(
                root,
                Path::new(STATE_DIR),
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
    use std::collections::HashMap;
    use std::rc::Rc;

    #[derive(Default)]
    struct Chip {
        index: u8,
        entered: u8,
        ldn: u8,
        regs: HashMap<(u8, u8), u8>,
        ids: [u8; 4],
        log: Vec<(u16, u8)>,
        /// After this register is read, another party selects LDN 6.
        steal_after_read: Option<u8>,
        /// Writes to this register do not stick.
        stuck: Option<u8>,
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
            if c.index == 0x07 {
                c.ldn = v;
            } else if c.stuck != Some(c.index) {
                let k = (c.ldn, c.index);
                c.regs.insert(k, v);
            }
        }

        fn inb(&mut self, _port: u16) -> u8 {
            let mut c = self.0.borrow_mut();
            let index = c.index;
            let v = match index {
                0x07 => c.ldn,
                0x20 => c.ids[0],
                0x21 => c.ids[1],
                0x23 => c.ids[2],
                0x24 => c.ids[3],
                _ => c.regs.get(&(c.ldn, index)).copied().unwrap_or(0),
            };
            if c.steal_after_read == Some(index) {
                c.ldn = 6;
            }
            v
        }
    }

    #[derive(Default)]
    struct FakeCmosInner {
        byte: u8,
        writes: Vec<u8>,
        fail_first_write: bool,
    }

    struct FakeCmos(Rc<RefCell<FakeCmosInner>>);

    impl Cmos for FakeCmos {
        fn read(&mut self, offset: u8) -> Result<u8, String> {
            assert_eq!(offset, 0x61);
            Ok(self.0.borrow().byte)
        }

        fn write(&mut self, _offset: u8, v: u8) -> Result<(), String> {
            let mut c = self.0.borrow_mut();
            if std::mem::take(&mut c.fail_first_write) {
                return Err("nvmem".into());
            }
            c.byte = v;
            c.writes.push(v);
            Ok(())
        }
    }

    fn chip(f4: u8) -> Rc<RefCell<Chip>> {
        let mut regs = HashMap::new();
        regs.insert((0x0A, 0xF4), f4);
        Rc::new(RefCell::new(Chip { regs, ids: [0x10, 0x07, 0x19, 0x34], ..Chip::default() }))
    }

    fn cmos(byte: u8) -> Rc<RefCell<FakeCmosInner>> {
        Rc::new(RefCell::new(FakeCmosInner { byte, ..FakeCmosInner::default() }))
    }

    fn f4(c: &Rc<RefCell<Chip>>) -> u8 {
        c.borrow().regs[&(0x0A, 0xF4)]
    }

    /// The fake machine, and a state directory holding the user's confirmation.
    fn hs251() -> (tempfile::TempDir, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        fake_hs251(d.path(), Some("f71869a"));
        let st = tempfile::tempdir().unwrap();
        control::write_confirmation(d.path(), st.path(), &model::MODELS[0]).unwrap();
        (d, st)
    }

    fn run(
        root: &Path,
        state: &Path,
        request: Request,
        c: &Rc<RefCell<Chip>>,
        m: &Rc<RefCell<FakeCmosInner>>,
    ) -> Result<PowerRecoveryState, String> {
        let (cc, mm) = (c.clone(), m.clone());
        power_recovery(
            root,
            state,
            &mut fake_gpio(),
            request,
            move |_| Ok(Box::new(FakePorts(cc)) as Box<dyn Ports>),
            move |_| Ok(Box::new(FakeCmos(mm)) as Box<dyn Cmos>),
        )
    }

    #[test]
    fn the_vendor_sequence_and_only_bits_2_1_change() {
        let (d, st) = hs251();
        let (c, m) = (chip(0xFF), cmos(0x3F));
        let s = run(d.path(), st.path(), Request::Set(PowerRecovery::LastState), &c, &m).unwrap();
        assert_eq!(f4(&c), 0xF9, "other bits kept");
        assert_eq!(s.mode, Some(PowerRecovery::LastState));
        {
            let chip = c.borrow();
            let log = &chip.log;
            assert_eq!(&log[..2], &[(0x2E, 0x87), (0x2E, 0x87)]);
            assert_eq!(*log.last().unwrap(), (0x2E, 0xAA));
            assert!(log.windows(2).any(|w| w == [(0x2E, 0x07), (0x2F, 0x0A)]), "logical device 0x0A");
            assert_eq!(chip.entered, 0, "left configuration mode");
        }
        let (c, m) = (chip(0x00), cmos(0x3F));
        let s = run(d.path(), st.path(), Request::Set(PowerRecovery::PowerOn), &c, &m).unwrap();
        assert_eq!((f4(&c), s.mode), (0x04, Some(PowerRecovery::PowerOn)));
        let (c, m) = (chip(0x00), cmos(0x3F));
        run(d.path(), st.path(), Request::Set(PowerRecovery::PowerOff), &c, &m).unwrap();
        assert_eq!(f4(&c), 0x06);
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
        let (d, st) = hs251();
        let (c, m) = (chip(0b100), cmos(0x21));
        let s = run(d.path(), st.path(), Request::Read, &c, &m).unwrap();
        assert_eq!((s.mode, s.sio_bits, s.eup, s.gpio_active), (Some(PowerRecovery::PowerOn), 0b10, false, false));
        assert_eq!(f4(&c), 0b100);
        assert!(m.borrow().writes.is_empty());
        assert_eq!(c.borrow().log.iter().filter(|(p, _)| *p == 0x2F).count(), 2, "only the two LDN selections");
        // a value without a meaning in the chip: the BIOS's copy decides, like the vendor's
        let (c, m) = (chip(0b010), cmos(0xE1));
        assert_eq!(run(d.path(), st.path(), Request::Read, &c, &m).unwrap().mode, Some(PowerRecovery::PowerOff));
    }

    #[test]
    fn no_port_without_the_users_confirmation() {
        let d = tempfile::tempdir().unwrap();
        fake_hs251(d.path(), Some("f71869a"));
        let empty_state = tempfile::tempdir().unwrap();
        let opened = RefCell::new(0);
        let attempt = |state: &Path| {
            power_recovery(
                d.path(),
                state,
                &mut fake_gpio(),
                Request::Read,
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
        assert!(attempt(empty_state.path()).unwrap_err().contains("not confirmed"));
        // confirmed on another machine: its MACs do not match
        control::write_confirmation(d.path(), empty_state.path(), &model::MODELS[0]).unwrap();
        std::fs::write(d.path().join("sys/class/net/eth1/address"), "00:08:9b:99:99:99\n").unwrap();
        assert!(attempt(empty_state.path()).unwrap_err().contains("not confirmed"));
        assert_eq!(*opened.borrow(), 0);
    }

    #[test]
    fn the_ports_are_never_opened_on_other_hardware() {
        let opened = RefCell::new(0);
        let state = tempfile::tempdir().unwrap();
        let attempt = |root: &Path, gpio: &mut dyn Gpio| {
            power_recovery(
                root,
                state.path(),
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
        let (extra, _st) = hs251();
        std::fs::create_dir_all(extra.path().join("sys/bus/pci/devices/0000:05:00.0")).unwrap();
        assert!(attempt(extra.path(), &mut fake_gpio()).is_err());
        assert_eq!(*opened.borrow(), 0, "not one port, not the CMOS");
    }

    #[test]
    fn another_chip_on_the_port_is_left_alone() {
        let (d, st) = hs251();
        let (c, m) = (chip(0x00), cmos(0x21));
        c.borrow_mut().ids = [0x09, 0x09, 0x19, 0x34];
        let e = run(d.path(), st.path(), Request::Set(PowerRecovery::PowerOn), &c, &m).unwrap_err();
        assert!(e.contains("not touched"), "{e}");
        assert_eq!(f4(&c), 0x00);
        assert!(m.borrow().writes.is_empty());
        assert_eq!(*c.borrow().log.last().unwrap(), (0x2E, 0xAA), "still leaves configuration mode");
    }

    #[test]
    fn no_cmos_means_nothing_is_written() {
        let (d, st) = hs251();
        let c = chip(0x00);
        let cc = c.clone();
        let r = power_recovery(
            d.path(),
            st.path(),
            &mut fake_gpio(),
            Request::Set(PowerRecovery::PowerOn),
            move |_| Ok(Box::new(FakePorts(cc)) as Box<dyn Ports>),
            |_| Err("the kernel exposes no cmos_nvram".to_string()),
        );
        assert!(r.is_err());
        assert!(c.borrow().log.is_empty());
    }

    #[test]
    fn a_logical_device_changed_by_someone_else_stops_the_write() {
        let (d, st) = hs251();
        let (c, m) = (chip(0x00), cmos(0x21));
        // the reader of 0xF4 runs in `read_state` too: let that pass, steal during the write
        let before = run(d.path(), st.path(), Request::Read, &c, &m).unwrap();
        assert_eq!(before.sio_bits, 0);
        c.borrow_mut().steal_after_read = Some(0xF4);
        // the first read of 0xF4 (in the state check) already moves it away
        let e = run(d.path(), st.path(), Request::Set(PowerRecovery::PowerOn), &c, &m).unwrap_err();
        assert!(e.contains("logical device"), "{e}");
        assert_eq!(f4(&c), 0x00, "nothing written to the wrong device or the right one");
        assert!(!c.borrow().regs.contains_key(&(6, 0xF4)), "the GPIO device's 0xF4 untouched");
        assert!(m.borrow().writes.is_empty());
        assert_eq!(*c.borrow().log.last().unwrap(), (0x2E, 0xAA));
    }

    #[test]
    fn the_whole_register_is_compared_and_a_failed_copy_is_undone() {
        let (d, st) = hs251();
        // the register does not take the value: reported, CMOS untouched
        let (c, m) = (chip(0x08), cmos(0x21));
        c.borrow_mut().stuck = Some(0xF4);
        let e = run(d.path(), st.path(), Request::Set(PowerRecovery::PowerOn), &c, &m).unwrap_err();
        assert!(e.contains("expected"), "{e}");
        assert!(m.borrow().writes.is_empty());
        // the CMOS copy fails: the chip goes back to what it held
        let (c, m) = (chip(0x08), cmos(0x21));
        m.borrow_mut().fail_first_write = true;
        let e = run(d.path(), st.path(), Request::Set(PowerRecovery::PowerOn), &c, &m).unwrap_err();
        assert!(e.contains("chip restored"), "{e}");
        assert_eq!(f4(&c), 0x08);
    }

    #[test]
    fn eup_on_refuses_the_write_as_the_vendor_interface_does() {
        let (d, st) = hs251();
        let (c, m) = (chip(0x00), cmos(0x21));
        c.borrow_mut().regs.insert((0x0A, 0xE0), 0x80);
        let s = run(d.path(), st.path(), Request::Read, &c, &m).unwrap();
        assert!(s.eup);
        let e = run(d.path(), st.path(), Request::Set(PowerRecovery::PowerOn), &c, &m).unwrap_err();
        assert!(e.contains("EuP"), "{e}");
        assert_eq!(f4(&c), 0x00);
        // the other reading of the vendor: CMOS not initialized, register 0xE1
        assert!(eup_on(0, 0x0C, 0x3F) && !eup_on(0x80, 0x00, 0x3F) && eup_on(0x80, 0, 0x21));
    }

    #[test]
    fn activation_of_the_gpio_device() {
        let (d, st) = hs251();
        let (c, m) = (chip(0x00), cmos(0x21));
        let s = run(d.path(), st.path(), Request::ActivateGpio, &c, &m).unwrap();
        assert!(s.gpio_active);
        assert_eq!(c.borrow().regs[&(6, 0x30)], 1);
        // already active: not written again
        let (c, m) = (chip(0x00), cmos(0x21));
        c.borrow_mut().regs.insert((6, 0x30), 1);
        let writes_before = c.borrow().log.len();
        run(d.path(), st.path(), Request::ActivateGpio, &c, &m).unwrap();
        assert!(c.borrow().log.len() > writes_before);
        assert!(!c.borrow().log.windows(2).any(|w| w == [(0x2E, 0x30), (0x2F, 1)]));
    }

    #[test]
    fn a_held_lock_refuses_the_worker() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("run/lock");
        let held = take_poll_lock(&path).unwrap();
        assert!(take_poll_lock(&path).unwrap_err().contains("polling"));
        drop(held);
        assert!(take_poll_lock(&path).is_ok());
    }

    #[test]
    fn led_test_lights_and_puts_back() {
        let (d, _st) = hs251();
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
        assert_eq!(parse_request("activate-gpio"), Some(Request::ActivateGpio));
        assert_eq!(parse_request("nope"), None);
        assert_ne!(worker_main(&["nope".to_string()]), 0);
    }
}
