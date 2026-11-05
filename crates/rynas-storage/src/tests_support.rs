//! The fixture NAS as an [`Inventory`], built from the parsers alone.
//!
//! - `sda`, `sdb`, `sdc`: 8 TB HDDs, md2 RAID5 (recovering onto sdc1) → LUKS → btrfs "vol1"
//! - `sdd`: 4 TB SMR HDD, blank, in standby
//! - `sde`: 2 TB HDD with a locked LUKS partition
//! - `nvme0n1`, `nvme1n1`: system, md0 RAID1 → btrfs "root"

use std::collections::HashMap;

use rynas_api_types::storage::PowerState;

use crate::Inventory;
use crate::btrfs;
use crate::inventory::{self, FsParts, SmartEntry};
use crate::lsblk::BlockGraph;
use crate::md;
use crate::mounts::parse_mountinfo;
use crate::smart::{self, SmartRead};

fn smart_entry(json: &str, serial: &str) -> SmartEntry {
    match smart::parse(json, 1_793_836_800_000) {
        SmartRead::Data(d) => SmartEntry {
            data: *d,
            serial: serial.into(),
        },
        other => panic!("{other:?}"),
    }
}

pub fn nas_inventory() -> Inventory {
    let graph = BlockGraph::parse(include_str!("../tests/fixtures/lsblk-nas.json")).unwrap();
    let hdd = include_str!("../tests/fixtures/smartctl-sata-hdd.json");
    let nvme = include_str!("../tests/fixtures/smartctl-nvme.json");
    let now = 1_793_836_860_000;
    let disks = graph
        .disks()
        .into_iter()
        .map(|d| {
            let (smart, power) = match d.kname.as_str() {
                "sdd" => (None, PowerState::Standby),
                n if n.starts_with("nvme") => (Some(smart_entry(nvme, "")), PowerState::Active),
                _ => (Some(smart_entry(hdd, "")), PowerState::Active),
            };
            let zoned = false;
            inventory::summary(d, &graph, smart.as_ref(), power, now, zoned)
        })
        .collect();

    let stat = md::parse_mdstat(include_str!("../tests/fixtures/proc-mdstat.txt"));
    let export = md::parse_export(include_str!(
        "../tests/fixtures/mdadm-detail-export-md2.txt"
    ));
    let detail = md::parse_detail(include_str!("../tests/fixtures/mdadm-detail-md2.txt"));
    let disk_of = |dev: &str| graph.disks_under(dev).into_iter().next();
    let arrays = stat
        .iter()
        .filter(|s| s.name == "md2" || s.name == "md0")
        .map(|s| {
            let mut sys = md::SysMd::default();
            if s.name == "md2" {
                sys.degraded = Some(1);
                sys.members
                    .insert("sda1".into(), ("in_sync".into(), Some(0), Some(0)));
                sys.members
                    .insert("sdb1".into(), ("in_sync".into(), Some(1), Some(0)));
                sys.members
                    .insert("sdc1".into(), ("spare".into(), Some(2), Some(0)));
                md::build(s, &sys, Some(&export), Some(&detail), false, &disk_of)
            } else {
                md::build(s, &sys, None, None, false, &disk_of)
            }
        })
        .collect();

    let mounts = parse_mountinfo(include_str!("../tests/fixtures/proc-self-mountinfo.txt"));
    let shows = btrfs::parse_show(include_str!("../tests/fixtures/btrfs-filesystem-show.txt"));
    let usage = btrfs::parse_usage(include_str!("../tests/fixtures/btrfs-usage-vol1.txt"));
    let stats: HashMap<_, _> = btrfs::parse_device_stats(include_str!(
        "../tests/fixtures/btrfs-device-stats-vol1.txt"
    ));
    let mut filesystems = Vec::new();
    for sh in shows.iter().take(2) {
        let sources: Vec<String> = sh.devices.iter().map(|d| d.path.clone()).collect();
        let vol1 = sh.label.as_deref() == Some("vol1");
        let parts = FsParts {
            show: Some(sh),
            usage: vol1.then_some(&usage),
            stats: vol1.then_some(&stats),
            subvolumes: if vol1 {
                btrfs::parse_subvolume_list(include_str!(
                    "../tests/fixtures/btrfs-subvolume-list-vol1.txt"
                ))
            } else {
                Vec::new()
            },
            readonly: if vol1 {
                btrfs::readonly_ids(include_str!(
                    "../tests/fixtures/btrfs-subvolume-list-ro-vol1.txt"
                ))
            } else {
                Default::default()
            },
            scrub: None,
            quota: Default::default(),
        };
        filesystems.push(inventory::build_filesystem(
            &sh.uuid,
            sh.label.clone(),
            &sources,
            &mounts,
            parts,
            now,
        ));
    }
    Inventory {
        graph,
        disks,
        arrays,
        filesystems,
        mounts,
    }
}
