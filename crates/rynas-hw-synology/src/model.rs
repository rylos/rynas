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

/// Models synohwd knows (DMI `product_name`).
pub const KNOWN_MODELS: [&str; 3] = ["DS920+", "DS923+", "DS1522+"];

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

fn tables(product: &str, mode: FanMode) -> &'static Tables {
    match (product, mode) {
        ("DS920+", FanMode::High) => &DS920_HIGH,
        ("DS920+", _) => &DS920_LOW,
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

/// A Synology running synodeb: the platform driver is bound (it binds only on supported
/// DMI) and DMI names Synology or a known model.
pub fn detect(root: &Path) -> Option<String> {
    platform_dir(root)?;
    let (vendor, product) = rynas_hw::dmi(root);
    let synology = vendor.to_ascii_lowercase().contains("synology")
        || KNOWN_MODELS.contains(&product.as_str());
    synology.then_some(product)
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
    }
}
