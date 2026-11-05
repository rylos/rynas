//! Demo data, used ONLY when the API does not answer or the URL has `?demo`.
//! Compiled with the `demo` cargo feature; the app bar shows a "Demo data" badge meanwhile.

use std::cell::Cell;
use std::time::Duration;

use leptos::prelude::*;
use rynas_api_types::*;

use crate::live::{Conn, HISTORY_CAP, Live};

const GIB: u64 = 1 << 30;
const TB: u64 = 1_000_000_000_000;

thread_local! {
    static SEED: Cell<u32> = const { Cell::new(0x9e37_79b9) };
}

/// xorshift, 0..1
fn rnd() -> f32 {
    SEED.with(|s| {
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        s.set(x);
        (x % 10_000) as f32 / 10_000.0
    })
}

fn wave(t: f64, period: f64) -> f32 {
    ((t / period * std::f64::consts::TAU).sin() * 0.5 + 0.5) as f32
}

fn sample(ts_ms: u64) -> MetricsSample {
    let t = ts_ms as f64 / 1000.0;
    let base = 8.0 + 22.0 * wave(t, 90.0) + 10.0 * wave(t, 17.0);
    let per: Vec<f32> = (0..4).map(|i| (base + (rnd() - 0.4) * 25.0 + i as f32 * 3.0).clamp(0.5, 100.0)).collect();
    let cpu = per.iter().sum::<f32>() / per.len() as f32;
    let rx = (40.0e6 * wave(t, 45.0).powi(3) + 2.0e5 + rnd() * 3.0e6) as u64;
    let tx = (6.0e6 * wave(t + 20.0, 60.0).powi(2) + 1.0e5 + rnd() * 6.0e5) as u64;
    let rd = (90.0e6 * wave(t, 70.0).powi(4) + rnd() * 2.0e6) as u64;
    let wr = (25.0e6 * wave(t + 30.0, 33.0).powi(6) + rnd() * 5.0e5) as u64;
    MetricsSample {
        ts_ms,
        cpu_pct: cpu,
        cpu_per_thread: per,
        load: [cpu / 25.0, cpu / 30.0, 0.62],
        mem: MemUsage {
            total: 8 * GIB,
            used: (2.6 * GIB as f64 + wave(t, 300.0) as f64 * 0.4 * GIB as f64) as u64,
            cache: 4 * GIB + GIB / 3,
            swap_total: 2 * GIB,
            swap_used: 12 << 20,
        },
        net: vec![
            NetRate { name: "eth0".into(), rx_bps: rx, tx_bps: tx, up: true, speed_mbps: Some(1000) },
            NetRate { name: "eth1".into(), rx_bps: 0, tx_bps: 0, up: false, speed_mbps: None },
        ],
        disks: vec![
            DiskRate { name: "md2".into(), read_bps: rd, write_bps: wr, busy_pct: (rd as f32 / 1.2e6).min(100.0) },
            DiskRate { name: "nvme0n1".into(), read_bps: rd / 9, write_bps: wr / 3, busy_pct: 2.0 + rnd() * 3.0 },
        ],
        temps: vec![
            Temperature { id: "cpu".into(), label: "CPU".into(), celsius: Some(44.0 + cpu / 6.0) },
            Temperature { id: "nvme0".into(), label: "NVMe 0".into(), celsius: Some(41.0) },
        ],
        zram: vec![ZramDevice {
            name: "zram0".into(),
            algorithm: "zstd".into(),
            disksize: 4 * GIB,
            data: 1229 << 20,
            compressed: 352 << 20,
            mem_used: 371 << 20,
            huge_pages: 210,
        }],
    }
}

fn system() -> SystemInfo {
    SystemInfo {
        hostname: "nas01".into(),
        os: "Debian GNU/Linux 13 (trixie)".into(),
        kernel: "6.12.48-amd64".into(),
        model: "Synology Inc. DS923+".into(),
        cpu_model: "AMD Ryzen Embedded R1600".into(),
        cpu_threads: 4,
        mem_total: 8 * GIB,
        uptime_s: 12 * 86400 + 4 * 3600 + 17 * 60,
        rynas_version: env!("CARGO_PKG_VERSION").into(),
    }
}

fn disk(name: &str, model: &str, serial: &str, size: u64, bay: u32, state: storage::PowerState) -> Disk {
    Disk {
        name: name.into(),
        model: model.into(),
        serial: serial.into(),
        size,
        rotational: true,
        transport: "sata".into(),
        bay: Some(bay),
        power_state: state,
    }
}

fn storage() -> StorageOverview {
    StorageOverview {
        disks: vec![
            disk("sda", "WDC WD40EFPX-68C6CN0", "WD-WX12A3456789", 4 * TB, 1, storage::PowerState::Active),
            disk("sdb", "WDC WD40EFPX-68C6CN0", "WD-WX12A3456790", 4 * TB, 2, storage::PowerState::Active),
            disk("sdc", "ST4000VN006-3CW104", "ZW60A1B2", 4 * TB, 3, storage::PowerState::Active),
            disk("sdd", "ST8000VN004-3CP101", "WWZ1C2D3", 8 * TB, 4, storage::PowerState::Standby),
            Disk {
                name: "nvme0n1".into(),
                model: "Samsung SSD 980 500GB".into(),
                serial: "S64DNX0T123456".into(),
                size: 500_107_862_016,
                rotational: false,
                transport: "nvme".into(),
                bay: None,
                power_state: storage::PowerState::Active,
            },
        ],
        arrays: vec![
            RaidArray {
                name: "md2".into(),
                level: "raid5".into(),
                size: 12 * TB - 300 * GIB,
                members: vec!["sda3".into(), "sdb3".into(), "sdc3".into()],
                state: "resyncing".into(),
                sync_pct: Some(42.6),
            },
            RaidArray {
                name: "md0".into(),
                level: "raid1".into(),
                size: 8 * GIB,
                members: vec!["sda1".into(), "sdb1".into(), "sdc1".into(), "sdd1".into()],
                state: "clean".into(),
                sync_pct: None,
            },
        ],
        filesystems: vec![
            Filesystem {
                mountpoint: "/".into(),
                source: "/dev/nvme0n1p2".into(),
                fstype: "ext4".into(),
                size: 460 * GIB,
                used: 38 * GIB,
                avail: 399 * GIB,
            },
            Filesystem {
                mountpoint: "/srv/data".into(),
                source: "/dev/md2".into(),
                fstype: "btrfs".into(),
                size: 10_900 * GIB,
                used: 7_420 * GIB,
                avail: 3_480 * GIB,
            },
            Filesystem {
                mountpoint: "/srv/backup".into(),
                source: "/dev/sdd1".into(),
                fstype: "xfs".into(),
                size: 7_450 * GIB,
                used: 6_980 * GIB,
                avail: 470 * GIB,
            },
        ],
    }
}

fn hardware(t: f64) -> HardwareStatus {
    let bay = |n: u32, dev: &str, c: f32, asleep: bool| Bay {
        number: n,
        present: true,
        powered: true,
        device: Some(dev.into()),
        celsius: Some(c),
        asleep,
    };
    HardwareStatus {
        provider: Some("synology".into()),
        model: "DS923+".into(),
        fans: vec![
            Fan { label: "Fan 1".into(), rpm: Some(1080 + (wave(t, 40.0) * 60.0) as u32), alarm: false },
            Fan { label: "Fan 2".into(), rpm: Some(1110 + (wave(t, 50.0) * 50.0) as u32), alarm: false },
        ],
        bays: vec![bay(1, "sda", 36.0, false), bay(2, "sdb", 37.0, false), bay(3, "sdc", 39.0, false), bay(4, "sdd", 31.0, true)],
        m2: vec![
            M2Slot { number: Some(1), present: true, device: Some("nvme0n1".into()), celsius: Some(41.0) },
            M2Slot { number: Some(2), present: false, device: None, celsius: None },
        ],
        fan_duty: Some(38),
        fan_fail: false,
        status_led: Some("green".into()),
        events: vec![
            ("2026-11-04 08:12:03".into(), "Disk 4 entered standby".into()),
            ("2026-11-04 09:30:41".into(), "Fan mode: quiet".into()),
            ("2026-11-04 11:02:17".into(), "RAID md2 resync started".into()),
        ],
        age_s: Some(2),
    }
}

pub fn start(live: Live) {
    live.conn.set(Conn::Demo);
    live.set_system(system());
    live.storage.set(Some(storage()));
    let now = js_sys::Date::now() as u64 / 1000 * 1000;
    for i in (1..HISTORY_CAP as u64).rev() {
        live.push(sample(now - i * 1000));
    }
    live.hardware.set(Some(hardware(now as f64 / 1000.0)));
    set_interval(
        move || {
            let now = js_sys::Date::now() as u64 / 1000 * 1000;
            live.push(sample(now));
            if now % 5000 < 1000 {
                live.hardware.set(Some(hardware(now as f64 / 1000.0)));
            }
        },
        Duration::from_secs(1),
    );
}
