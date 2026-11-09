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
    /// The line mapping was tested on this model's hardware.
    pub verified: bool,
    pub bios_vendor: &'static str,
    pub sys_vendor: &'static str,
    pub product_name: &'static str,
    pub board_vendor: &'static str,
    pub board_name: &'static str,
    /// Text the CPU's model name must contain.
    pub cpu: &'static str,
    pub pci: &'static [PciDevice],
    /// PCI addresses of the NICs whose MAC starts with [`Self::mac_oui`].
    pub nics: &'static [&'static str],
    pub mac_oui: &'static str,
    /// `name` of the chip's hwmon (driver f71882fg).
    pub hwmon_chip: &'static str,
    /// Lines of each gpio-f7188x bank, in bank order.
    pub gpio_banks: &'static [u32],
    /// PCI address of the disks' AHCI controller.
    pub ahci: &'static str,
    pub bays: &'static [BayMap],
    pub status_green: Line,
    pub status_red: Line,
    pub usb_led: Line,
    pub reset_button: Line,
    pub usb_copy_button: Line,
    /// Buzzer tone, in Hz.
    pub beep_hz: u32,
}

/// The models the module knows (data, one entry per model).
pub const MODELS: &[Model] = &[Model {
    id: "hs-251",
    display: "QNAP HS-251",
    verified: false,
    bios_vendor: "American Megatrends Inc.",
    sys_vendor: "To be filled by O.E.M.",
    product_name: "To be filled by O.E.M.",
    board_vendor: "AMI Corporation",
    board_name: "Aptio CRB",
    cpu: "J1800",
    pci: &[
        PciDevice { address: "0000:00:13.0", id: "8086:0f23" },
        PciDevice { address: "0000:00:14.0", id: "8086:0f35" },
        PciDevice { address: "0000:00:1c.2", id: "8086:0f4c" },
        PciDevice { address: "0000:00:1c.3", id: "8086:0f4e" },
        PciDevice { address: "0000:03:00.0", id: "8086:1533" },
        PciDevice { address: "0000:04:00.0", id: "8086:1533" },
    ],
    nics: &["0000:03:00.0", "0000:04:00.0"],
    mac_oui: "00:08:9b",
    hwmon_chip: "f71869a",
    gpio_banks: &[6, 8, 8, 8, 8, 5, 8, 8],
    ahci: "0000:00:13.0",
    bays: &[
        BayMap { bay: 1, ahci_port: 1, err_led: low(7, 0) },
        BayMap { bay: 2, ahci_port: 0, err_led: low(7, 1) },
    ],
    status_green: low(6, 2),
    status_red: low(6, 3),
    usb_led: low(1, 7),
    reset_button: low(6, 1),
    usb_copy_button: low(1, 2),
    beep_hz: 800,
}];

/// The value of a file below `root`, whitespace trimmed; empty if unreadable.
pub fn read(root: &Path, path: &str) -> String {
    std::fs::read_to_string(rynas_hw::under(root, path))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn dmi(root: &Path, f: &str) -> String {
    read(root, &format!("/sys/class/dmi/id/{f}"))
}

fn pci_id(root: &Path, address: &str) -> String {
    let attr = |f: &str| read(root, &format!("/sys/bus/pci/devices/{address}/{f}"));
    let id = |v: String| v.strip_prefix("0x").unwrap_or("").to_string();
    format!("{}:{}", id(attr("vendor")), id(attr("device")))
}

fn nic_mac(root: &Path, address: &str) -> Option<String> {
    let net = rynas_hw::under(root, &format!("/sys/bus/pci/devices/{address}/net"));
    let iface = std::fs::read_dir(net).ok()?.flatten().next()?;
    Some(read(root, &format!("/sys/class/net/{}/address", iface.file_name().to_string_lossy())))
}

/// The model whose **whole** signature matches this machine; `None` otherwise.
pub fn identify(root: &Path) -> Option<&'static Model> {
    let cpuinfo = read(root, "/proc/cpuinfo");
    MODELS.iter().find(|m| {
        dmi(root, "bios_vendor") == m.bios_vendor
            && dmi(root, "sys_vendor") == m.sys_vendor
            && dmi(root, "product_name") == m.product_name
            && dmi(root, "board_vendor") == m.board_vendor
            && dmi(root, "board_name") == m.board_name
            && cpuinfo.lines().any(|l| l.starts_with("model name") && l.contains(m.cpu))
            && m.pci.iter().all(|d| pci_id(root, d.address) == d.id)
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

/// The chip named by the f71882fg hwmon driver, against the model.
pub fn chip(root: &Path, m: &Model) -> Chip {
    let mut names: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(rynas_hw::under(root, "/sys/class/hwmon")) {
        for e in rd.flatten() {
            names.push(read(root, &format!("/sys/class/hwmon/{}/name", e.file_name().to_string_lossy())));
        }
    }
    if names.iter().any(|n| n == m.hwmon_chip) {
        return Chip::Confirmed;
    }
    // another Fintek chip was found by the driver: not the one the mapping is about
    if names.iter().any(|n| FINTEK_HWMON.contains(&n.as_str())) {
        return Chip::Mismatch;
    }
    Chip::Unknown
}

/// `f71882fg_names[]` of the driver.
const FINTEK_HWMON: &[&str] = &[
    "f71808e", "f71808a", "f71858fg", "f71862fg", "f71868a", "f71869", "f71869a", "f71882fg",
    "f71889ed", "f71889a", "f8000", "f81768d", "f81865f", "f81866a",
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
            let (v, i) = d.id.split_once(':').unwrap();
            w(root, &format!("sys/bus/pci/devices/{}/vendor", d.address), &format!("0x{v}\n"));
            w(root, &format!("sys/bus/pci/devices/{}/device", d.address), &format!("0x{i}\n"));
        }
        for (i, n) in MODELS[0].nics.iter().enumerate() {
            let dir = root.join(format!("sys/bus/pci/devices/{n}/net/eth{i}"));
            std::fs::create_dir_all(dir).unwrap();
            w(root, &format!("sys/class/net/eth{i}/address"), &format!("00:08:9b:aa:bb:0{i}\n"));
        }
        if let Some(h) = hwmon {
            w(root, "sys/class/hwmon/hwmon1/name", &format!("{h}\n"));
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
        assert!(!m.verified, "the mapping was never tested on the hardware");
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
        // every line exists in its bank (F71869A: bank 6 has 8 lines, plain F71869 only 6)
        for l in [m.status_green, m.status_red, m.usb_led, m.reset_button, m.usb_copy_button] {
            assert!(l.offset < m.gpio_banks[l.bank as usize]);
        }
    }
}
