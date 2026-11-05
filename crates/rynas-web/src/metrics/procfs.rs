//! Pure parsers for the `/proc` files read by the collector. No I/O here: every function
//! takes the file content, so they are tested with textual fixtures.

/// Cumulative CPU time of one line of `/proc/stat`, in USER_HZ ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuTimes {
    pub busy: u64,
    pub total: u64,
}

impl CpuTimes {
    /// Busy share between `prev` and `self`, 0..=100.
    pub fn pct_since(&self, prev: &CpuTimes) -> f32 {
        let total = self.total.saturating_sub(prev.total);
        if total == 0 {
            return 0.0;
        }
        let busy = self.busy.saturating_sub(prev.busy).min(total);
        (busy as f64 * 100.0 / total as f64) as f32
    }
}

fn cpu_times(fields: &str) -> Option<CpuTimes> {
    // user nice system idle iowait irq softirq steal [guest guest_nice]
    // guest time is already counted in user/nice, so only the first 8 make the total.
    let mut v = [0u64; 8];
    let mut n = 0;
    for (slot, tok) in v.iter_mut().zip(fields.split_ascii_whitespace()) {
        *slot = tok.parse().ok()?;
        n += 1;
    }
    if n < 4 {
        return None;
    }
    let total: u64 = v.iter().sum();
    let idle = v[3] + v[4];
    Some(CpuTimes {
        busy: total - idle,
        total,
    })
}

/// `/proc/stat` → (aggregate "cpu" line, per-thread "cpuN" lines in file order).
pub fn parse_stat(s: &str) -> (Option<CpuTimes>, Vec<CpuTimes>) {
    let mut total = None;
    let mut per = Vec::new();
    for line in s.lines() {
        let Some(rest) = line.strip_prefix("cpu") else {
            // the cpu lines come first: stop at the first other line
            if total.is_some() {
                break;
            }
            continue;
        };
        if let Some(fields) = rest.strip_prefix(' ') {
            total = cpu_times(fields);
        } else if let Some((idx, fields)) = rest.split_once(' ')
            && idx.bytes().all(|b| b.is_ascii_digit())
            && let Some(t) = cpu_times(fields)
        {
            per.push(t);
        }
    }
    (total, per)
}

/// `/proc/loadavg` → 1, 5, 15 minute load.
pub fn parse_loadavg(s: &str) -> [f32; 3] {
    let mut out = [0.0; 3];
    for (slot, tok) in out.iter_mut().zip(s.split_ascii_whitespace()) {
        *slot = tok.parse().unwrap_or(0.0);
    }
    out
}

/// The fields of `/proc/meminfo` the collector needs, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemInfo {
    pub total: u64,
    pub available: u64,
    pub cached: u64,
    pub sreclaimable: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

pub fn parse_meminfo(s: &str) -> MemInfo {
    let mut m = MemInfo::default();
    for line in s.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let slot = match key {
            "MemTotal" => &mut m.total,
            "MemAvailable" => &mut m.available,
            "Cached" => &mut m.cached,
            "SReclaimable" => &mut m.sreclaimable,
            "SwapTotal" => &mut m.swap_total,
            "SwapFree" => &mut m.swap_free,
            _ => continue,
        };
        let kb: u64 = rest
            .split_ascii_whitespace()
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        *slot = kb * 1024;
    }
    m
}

/// One interface of `/proc/net/dev`: cumulative bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetCounters {
    pub name: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

pub fn parse_net_dev(s: &str) -> Vec<NetCounters> {
    s.lines()
        .skip(2)
        .filter_map(|line| {
            let (name, rest) = line.split_once(':')?;
            let mut f = rest.split_ascii_whitespace();
            let rx_bytes = f.next()?.parse().ok()?;
            let tx_bytes = f.nth(7)?.parse().ok()?;
            Some(NetCounters {
                name: name.trim().to_string(),
                rx_bytes,
                tx_bytes,
            })
        })
        .collect()
}

/// One device of `/proc/diskstats`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskCounters {
    pub name: String,
    pub sectors_read: u64,
    pub sectors_written: u64,
    /// Milliseconds spent doing I/O.
    pub io_ticks: u64,
}

/// `/proc/diskstats`, whole disks only (see [`is_whole_disk`]).
pub fn parse_diskstats(s: &str) -> Vec<DiskCounters> {
    s.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_ascii_whitespace().collect();
            if f.len() < 13 || !is_whole_disk(f[2]) {
                return None;
            }
            Some(DiskCounters {
                name: f[2].to_string(),
                sectors_read: f[5].parse().ok()?,
                sectors_written: f[9].parse().ok()?,
                io_ticks: f[12].parse().ok()?,
            })
        })
        .collect()
}

pub use rynas_storage::overview::is_physical_disk;

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Whole block devices whose I/O is worth showing: physical disks and md arrays.
/// Partitions, loop, ram, zram, dm and optical drives are left out.
pub fn is_whole_disk(name: &str) -> bool {
    is_physical_disk(name) || name.strip_prefix("md").is_some_and(digits)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = include_str!("../../tests/fixtures/proc-stat.txt");
    const STAT2: &str = include_str!("../../tests/fixtures/proc-stat-2.txt");

    #[test]
    fn stat_and_pct() {
        let (t1, p1) = parse_stat(STAT);
        let (t2, p2) = parse_stat(STAT2);
        assert_eq!(p1.len(), 4);
        assert_eq!(p2.len(), 4);
        let (t1, t2) = (t1.unwrap(), t2.unwrap());
        // total delta 400 ticks, idle+iowait delta 300 → 25 %
        assert!(
            (t2.pct_since(&t1) - 25.0).abs() < 0.01,
            "{}",
            t2.pct_since(&t1)
        );
        // cpu0 fully busy, cpu3 idle
        assert!((p2[0].pct_since(&p1[0]) - 100.0).abs() < 0.01);
        assert_eq!(p2[3].pct_since(&p1[3]), 0.0);
        // counters going backwards (cpu hotplug) do not underflow
        assert_eq!(t1.pct_since(&t2), 0.0);
    }

    #[test]
    fn loadavg() {
        assert_eq!(
            parse_loadavg("0.52 0.58 0.59 2/312 12345\n"),
            [0.52, 0.58, 0.59]
        );
    }

    #[test]
    fn meminfo() {
        let m = parse_meminfo(include_str!("../../tests/fixtures/proc-meminfo.txt"));
        assert_eq!(m.total, 3_869_628 * 1024);
        assert_eq!(m.available, 2_912_532 * 1024);
        assert_eq!(m.cached, 1_402_116 * 1024);
        assert_eq!(m.sreclaimable, 98_472 * 1024);
        assert_eq!(m.swap_total, 1_048_572 * 1024);
        assert_eq!(m.swap_free, 1_047_036 * 1024);
    }

    #[test]
    fn net_dev() {
        let n = parse_net_dev(include_str!("../../tests/fixtures/proc-net-dev.txt"));
        let names: Vec<_> = n.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            ["lo", "eth0", "eth1", "bond0", "docker0", "veth1a2b3c4"]
        );
        assert_eq!(n[1].rx_bytes, 9_876_543_210);
        assert_eq!(n[1].tx_bytes, 1_234_567_890);
    }

    #[test]
    fn diskstats() {
        let d = parse_diskstats(include_str!("../../tests/fixtures/proc-diskstats.txt"));
        let names: Vec<_> = d.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["sda", "sdb", "nvme0n1", "md2"]);
        assert_eq!(d[0].sectors_read, 6_543_210);
        assert_eq!(d[0].sectors_written, 1_234_567);
        assert_eq!(d[0].io_ticks, 456_789);
    }

    #[test]
    fn whole_disk_names() {
        for ok in [
            "sda", "sdab", "nvme0n1", "nvme10n2", "md0", "md127", "vda", "mmcblk0",
        ] {
            assert!(is_whole_disk(ok), "{ok}");
        }
        for no in [
            "sda1",
            "nvme0n1p1",
            "nvme0",
            "loop0",
            "ram0",
            "zram0",
            "dm-0",
            "sr0",
            "mmcblk0p1",
            "mmcblk0boot0",
            "md",
            "md0p1",
        ] {
            assert!(!is_whole_disk(no), "{no}");
        }
    }
}
