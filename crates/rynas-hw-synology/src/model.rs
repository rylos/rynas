//! Model data, copied from synohwd.py (synodeb): DSM's fan curves (`FAN_TABLES`,
//! `MODEL_FAN_TABLES`) and the bay count of each model (`MODEL_BAYS`). Like synohwd, any
//! other product gets the DS923+ data.

use std::path::Path;

use rynas_api_types::synology::{FanCurve, FanMode, FanModeCurves, FanPoint, SensorClass};

/// Directories of the synodeb platform driver: `synology-nas`, `synology-ds923` in the
/// kernels before 6.18.60-7 / 7.3.1-7.
pub const PLATFORM_DIRS: [&str; 2] = [
    "/sys/devices/platform/synology-nas",
    "/sys/devices/platform/synology-ds923",
];

/// Models synohwd knows (the keys of its `MODEL_BAYS`): the names its `model` may take and
/// the models this module has data for (fan curves of their own or the DS923+ ones, bays).
pub const KNOWN_MODELS: [&str; 5] = ["DS415+", "DS423+", "DS920+", "DS923+", "DS1522+"];

type Table = &'static [(u8, u8)];

/// `(disk, disk_shutdown, nvme, nvme_shutdown, cpu, cpu_shutdown, nic, nic_shutdown)`.
struct Tables {
    disk: (Table, u8),
    nvme: (Table, u8),
    cpu: (Table, u8),
    nic: (Table, u8),
}

const DS923_HIGH: Tables = Tables {
    disk: (&[(0, 50), (41, 60), (46, 70), (50, 80), (53, 99)], 61),
    nvme: (&[(0, 50), (44, 60), (52, 70), (60, 99)], 70),
    cpu: (&[(0, 50), (68, 70), (78, 99)], 95),
    nic: (&[(0, 50), (72, 70), (82, 99)], 100),
};
const DS923_LOW: Tables = Tables {
    disk: (&[(0, 50), (46, 60), (52, 70), (55, 80), (58, 99)], 61),
    nvme: (&[(0, 50), (48, 60), (56, 70), (64, 99)], 70),
    cpu: (&[(0, 50), (78, 70), (86, 99)], 95),
    nic: (&[(0, 50), (78, 70), (88, 99)], 100),
};
// DS920+ (scemd.xml of DSM 7.4.1); no NIC sensor, "nic" is the DS923+ one
const DS920_HIGH: Tables = Tables {
    disk: (&[(0, 20), (41, 30), (46, 50), (51, 70), (53, 99)], 61),
    nvme: (&[(0, 20), (51, 50), (60, 99)], 70),
    cpu: (&[(0, 20), (60, 50), (70, 99)], 90),
    nic: DS923_HIGH.nic,
};
const DS920_LOW: Tables = Tables {
    disk: (&[(0, 20), (46, 30), (51, 50), (56, 70), (58, 99)], 61),
    nvme: (&[(0, 20), (56, 50), (65, 99)], 70),
    cpu: (&[(0, 20), (65, 50), (75, 99)], 90),
    nic: DS923_LOW.nic,
};

// DS423+ (scemd.xml of DSM 7.4.1); no NIC sensor, "nic" is the DS923+ one
const DS423_HIGH: Tables = Tables {
    disk: (&[(0, 20), (41, 30), (46, 50), (50, 70), (53, 99)], 61),
    nvme: (&[(0, 20), (55, 50), (60, 70), (65, 99)], 70),
    cpu: (&[(0, 20), (45, 30), (55, 50), (65, 70), (75, 99)], 90),
    nic: DS923_HIGH.nic,
};
const DS423_LOW: Tables = Tables {
    disk: (&[(0, 20), (46, 30), (52, 50), (55, 70), (58, 99)], 61),
    nvme: (&[(0, 20), (55, 50), (60, 70), (65, 99)], 70),
    cpu: (&[(0, 20), (50, 30), (60, 50), (70, 70), (80, 99)], 90),
    nic: DS923_LOW.nic,
};

fn tables(product: &str, mode: FanMode) -> &'static Tables {
    match (product, mode) {
        ("DS920+", FanMode::High) => &DS920_HIGH,
        ("DS920+", _) => &DS920_LOW,
        ("DS423+", FanMode::High) => &DS423_HIGH,
        ("DS423+", _) => &DS423_LOW,
        (_, FanMode::High) => &DS923_HIGH,
        (_, _) => &DS923_LOW,
    }
}

fn curve(class: SensorClass, (t, shutdown): (Table, u8)) -> FanCurve {
    FanCurve {
        class,
        points: t
            .iter()
            .map(|&(threshold_c, duty_pct)| FanPoint {
                threshold_c,
                duty_pct,
            })
            .collect(),
        shutdown_c: shutdown,
    }
}

/// The curves of one mode (`full` uses the `low` thresholds for shutdown and runs at 99 %).
pub fn mode_curves(product: &str, mode: FanMode) -> FanModeCurves {
    let t = tables(product, mode);
    FanModeCurves {
        mode,
        curves: vec![
            curve(SensorClass::Disk, t.disk),
            curve(SensorClass::Nvme, t.nvme),
            curve(SensorClass::Cpu, t.cpu),
            curve(SensorClass::Nic, t.nic),
        ],
    }
}

/// Shutdown threshold of a sensor class (the same in both modes on every model).
pub fn shutdown_c(product: &str, class: SensorClass) -> u8 {
    let t = tables(product, FanMode::Low);
    match class {
        SensorClass::Disk => t.disk.1,
        SensorClass::Nvme => t.nvme.1,
        SensorClass::Cpu => t.cpu.1,
        SensorClass::Nic => t.nic.1,
    }
}

/// The duty synohwd's tables give for `temp` without hysteresis (`Fan.step` from a cold
/// start): the highest step whose threshold is reached.
pub fn duty_for(points: &[FanPoint], temp: f32) -> u8 {
    points
        .iter()
        .filter(|p| temp >= p.threshold_c as f32)
        .map(|p| p.duty_pct)
        .next_back()
        .unwrap_or(0)
}

/// Number of bays (`MODEL_BAYS`).
pub fn bay_count(product: &str) -> u32 {
    match product {
        "DS1522+" => 5,
        _ => 4,
    }
}

/// Fans: every hwmon `synology` exposes two tachometers.
pub const FANS: u32 = 2;

/// The platform driver directory below `root`, if the driver is bound.
pub fn platform_dir(root: &Path) -> Option<std::path::PathBuf> {
    PLATFORM_DIRS
        .iter()
        .map(|d| rynas_hw::under(root, d))
        .find(|p| p.is_dir())
}

/// synodeb's board file: `board MODEL` prints the model it tells by the PCI devices, and
/// exits 1 on a NAS it does not know.
pub const BOARD: &str = "/usr/lib/synodeb/board";

/// Longest wait for the board file (synohwd waits as long).
const BOARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The model synohwd names in its status file (`model`, newer synodeb releases: the DMI name, or the
/// MODEL of the board file where the DMI name is a placeholder). `None` without the file
/// (synohwd not running), without the field (an older synohwd) or with an empty one.
pub fn synohwd_model(root: &Path) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Model {
        #[serde(default)]
        model: Option<String>,
    }
    let text = std::fs::read_to_string(rynas_hw::under(root, crate::status::STATUS)).ok()?;
    let m: Model = serde_json::from_str(&text).ok()?;
    m.model.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// `board MODEL` below `root` (before synohwd writes its first status, at boot): the model on
/// exit 0, `None` when the file is missing, fails (an unknown NAS) or hangs.
pub fn board_model(root: &Path) -> Option<String> {
    use std::process::{Command, Stdio};
    let board = rynas_hw::under(root, BOARD);
    if !board.is_file() {
        return None;
    }
    let mut child = Command::new(&board)
        .arg("MODEL")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
                let m = out.trim().to_string();
                return (status.success() && !m.is_empty()).then_some(m);
            }
            Ok(None) if start.elapsed() < BOARD_TIMEOUT => std::thread::sleep(std::time::Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// A Synology running synodeb: the platform driver is bound (it binds only on supported
/// DMI), and DMI names Synology or a known model. Where the DMI name is a placeholder
/// (Avoton boards: `Insyde` / `MohonPeak`), the model synohwd names (its status file, else
/// synodeb's board file) when it is a known one: its name is the product the tables use.
/// Never a model without the platform driver.
pub fn detect(root: &Path) -> Option<String> {
    platform_dir(root)?;
    let (vendor, product) = rynas_hw::dmi(root);
    if vendor.to_ascii_lowercase().contains("synology") || KNOWN_MODELS.contains(&product.as_str()) {
        return Some(product);
    }
    synohwd_model(root)
        .filter(|m| KNOWN_MODELS.contains(&m.as_str()))
        .or_else(|| board_model(root).filter(|m| KNOWN_MODELS.contains(&m.as_str())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_follow_synohwd() {
        let l = mode_curves("DS923+", FanMode::Low);
        assert_eq!(l.curves[0].class, SensorClass::Disk);
        assert_eq!(l.curves[0].points.len(), 5);
        assert_eq!(l.curves[0].shutdown_c, 61);
        assert_eq!(l.curves[2].shutdown_c, 95);
        let h920 = mode_curves("DS920+", FanMode::High);
        assert_eq!(h920.curves[2].shutdown_c, 90);
        assert_eq!(
            h920.curves[1].points[1],
            FanPoint {
                threshold_c: 51,
                duty_pct: 50
            }
        );
        // unknown models get the DS923+ tables, like synohwd
        assert_eq!(mode_curves("RS422+", FanMode::Low), l);
        assert_eq!(shutdown_c("DS920+", SensorClass::Cpu), 90);
        assert_eq!(shutdown_c("DS923+", SensorClass::Nvme), 70);
        assert_eq!(bay_count("DS1522+"), 5);
        assert_eq!(bay_count("DS920+"), 4);
    }

    #[test]
    fn duty_from_a_cold_start() {
        // synohwd's hysteresis test starts at 40 °C on the quiet disk table: 50 %, 47 °C: 60 %
        let p = &mode_curves("DS923+", FanMode::Low).curves[0].points;
        assert_eq!(duty_for(p, 40.0), 50);
        assert_eq!(duty_for(p, 47.0), 60);
        assert_eq!(duty_for(p, 59.0), 99);
    }

    #[test]
    fn detection() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(detect(d.path()), None);
        std::fs::create_dir_all(d.path().join("sys/devices/platform/synology-nas")).unwrap();
        assert_eq!(detect(d.path()), None, "no DMI");
        let id = d.path().join("sys/class/dmi/id");
        std::fs::create_dir_all(&id).unwrap();
        std::fs::write(id.join("sys_vendor"), "Synology Inc.\n").unwrap();
        std::fs::write(id.join("product_name"), "DS920+\n").unwrap();
        assert_eq!(detect(d.path()).as_deref(), Some("DS920+"));
        // a known product with a placeholder vendor; a status file naming another model
        // changes nothing on this path
        std::fs::write(id.join("sys_vendor"), "To Be Filled By O.E.M.\n").unwrap();
        status_model(d.path(), r#""model": "DS423+","#);
        assert_eq!(detect(d.path()).as_deref(), Some("DS920+"));
    }

    fn status_model(root: &Path, field: &str) {
        let f = root.join("run/synohwd/status.json");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, format!(r#"{{{field} "epoch": 1762250000, "sensors": {{}}}}"#)).unwrap();
    }

    /// Avoton boards (DS415+, DS2415+) publish DMI `Insyde` / `MohonPeak`: the model comes
    /// from synohwd's status file, else synodeb's board file, and only with the platform
    /// driver bound and a model this module knows.
    #[test]
    fn detection_with_a_placeholder_dmi() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        let id = r.join("sys/class/dmi/id");
        std::fs::create_dir_all(&id).unwrap();
        std::fs::write(id.join("sys_vendor"), "Insyde\n").unwrap();
        std::fs::write(id.join("product_name"), "MohonPeak\n").unwrap();
        status_model(r, r#""model": "DS415+","#);
        assert_eq!(detect(r), None, "no platform driver: never a model");
        std::fs::create_dir_all(r.join("sys/devices/platform/synology-nas")).unwrap();
        assert_eq!(detect(r).as_deref(), Some("DS415+"));
        let p = crate::provider::Synology::detect(r).unwrap();
        assert_eq!((p.product(), p.model(), p.bays()), ("DS415+", "Synology DS415+", 4));
        // synohwd names the placeholder (a board file that does not know the NAS), or a model
        // without data here, or an older synohwd without the field: not detected
        for field in [r#""model": "MohonPeak","#, r#""model": "DS2415+","#, r#""model": "","#, ""] {
            status_model(r, field);
            assert_eq!(detect(r), None, "{field}");
        }
        std::fs::remove_file(r.join("run/synohwd/status.json")).unwrap();
        assert_eq!(detect(r), None, "synohwd not running, no board file");
        // before synohwd's first status: the board file
        let board = r.join("usr/lib/synodeb/board");
        std::fs::create_dir_all(board.parent().unwrap()).unwrap();
        std::fs::write(&board, "#!/bin/sh\n[ \"$1\" = MODEL ] && echo DS415+\n").unwrap();
        std::fs::set_permissions(&board, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(detect(r).as_deref(), Some("DS415+"));
        // an unknown NAS: the board file exits 1
        std::fs::write(&board, "#!/bin/sh\necho unknown >&2\nexit 1\n").unwrap();
        assert_eq!(detect(r), None);
        // the DS423+ has fan curves of its own (synohwd's MODEL_FAN_TABLES)
        assert_eq!(shutdown_c("DS423+", SensorClass::Cpu), 90);
        assert_eq!(mode_curves("DS423+", FanMode::Low).curves[2].points.len(), 5);
        assert_ne!(mode_curves("DS423+", FanMode::High), mode_curves("DS923+", FanMode::High));
    }
}
