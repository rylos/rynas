//! The model table and strict identification. Adding a model is adding a [`Model`] here: every
//! field of its signature must be read from the machine, none guessed
//! (`docs/plugins/qnap.md`, "Adding a model").

use std::path::Path;

/// A device expected at a fixed PCI address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciDevice {
    /// `0000:00:13.0`.
    pub address: &'static str,
    /// `vendor:device` as in sysfs without `0x`: `8086:0f23`.
    pub id: &'static str,
}

/// Where the Super I/O keeps the power-recovery setting, with the BIOS's copy in CMOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerRecoveryMap {
    /// Index port of the Super I/O (data is the next port).
    pub sio_port: u16,
    /// `(device id, manufacturer id)` read back from the chip before anything is written.
    pub sio_ids: (u16, u16),
    pub ldn: u8,
    pub reg: u8,
    /// Logical device of the GPIO banks (the vendor activates it before using it).
    pub gpio_ldn: u8,
    /// CMOS byte holding the copy in its two high bits.
    pub cmos_offset: u8,
}

/// A line of a bank of the Super I/O: chip `gpio-f7188x-<bank>`, line `offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Line {
    pub bank: u8,
    pub offset: u32,
    /// Logical on = electrical low.
    pub active_low: bool,
}

const fn low(bank: u8, offset: u32) -> Line {
    Line { bank, offset, active_low: true }
}

/// A line written as a raw level (the brightness lines).
const fn raw(bank: u8, offset: u32) -> Line {
    Line { bank, offset, active_low: false }
}

/// The gpio-f7188x register block of a bank (`f71869a_gpio_bank[]` in the driver): the
/// vendor's `SIO:I<reg>:B<bit>` is `regbase + 1` (data out) or `regbase + 2` (pin status).
pub const fn bank_regbase(bank: u8) -> u8 {
    0xF0 - bank * 0x10
}

/// One bay: the AHCI port behind it and its error LED.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BayMap {
    pub bay: u32,
    /// `DEV_PORT` of the vendor's model file: 0-based port of the AHCI controller.
    pub ahci_port: u32,
    pub err_led: Line,
}

/// One entry of the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Model {
    /// `hs-251`.
    pub id: &'static str,
    /// `QNAP HS-251`.
    pub display: &'static str,
    /// `HS-251`: what the user types to confirm the mapping.
    pub short: &'static str,
    /// The line mapping was tested on this model's hardware.
    pub verified: bool,
    pub bios_vendor: &'static str,
    pub sys_vendor: &'static str,
    pub product_name: &'static str,
    pub board_vendor: &'static str,
    pub board_name: &'static str,
    /// Text the CPU's model name must contain.
    pub cpu: &'static str,
    /// **Every** PCI device of the machine: one more or one less is another machine.
    pub pci: &'static [PciDevice],
    /// PCI addresses of the NICs whose MAC starts with [`Self::mac_oui`].
    pub nics: &'static [&'static str],
    pub mac_oui: &'static str,
    /// USB `(vendor, product)` of the internal boot flash ("DOM") holding the vendor's
    /// firmware: part of the machine, never a removable drive (HS-251: an Apacer module,
    /// `USB DISK MODULE`, read on the box on 2026-11-10).
    pub boot_flash_usb: &'static [(&'static str, &'static str)],
    /// `name` of the chip's hwmon (driver f71882fg).
    pub hwmon_chip: &'static str,
    /// No fan: every tachometer of the chip must read 0 (a fan turning is another model).
    pub fanless: bool,
    /// Lines of each gpio-f7188x bank, in bank order.
    pub gpio_banks: &'static [u32],
    /// PCI address of the disks' AHCI controller.
    pub ahci: &'static str,
    pub bays: &'static [BayMap],
    pub status_green: Line,
    pub status_red: Line,
    pub usb_led: Line,
    /// The bays' error LEDs and the USB LED exist on the front panel. The vendor's model file
    /// names their lines on every sibling, but some cases have no such LED (HS-251: only the
    /// status and LAN LEDs, per the specification and the first-boot test of 2026-11-10);
    /// lines of a missing LED are never written.
    pub disk_leds_fitted: bool,
    pub usb_led_fitted: bool,
    /// The front USB copy button exists (HS-251: no, and the BIOS leaves its line an output,
    /// seen on 2026-11-10): a missing button's line is never read.
    pub usb_copy_button_fitted: bool,
    pub reset_button: Line,
    pub usb_copy_button: Line,
    /// Buzzer tone, in Hz.
    pub beep_hz: u32,
    /// The four brightness lines of the front LEDs (the vendor's register 0xA1).
    pub brightness: [Line; 4],
    /// `None`: the model has no known way to set it.
    pub power_recovery: Option<PowerRecoveryMap>,
    /// hwmon input of the board sensor (`SYSTEM_TEMP_UNIT` of the vendor's model file).
    pub system_temp: u32,
}

/// The models the module knows (data, one entry per model).
pub const MODELS: &[Model] = &[Model {
    id: "hs-251",
    display: "QNAP HS-251",
    short: "HS-251",
    // first-boot procedure run on a real HS-251 on 2026-11-10 (docs/plugins/qnap.md)
    verified: true,
    bios_vendor: "American Megatrends Inc.",
    sys_vendor: "To be filled by O.E.M.",
    product_name: "To be filled by O.E.M.",
    board_vendor: "AMI Corporation",
    board_name: "Aptio CRB",
    cpu: "J1800",
    pci: &[
        dev("0000:00:00.0", "8086:0f00"),
        dev("0000:00:02.0", "8086:0f31"),
        dev("0000:00:11.0", "8086:0f15"),
        dev("0000:00:12.0", "8086:0f16"),
        dev("0000:00:13.0", "8086:0f23"),
        dev("0000:00:14.0", "8086:0f35"),
        dev("0000:00:17.0", "8086:0f50"),
        dev("0000:00:18.0", "8086:0f40"),
        dev("0000:00:18.1", "8086:0f41"),
        dev("0000:00:18.2", "8086:0f42"),
        dev("0000:00:18.3", "8086:0f43"),
        dev("0000:00:18.4", "8086:0f44"),
        dev("0000:00:18.5", "8086:0f45"),
        dev("0000:00:18.6", "8086:0f46"),
        dev("0000:00:18.7", "8086:0f47"),
        dev("0000:00:1a.0", "8086:0f18"),
        dev("0000:00:1b.0", "8086:0f04"),
        dev("0000:00:1c.0", "8086:0f48"),
        dev("0000:00:1c.1", "8086:0f4a"),
        dev("0000:00:1c.2", "8086:0f4c"),
        dev("0000:00:1c.3", "8086:0f4e"),
        dev("0000:00:1e.0", "8086:0f06"),
        dev("0000:00:1e.1", "8086:0f08"),
        dev("0000:00:1e.2", "8086:0f09"),
        dev("0000:00:1e.3", "8086:0f0a"),
        dev("0000:00:1e.4", "8086:0f0c"),
        dev("0000:00:1e.5", "8086:0f0e"),
        dev("0000:00:1f.0", "8086:0f1c"),
        dev("0000:00:1f.3", "8086:0f12"),
        dev("0000:03:00.0", "8086:1533"),
        dev("0000:04:00.0", "8086:1533"),
    ],
    nics: &["0000:03:00.0", "0000:04:00.0"],
    mac_oui: "00:08:9b",
    boot_flash_usb: &[("1005", "b155")],
    hwmon_chip: "f71869a",
    fanless: true,
    gpio_banks: &[6, 8, 8, 8, 8, 5, 8, 8],
    ahci: "0000:00:13.0",
    bays: &[
        BayMap { bay: 1, ahci_port: 1, err_led: low(7, 0) },
        BayMap { bay: 2, ahci_port: 0, err_led: low(7, 1) },
    ],
    status_green: low(6, 2),
    status_red: low(6, 3),
    usb_led: low(1, 7),
    disk_leds_fitted: false,
    usb_led_fitted: false,
    usb_copy_button_fitted: false,
    reset_button: low(6, 1),
    usb_copy_button: low(1, 2),
    beep_hz: 800,
    brightness: [raw(5, 0), raw(5, 1), raw(5, 2), raw(5, 3)],
    power_recovery: Some(PowerRecoveryMap {
        sio_port: 0x2E,
        sio_ids: (0x1007, 0x1934),
        ldn: 0x0A,
        reg: 0xF4,
        gpio_ldn: 0x06,
        cmos_offset: 0x61,
    }),
    system_temp: 3,
}];

const fn dev(address: &'static str, id: &'static str) -> PciDevice {
    PciDevice { address, id }
}

/// The value of a file below `root`, whitespace trimmed; empty if unreadable.
pub fn read(root: &Path, path: &str) -> String {
    std::fs::read_to_string(rynas_hw::under(root, path))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn dmi(root: &Path, f: &str) -> String {
    read(root, &format!("/sys/class/dmi/id/{f}"))
}

/// `vendor:device` of a PCI device.
fn pci_id(root: &Path, address: &str) -> String {
    let attr = |f: &str| read(root, &format!("/sys/bus/pci/devices/{address}/{f}"));
    let id = |v: String| v.strip_prefix("0x").unwrap_or("").to_string();
    format!("{}:{}", id(attr("vendor")), id(attr("device")))
}

/// The machine has exactly the model's PCI devices: nothing missing, nothing extra.
fn pci_exact(root: &Path, m: &Model) -> bool {
    let found: Vec<String> = std::fs::read_dir(rynas_hw::under(root, "/sys/bus/pci/devices"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    found.len() == m.pci.len()
        && m.pci.iter().all(|d| {
            found.iter().any(|f| f == d.address) && pci_id(root, d.address) == d.id
        })
}

/// The disks' controller has as many ATA ports as the model has bays.
fn ata_ports(root: &Path, m: &Model) -> usize {
    std::fs::read_dir(rynas_hw::under(root, &format!("/sys/bus/pci/devices/{}", m.ahci)))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.strip_prefix("ata").is_some_and(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit())))
        .count()
}

pub(crate) fn nic_mac(root: &Path, address: &str) -> Option<String> {
    let net = rynas_hw::under(root, &format!("/sys/bus/pci/devices/{address}/net"));
    let iface = std::fs::read_dir(net).ok()?.flatten().next()?;
    Some(read(root, &format!("/sys/class/net/{}/address", iface.file_name().to_string_lossy())))
}

/// The model whose firmware strings and CPU match: nothing that appears late at boot. Enough
/// to wait for the rest of the signature, never enough to touch anything.
pub fn plausible(root: &Path) -> Option<&'static Model> {
    let cpuinfo = read(root, "/proc/cpuinfo");
    MODELS.iter().find(|m| {
        dmi(root, "bios_vendor") == m.bios_vendor
            && dmi(root, "sys_vendor") == m.sys_vendor
            && dmi(root, "product_name") == m.product_name
            && dmi(root, "board_vendor") == m.board_vendor
            && dmi(root, "board_name") == m.board_name
            && cpuinfo.lines().any(|l| l.starts_with("model name") && l.contains(m.cpu))
    })
}

/// The model whose **whole** signature matches this machine; `None` otherwise: the firmware
/// strings and CPU, exactly its PCI devices, as many ATA ports as bays, the NICs' prefix.
///
/// What this excludes and what it cannot: a machine with another device set or another bay
/// count is rejected, so a 4-bay sibling is. A two-bay sibling with the same CPU and the same
/// PCI devices would pass here; only its fan makes a difference, checked in [`chip`]. Nothing
/// readable from Linux names the model outright (the name is in the board's I²C EEPROM), so
/// a model not verified on hardware also needs the user's own confirmation before any LED
/// or Super I/O write.
pub fn identify(root: &Path) -> Option<&'static Model> {
    plausible(root).filter(|m| {
        pci_exact(root, m)
            && ata_ports(root, m) == m.bays.len()
            && m.nics
                .iter()
                .all(|n| nic_mac(root, n).is_some_and(|mac| mac.to_ascii_lowercase().starts_with(m.mac_oui)))
    })
}

/// What the Super I/O's own driver says about the chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chip {
    /// The hwmon driver is not loaded (yet).
    Unknown,
    /// The driver names the model's chip.
    Confirmed,
    /// The driver names another chip: this is not the model, the module must stay out.
    Mismatch,
}

/// The chip named by the f71882fg hwmon driver, against the model. A fanless model with a
/// fan turning is another model.
pub fn chip(root: &Path, m: &Model) -> Chip {
    let attr = |dir: &Path, name: &str| std::fs::read_to_string(dir.join(name)).map(|s| s.trim().to_string()).unwrap_or_default();
    let mut other_fintek = false;
    for e in std::fs::read_dir(rynas_hw::under(root, "/sys/class/hwmon")).into_iter().flatten().flatten() {
        let dir = attr_dir(&e.path());
        let name = attr(&dir, "name");
        if name == m.hwmon_chip {
            let turning = m.fanless
                && (1..=4).any(|n| attr(&dir, &format!("fan{n}_input")).parse::<u32>().is_ok_and(|rpm| rpm > 0));
            return if turning { Chip::Mismatch } else { Chip::Confirmed };
        }
        other_fintek |= FINTEK_HWMON.contains(&name.as_str());
    }
    if other_fintek { Chip::Mismatch } else { Chip::Unknown }
}

/// Where a hwmon device keeps its attributes: the `hwmonN` directory itself, or its parent
/// device for drivers that still register the old way (f71882fg on the HS-251: `name`,
/// `fan*_input` and `temp*_input` are in `hwmonN/device`).
pub fn attr_dir(hwmon: &Path) -> std::path::PathBuf {
    if hwmon.join("name").exists() { hwmon.to_path_buf() } else { hwmon.join("device") }
}

/// `f71882fg_names[]` of the driver.
const FINTEK_HWMON: &[&str] = &[
    "f71808e", "f71808a", "f71858fg", "f71862fg", "f71868a", "f71869", "f71869a", "f71882fg",
    "f71889fg", "f71889ed", "f71889a", "f8000", "f81768d", "f81865f", "f81866a",
];

#[cfg(test)]
pub mod testing {
    use super::*;

    fn w(root: &Path, rel: &str, v: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, v).unwrap();
    }

    /// A fake HS-251 below `root`; the chip's driver loaded when `hwmon` names it.
    pub fn fake_hs251(root: &Path, hwmon: Option<&str>) {
        let dmi = "sys/class/dmi/id";
        w(root, &format!("{dmi}/bios_vendor"), "American Megatrends Inc.\n");
        w(root, &format!("{dmi}/sys_vendor"), "To be filled by O.E.M.\n");
        w(root, &format!("{dmi}/product_name"), "To be filled by O.E.M.\n");
        w(root, &format!("{dmi}/board_vendor"), "AMI Corporation\n");
        w(root, &format!("{dmi}/board_name"), "Aptio CRB\n");
        w(root, "proc/cpuinfo", "processor\t: 0\nmodel name\t: Intel(R) Celeron(R)  CPU  J1800  @ 2.41GHz\n");
        for d in MODELS[0].pci {
            let at = |f: &str| format!("sys/bus/pci/devices/{}/{f}", d.address);
            let (v, i) = d.id.split_once(':').unwrap();
            w(root, &at("vendor"), &format!("0x{v}\n"));
            w(root, &at("device"), &format!("0x{i}\n"));
        }
        for a in ["ata1", "ata2"] {
            std::fs::create_dir_all(root.join(format!("sys/bus/pci/devices/0000:00:13.0/{a}"))).unwrap();
        }
        for (i, n) in MODELS[0].nics.iter().enumerate() {
            let dir = root.join(format!("sys/bus/pci/devices/{n}/net/eth{i}"));
            std::fs::create_dir_all(dir).unwrap();
            w(root, &format!("sys/class/net/eth{i}/address"), &format!("00:08:9b:aa:bb:0{i}\n"));
        }
        if let Some(h) = hwmon {
            w(root, "sys/class/hwmon/hwmon1/name", &format!("{h}\n"));
            w(root, "sys/class/hwmon/hwmon1/fan1_input", "0\n");
            w(root, "sys/class/hwmon/hwmon1/temp3_input", "39000\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::testing::fake_hs251;

    #[test]
    fn the_whole_signature_identifies() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(identify(d.path()), None, "an empty machine");
        fake_hs251(d.path(), Some("f71869a"));
        let m = identify(d.path()).unwrap();
        assert_eq!(m.id, "hs-251");
        assert!(m.verified, "the first-boot procedure was run on an HS-251");
        assert_eq!(chip(d.path(), m), Chip::Confirmed);
    }

    #[test]
    fn one_different_field_is_no_match() {
        let change = |rel: &str, v: &str| {
            let d = tempfile::tempdir().unwrap();
            fake_hs251(d.path(), Some("f71869a"));
            std::fs::write(d.path().join(rel), v).unwrap();
            assert_eq!(identify(d.path()), None, "{rel}");
        };
        change("sys/class/dmi/id/board_name", "Other\n");
        change("sys/class/dmi/id/sys_vendor", "QNAP\n");
        change("proc/cpuinfo", "model name\t: Intel(R) Celeron(R) CPU J1900\n");
        change("sys/bus/pci/devices/0000:03:00.0/device", "0x1539\n");
        change("sys/class/net/eth1/address", "00:11:22:33:44:55\n");
    }

    #[test]
    fn sibling_shapes_are_rejected() {
        // one extra PCI device (an added card, a different board)
        let d = tempfile::tempdir().unwrap();
        fake_hs251(d.path(), Some("f71869a"));
        std::fs::create_dir_all(d.path().join("sys/bus/pci/devices/0000:05:00.0")).unwrap();
        assert_eq!(identify(d.path()), None, "extra device");
        // one PCI device missing
        let d = tempfile::tempdir().unwrap();
        fake_hs251(d.path(), Some("f71869a"));
        std::fs::remove_dir_all(d.path().join("sys/bus/pci/devices/0000:04:00.0")).unwrap();
        assert_eq!(identify(d.path()), None, "missing device");
        // four bays
        let d = tempfile::tempdir().unwrap();
        fake_hs251(d.path(), Some("f71869a"));
        for a in ["ata3", "ata4"] {
            std::fs::create_dir_all(d.path().join(format!("sys/bus/pci/devices/0000:00:13.0/{a}"))).unwrap();
        }
        assert_eq!(identify(d.path()), None, "four bays");
        assert!(plausible(d.path()).is_some(), "the firmware strings alone are not identification");
        // a fan turning on a fanless model
        let d = tempfile::tempdir().unwrap();
        fake_hs251(d.path(), Some("f71869a"));
        let m = identify(d.path()).unwrap();
        std::fs::write(d.path().join("sys/class/hwmon/hwmon1/fan1_input"), "1100\n").unwrap();
        assert_eq!(chip(d.path(), m), Chip::Mismatch);
    }

    #[test]
    fn the_driver_decides_the_chip() {
        let d = tempfile::tempdir().unwrap();
        fake_hs251(d.path(), None);
        let m = identify(d.path()).unwrap();
        assert_eq!(chip(d.path(), m), Chip::Unknown, "driver not loaded");
        std::fs::create_dir_all(d.path().join("sys/class/hwmon/hwmon1")).unwrap();
        std::fs::write(d.path().join("sys/class/hwmon/hwmon1/name"), "f71869\n").unwrap();
        assert_eq!(chip(d.path(), m), Chip::Mismatch, "plain F71869: another bank layout");
        std::fs::write(d.path().join("sys/class/hwmon/hwmon1/name"), "nct6775\n").unwrap();
        assert_eq!(chip(d.path(), m), Chip::Unknown);
    }

    /// As on a real HS-251 (Debian 13, kernel 6.12): f71882fg registers its hwmon the old
    /// way, so the attributes are in `hwmonN/device` and `hwmonN` has no `name`.
    #[test]
    fn attributes_on_the_parent_device() {
        let d = tempfile::tempdir().unwrap();
        fake_hs251(d.path(), None);
        let m = identify(d.path()).unwrap();
        let dev = d.path().join("sys/class/hwmon/hwmon4/device");
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(dev.join("name"), "f71869a\n").unwrap();
        std::fs::write(dev.join("fan1_input"), "0\n").unwrap();
        assert_eq!(chip(d.path(), m), Chip::Confirmed);
        std::fs::write(dev.join("fan1_input"), "900\n").unwrap();
        assert_eq!(chip(d.path(), m), Chip::Mismatch, "a fan turning on a fanless model");
    }

    /// The vendor's registers against the banks of the driver: `SIO:I91:B2` is bank 6
    /// (regbase 0x90), data register (+1), line 2.
    #[test]
    fn vendor_registers_are_the_driver_banks() {
        let m = &MODELS[0];
        let reg = |l: Line, off: u8| bank_regbase(l.bank) + off;
        assert_eq!((reg(m.status_green, 1), m.status_green.offset), (0x91, 2));
        assert_eq!((reg(m.status_red, 1), m.status_red.offset), (0x91, 3));
        assert_eq!((reg(m.reset_button, 2), m.reset_button.offset), (0x92, 1));
        assert_eq!((reg(m.usb_led, 1), m.usb_led.offset), (0xE1, 7));
        assert_eq!((reg(m.usb_copy_button, 2), m.usb_copy_button.offset), (0xE2, 2));
        assert_eq!((reg(m.bays[0].err_led, 1), m.bays[0].err_led.offset), (0x81, 0));
        assert_eq!((reg(m.bays[1].err_led, 1), m.bays[1].err_led.offset), (0x81, 1));
        // the vendor's brightness register 0xA1, bits 0-3
        for (i, l) in m.brightness.iter().enumerate() {
            assert_eq!((reg(*l, 1), l.offset), (0xA1, i as u32));
        }
        // every line exists in its bank (F71869A: bank 6 has 8 lines, plain F71869 only 6)
        for l in [m.status_green, m.status_red, m.usb_led, m.reset_button, m.usb_copy_button, m.brightness[3]] {
            assert!(l.offset < m.gpio_banks[l.bank as usize]);
        }
    }
}
