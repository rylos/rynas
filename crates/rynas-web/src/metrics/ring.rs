//! The one-hour metrics history in a compact form (guidelines M4, review M14).
//!
//! A `MetricsSample` holds three `Vec`s of structs with their own `String` names: about 30
//! heap allocations and 1–1.5 KB per sample, 4–5 MB for the 3 600 samples of an hour. A
//! [`Packed`] sample keeps the numbers in two boxed slices and shares the names in a
//! [`Series`] table, rebuilt only when the set of interfaces, disks or sensors changes.
//!
//! Size per sample: 104 B inline in the ring + `4 × (threads + disks + temps)` B +
//! `8 × (3 × nets + 2 × disks)` B + 2 allocator headers. A DS920+ (4 threads, 3 interfaces,
//! 8 block devices, 8 sensors) needs 384 B + ~32 B of headers, i.e. **~1.5 MB for the full
//! hour**, in 2 allocations per sample; see the `packed_size` test.

use std::sync::Arc;

use rynas_api_types::{DiskRate, MemUsage, MetricsSample, NetRate, Temperature};

/// Names of the series of a sample, shared by every sample with the same devices.
#[derive(Debug, PartialEq, Default)]
pub struct Series {
    pub threads: usize,
    pub net: Vec<String>,
    pub disks: Vec<String>,
    /// (id, label)
    pub temps: Vec<(String, String)>,
}

impl Series {
    fn of(s: &MetricsSample) -> Self {
        Self {
            threads: s.cpu_per_thread.len(),
            net: s.net.iter().map(|n| n.name.clone()).collect(),
            disks: s.disks.iter().map(|d| d.name.clone()).collect(),
            temps: s
                .temps
                .iter()
                .map(|t| (t.id.clone(), t.label.clone()))
                .collect(),
        }
    }

    fn matches(&self, s: &MetricsSample) -> bool {
        self.threads == s.cpu_per_thread.len()
            && self.net.len() == s.net.len()
            && self.disks.len() == s.disks.len()
            && self.temps.len() == s.temps.len()
            && self.net.iter().zip(&s.net).all(|(a, b)| *a == b.name)
            && self.disks.iter().zip(&s.disks).all(|(a, b)| *a == b.name)
            && self
                .temps
                .iter()
                .zip(&s.temps)
                .all(|((id, label), t)| *id == t.id && *label == t.label)
    }
}

/// `NetRate::up` and `speed_mbps` in one word: bit 32 = up, bit 33 = speed known.
fn net_meta(n: &NetRate) -> u64 {
    let mut m = u64::from(n.up) << 32;
    if let Some(s) = n.speed_mbps {
        m |= 1 << 33 | u64::from(s);
    }
    m
}

/// One sample: fixed fields inline, the per-device numbers in two slices.
#[derive(Debug)]
pub struct Packed {
    ts_ms: u64,
    cpu_pct: f32,
    load: [f32; 3],
    mem: MemUsage,
    series: Arc<Series>,
    /// cpu per thread, then disk busy %, then temperatures (NaN = no reading)
    f: Box<[f32]>,
    /// per interface rx, tx, meta; then per disk read, write
    u: Box<[u64]>,
}

impl Packed {
    /// Packs `s`, reusing `prev` when the series are the same (the common case).
    pub fn pack(s: &MetricsSample, prev: Option<&Arc<Series>>) -> Self {
        let series = match prev {
            Some(p) if p.matches(s) => p.clone(),
            _ => Arc::new(Series::of(s)),
        };
        let f: Box<[f32]> = s
            .cpu_per_thread
            .iter()
            .copied()
            .chain(s.disks.iter().map(|d| d.busy_pct))
            .chain(s.temps.iter().map(|t| t.celsius.unwrap_or(f32::NAN)))
            .collect();
        let u: Box<[u64]> = s
            .net
            .iter()
            .flat_map(|n| [n.rx_bps, n.tx_bps, net_meta(n)])
            .chain(s.disks.iter().flat_map(|d| [d.read_bps, d.write_bps]))
            .collect();
        Self {
            ts_ms: s.ts_ms,
            cpu_pct: s.cpu_pct,
            load: s.load,
            mem: s.mem,
            series,
            f,
            u,
        }
    }

    pub fn series(&self) -> &Arc<Series> {
        &self.series
    }

    pub fn unpack(&self) -> MetricsSample {
        let sr = &*self.series;
        let (cpu, rest) = self.f.split_at(sr.threads);
        let (busy, temps) = rest.split_at(sr.disks.len());
        let (net, disk) = self.u.split_at(sr.net.len() * 3);
        MetricsSample {
            ts_ms: self.ts_ms,
            cpu_pct: self.cpu_pct,
            cpu_per_thread: cpu.to_vec(),
            load: self.load,
            mem: self.mem,
            net: sr
                .net
                .iter()
                .zip(net.as_chunks::<3>().0)
                .map(|(name, v)| NetRate {
                    name: name.clone(),
                    rx_bps: v[0],
                    tx_bps: v[1],
                    up: v[2] & (1 << 32) != 0,
                    speed_mbps: (v[2] & (1 << 33) != 0).then_some(v[2] as u32),
                })
                .collect(),
            disks: sr
                .disks
                .iter()
                .zip(disk.as_chunks::<2>().0.iter().zip(busy))
                .map(|(name, (v, &b))| DiskRate {
                    name: name.clone(),
                    read_bps: v[0],
                    write_bps: v[1],
                    busy_pct: b,
                })
                .collect(),
            temps: sr
                .temps
                .iter()
                .zip(temps)
                .map(|((id, label), &c)| Temperature {
                    id: id.clone(),
                    label: label.clone(),
                    celsius: (!c.is_nan()).then_some(c),
                })
                .collect(),
            // live only: the store puts the newest devices on the newest history point
            zram: Vec::new(),
        }
    }

    /// Heap and inline bytes of this sample, without the shared series table.
    pub fn bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.f.len() * 4 + self.u.len() * 8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nas_sample(i: u64) -> MetricsSample {
        MetricsSample {
            ts_ms: i,
            cpu_pct: 12.5,
            cpu_per_thread: vec![1.0, 2.0, 3.0, 4.0],
            load: [0.1, 0.2, 0.3],
            mem: MemUsage {
                total: 4 << 30,
                used: 1 << 30,
                ..Default::default()
            },
            net: ["bond0", "eth0", "eth1"]
                .iter()
                .map(|n| NetRate {
                    name: n.to_string(),
                    rx_bps: 125_000_000 + i,
                    tx_bps: 7,
                    up: *n != "eth1",
                    speed_mbps: (*n != "bond0").then_some(1000),
                })
                .collect(),
            disks: [
                "md0", "md2", "nvme0n1", "nvme1n1", "sda", "sdb", "sdc", "sdd",
            ]
            .iter()
            .map(|d| DiskRate {
                name: d.to_string(),
                read_bps: 5_000_000_000,
                write_bps: 1,
                busy_pct: 33.3,
            })
            .collect(),
            temps: [
                "cpu", "nvme0", "nvme1", "bay1", "bay2", "bay3", "bay4", "nic:eth0",
            ]
            .iter()
            .enumerate()
            .map(|(k, t)| Temperature {
                id: t.to_string(),
                label: t.to_uppercase(),
                celsius: (k != 3).then_some(40.5),
            })
            .collect(),
            zram: Vec::new(),
        }
    }

    #[test]
    fn round_trip_is_exact() {
        let s = nas_sample(1);
        let p = Packed::pack(&s, None);
        assert_eq!(p.unpack(), s);
        let empty = MetricsSample::default();
        assert_eq!(Packed::pack(&empty, None).unpack(), empty);
    }

    #[test]
    fn series_are_shared_until_devices_change() {
        let a = Packed::pack(&nas_sample(1), None);
        let b = Packed::pack(&nas_sample(2), Some(a.series()));
        assert!(Arc::ptr_eq(a.series(), b.series()));
        let mut s = nas_sample(3);
        s.net.pop();
        let c = Packed::pack(&s, Some(b.series()));
        assert!(!Arc::ptr_eq(b.series(), c.series()));
        assert_eq!(c.unpack(), s);
    }

    #[test]
    fn packed_size() {
        let p = Packed::pack(&nas_sample(1), None);
        // 104 inline + 20 f32 + 25 u64 = 384 B, + 2 malloc headers ≈ 400 B
        assert_eq!(std::mem::size_of::<Packed>(), 104);
        assert_eq!(p.bytes(), 104 + 20 * 4 + 25 * 8);
        // the full hour stays under 2 MB (with allocator overhead)
        assert!((p.bytes() + 32) * crate::metrics::RING_CAPACITY < 2 << 20);
    }
}
