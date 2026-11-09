//! The read-only web provider (`rynas_hw::HardwareProvider`, rynas-web, not root) and the map
//! from a disk to its bay: `/sys/block/<disk>/device` resolves to the AHCI controller and the
//! `ataN` port; `/sys/class/ata_port/ataN/port_no` is that port's 1-based number.

use std::path::Path;

use rynas_api_types::{Bay, HardwareStatus};
use rynas_hw::HardwareProvider;

use crate::model::{self, Chip, Model};

/// Bay of the whole disk `kname` (`sda`); `None` for a disk not on the bays' controller.
pub fn bay_of(root: &Path, m: &Model, kname: &str) -> Option<u32> {
    let dev = std::fs::canonicalize(rynas_hw::under(root, &format!("/sys/block/{kname}/device"))).ok()?;
    let path = dev.to_str()?;
    if !path.split('/').any(|c| c == m.ahci) {
        return None;
    }
    let ata = path.split('/').find(|c| {
        c.strip_prefix("ata").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })?;
    let port: u32 = model::read(root, &format!("/sys/class/ata_port/{ata}/port_no")).parse().ok()?;
    m.bays.iter().find(|b| b.ahci_port + 1 == port).map(|b| b.bay)
}

/// The disks in the bays: `(bay, kernel name)`.
pub fn disks_in_bays(root: &Path, m: &Model) -> Vec<(u32, String)> {
    let mut out: Vec<(u32, String)> = std::fs::read_dir(rynas_hw::under(root, "/sys/block"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("sd"))
        .filter_map(|n| bay_of(root, m, &n).map(|b| (b, n)))
        .collect();
    out.sort();
    out
}

pub struct QnapProvider {
    root: std::path::PathBuf,
    model: &'static Model,
}

impl QnapProvider {
    /// The provider, only on an identified machine whose chip the driver confirmed.
    pub fn detect(root: &Path) -> Option<Self> {
        let model = model::identify(root)?;
        (model::chip(root, model) == Chip::Confirmed).then(|| Self { root: root.to_path_buf(), model })
    }
}

impl HardwareProvider for QnapProvider {
    fn status(&self) -> Result<Option<HardwareStatus>, String> {
        let found = disks_in_bays(&self.root, self.model);
        let bays = self
            .model
            .bays
            .iter()
            .map(|b| {
                let device = found.iter().find(|(n, _)| *n == b.bay).map(|(_, d)| d.clone());
                Bay {
                    number: b.bay,
                    present: device.is_some(),
                    powered: device.is_some(),
                    device,
                    celsius: None,
                    asleep: false,
                }
            })
            .collect();
        Ok(Some(HardwareStatus {
            provider: Some("qnap".into()),
            model: self.model.display.into(),
            bays,
            ..HardwareStatus::default()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MODELS, testing::fake_hs251};

    fn disk(root: &Path, name: &str, ata: u32, port_no: u32) {
        let dev = format!("sys/devices/pci0000:00/0000:00:13.0/ata{ata}/host0/target0:0:0/0:0:0:0");
        std::fs::create_dir_all(root.join(&dev)).unwrap();
        std::fs::create_dir_all(root.join(format!("sys/block/{name}"))).unwrap();
        std::os::unix::fs::symlink(root.join(&dev), root.join(format!("sys/block/{name}/device"))).unwrap();
        let p = root.join(format!("sys/class/ata_port/ata{ata}"));
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("port_no"), format!("{port_no}\n")).unwrap();
    }

    #[test]
    fn bays_follow_the_ahci_ports() {
        let d = tempfile::tempdir().unwrap();
        // the vendor's file: bay 1 is AHCI port 1, bay 2 port 0
        disk(d.path(), "sda", 1, 1);
        disk(d.path(), "sdb", 2, 2);
        assert_eq!(disks_in_bays(d.path(), &MODELS[0]), vec![(1, "sdb".to_string()), (2, "sda".to_string())]);
        // a USB disk is on no bay
        std::fs::create_dir_all(d.path().join("sys/block/sdc")).unwrap();
        assert_eq!(bay_of(d.path(), &MODELS[0], "sdc"), None);
    }

    #[test]
    fn provider_only_with_the_chip_confirmed() {
        let d = tempfile::tempdir().unwrap();
        fake_hs251(d.path(), None);
        assert!(QnapProvider::detect(d.path()).is_none());
        fake_hs251(d.path(), Some("f71869a"));
        let p = QnapProvider::detect(d.path()).unwrap();
        let s = p.status().unwrap().unwrap();
        assert_eq!(s.provider.as_deref(), Some("qnap"));
        assert_eq!(s.bays.len(), 2);
        assert!(s.fans.is_empty() && !s.bays[0].present);
    }
}
