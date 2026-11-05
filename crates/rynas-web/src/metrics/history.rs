//! Reduction of the history for `GET /api/v1/metrics/history`: consecutive samples are
//! averaged in buckets so the response stays around a few hundred points.

use std::ops::Range;

use rynas_api_types::{DiskRate, MetricsSample, NetRate, Temperature};

/// Default and maximum window, in minutes.
pub const DEFAULT_MINUTES: u32 = 10;
pub const MAX_MINUTES: u32 = 60;
/// Above this many samples the history is averaged in buckets.
pub const MAX_POINTS: usize = 600;

/// Bucket boundaries for `len` samples (oldest first) reduced to at most `max_points`:
/// equal buckets aligned on the newest sample, the oldest one possibly partial. One bucket
/// per sample when no reduction is needed.
pub fn buckets(len: usize, max_points: usize) -> Vec<Range<usize>> {
    if len <= max_points || max_points == 0 {
        return (0..len).map(|i| i..i + 1).collect();
    }
    let size = len.div_ceil(max_points);
    let first = len % size;
    let mut out = Vec::with_capacity(max_points);
    if first > 0 {
        out.push(0..first);
    }
    out.extend((first..len).step_by(size).map(|s| s..(s + size).min(len)));
    out
}

/// Averages `samples` (oldest first) into at most `max_points` buckets of equal size.
/// Each bucket keeps the timestamp and the series list of its last sample.
pub fn downsample(samples: &[&MetricsSample], max_points: usize) -> Vec<MetricsSample> {
    buckets(samples.len(), max_points)
        .into_iter()
        .map(|r| average(&samples[r]))
        .collect()
}

fn mean(it: impl Iterator<Item = f64>) -> f64 {
    let (sum, n) = it.fold((0.0, 0u32), |(s, n), v| (s + v, n + 1));
    if n == 0 { 0.0 } else { sum / n as f64 }
}

fn mean_u64(it: impl Iterator<Item = u64>) -> u64 {
    mean(it.map(|v| v as f64)).round() as u64
}

/// The average of a bucket (one sample: that sample).
pub fn average(bucket: &[&MetricsSample]) -> MetricsSample {
    let last = bucket[bucket.len() - 1];
    if bucket.len() == 1 {
        return last.clone();
    }
    let n = bucket.len() as f64;
    let mut out = last.clone();
    out.cpu_pct = (bucket.iter().map(|s| s.cpu_pct as f64).sum::<f64>() / n) as f32;
    for (i, v) in out.cpu_per_thread.iter_mut().enumerate() {
        *v = mean(
            bucket
                .iter()
                .filter_map(|s| s.cpu_per_thread.get(i).map(|&x| x as f64)),
        ) as f32;
    }
    for i in 0..3 {
        out.load[i] = (bucket.iter().map(|s| s.load[i] as f64).sum::<f64>() / n) as f32;
    }
    out.mem.used = mean_u64(bucket.iter().map(|s| s.mem.used));
    out.mem.cache = mean_u64(bucket.iter().map(|s| s.mem.cache));
    out.mem.swap_used = mean_u64(bucket.iter().map(|s| s.mem.swap_used));

    let nets = |name: &str| -> Vec<&NetRate> {
        bucket
            .iter()
            .filter_map(|s| s.net.iter().find(|x| x.name == name))
            .collect()
    };
    for r in &mut out.net {
        let v = nets(&r.name);
        r.rx_bps = mean_u64(v.iter().map(|x| x.rx_bps));
        r.tx_bps = mean_u64(v.iter().map(|x| x.tx_bps));
    }
    let disks = |name: &str| -> Vec<&DiskRate> {
        bucket
            .iter()
            .filter_map(|s| s.disks.iter().find(|x| x.name == name))
            .collect()
    };
    for d in &mut out.disks {
        let v = disks(&d.name);
        d.read_bps = mean_u64(v.iter().map(|x| x.read_bps));
        d.write_bps = mean_u64(v.iter().map(|x| x.write_bps));
        d.busy_pct = mean(v.iter().map(|x| x.busy_pct as f64)) as f32;
    }
    let temps = |id: &str| -> Vec<&Temperature> {
        bucket
            .iter()
            .filter_map(|s| s.temps.iter().find(|x| x.id == id))
            .collect()
    };
    for t in &mut out.temps {
        let v: Vec<f64> = temps(&t.id)
            .iter()
            .filter_map(|x| x.celsius.map(f64::from))
            .collect();
        if !v.is_empty() {
            t.celsius = Some((v.iter().sum::<f64>() / v.len() as f64) as f32);
        }
    }
    out
}

/// Parses the `minutes` query parameter: default 10, 1..=60.
pub fn parse_minutes(query: Option<&str>) -> Result<u32, String> {
    let Some(raw) = query
        .unwrap_or("")
        .split('&')
        .find_map(|kv| kv.strip_prefix("minutes="))
    else {
        return Ok(DEFAULT_MINUTES);
    };
    match raw.parse::<u32>() {
        Ok(m) if (1..=MAX_MINUTES).contains(&m) => Ok(m),
        _ => Err(format!(
            "minutes must be an integer between 1 and {MAX_MINUTES}, got {raw:?}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(i: u64) -> MetricsSample {
        MetricsSample {
            ts_ms: i * 1000,
            cpu_pct: i as f32,
            cpu_per_thread: vec![i as f32, 0.0],
            net: vec![NetRate {
                name: "eth0".into(),
                rx_bps: i * 10,
                ..Default::default()
            }],
            disks: vec![DiskRate {
                name: "sda".into(),
                busy_pct: 50.0,
                ..Default::default()
            }],
            temps: vec![Temperature {
                id: "cpu".into(),
                celsius: Some(40.0 + i as f32),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn no_reduction_below_max() {
        let v: Vec<_> = (0..10).map(sample).collect();
        let r: Vec<_> = v.iter().collect();
        assert_eq!(downsample(&r, 600).len(), 10);
    }

    #[test]
    fn buckets_average() {
        let v: Vec<_> = (0..3600).map(sample).collect();
        let r: Vec<_> = v.iter().collect();
        let d = downsample(&r, 600);
        assert_eq!(d.len(), 600);
        // first bucket = samples 0..6 → mean 2.5, timestamp of its last sample
        assert_eq!(d[0].ts_ms, 5000);
        assert!((d[0].cpu_pct - 2.5).abs() < 1e-4);
        assert!((d[0].cpu_per_thread[0] - 2.5).abs() < 1e-4);
        assert_eq!(d[0].net[0].rx_bps, 25);
        assert_eq!(d[599].ts_ms, 3_599_000);
        assert!((d[599].temps[0].celsius.unwrap() - (40.0 + 3596.5)).abs() < 1e-2);
    }

    #[test]
    fn partial_oldest_bucket() {
        let v: Vec<_> = (0..1000).map(sample).collect();
        let r: Vec<_> = v.iter().collect();
        let d = downsample(&r, 600);
        assert!(d.len() <= 600);
        assert_eq!(d.last().unwrap().ts_ms, 999_000);
    }

    #[test]
    fn minutes() {
        assert_eq!(parse_minutes(None), Ok(10));
        assert_eq!(parse_minutes(Some("minutes=60")), Ok(60));
        assert_eq!(parse_minutes(Some("x=1&minutes=5")), Ok(5));
        assert!(parse_minutes(Some("minutes=0")).is_err());
        assert!(parse_minutes(Some("minutes=61")).is_err());
        assert!(parse_minutes(Some("minutes=abc")).is_err());
    }
}
