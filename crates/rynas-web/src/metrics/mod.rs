//! Live metrics: a collector thread samples `/proc` and `/sys` once per second into a ring
//! buffer (one hour) and broadcasts every sample, already serialized, to the SSE clients.
//! The same thread reads the hardware provider (it needs the bay temperatures anyway) and
//! broadcasts its status when it changes ([`Store::push_hardware`]), so no client polls
//! `/api/v1/hardware`.
//!
//! The collector runs on a plain OS thread: its work is a handful of small synchronous file
//! reads, it never spawns processes, and keeping it off the async runtime means a slow sysfs
//! read can never delay an HTTP response.

pub mod history;
pub mod net;
pub mod procfs;
pub mod ring;
pub mod temps;
pub mod zram;

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rynas_api_types::{DiskRate, HardwareStatus, MemUsage, MetricsSample, NetRate, ZramDevice};
use tokio::sync::broadcast;

use crate::hardware::HardwareProvider;
use procfs::{CpuTimes, DiskCounters};

/// One hour at one sample per second (hard cap; the oldest sample is dropped). Packed, the
/// full ring is ~1.5 MB on a DS920+ ([`ring`]).
pub const RING_CAPACITY: usize = 3600;
pub const INTERVAL: Duration = Duration::from_secs(1);
/// An unchanged hardware status is broadcast again after this long, so that clients see
/// `age_s` grow when the provider stops updating.
pub const HW_REPEAT: Duration = Duration::from_secs(60);

/// Shared between the collector (writer) and the HTTP handlers (readers).
pub struct Store {
    ring: RwLock<VecDeque<ring::Packed>>,
    latest_json: RwLock<Option<Arc<str>>>,
    last_io: RwLock<HashMap<String, Instant>>,
    /// zram devices of the newest sample (the ring does not keep them).
    zram: RwLock<Vec<ZramDevice>>,
    tx: broadcast::Sender<Arc<str>>,
    /// Last broadcast hardware status (`age_s` cleared, to compare) and when it was sent.
    hw_last: Mutex<Option<(HardwareStatus, Instant)>>,
    hw_json: RwLock<Option<Arc<str>>>,
    hw_tx: broadcast::Sender<Arc<str>>,
}

impl Store {
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(16);
        let (hw_tx, _) = broadcast::channel(4);
        Arc::new(Self {
            ring: RwLock::new(VecDeque::with_capacity(RING_CAPACITY)),
            latest_json: RwLock::new(None),
            last_io: RwLock::new(HashMap::new()),
            zram: RwLock::new(Vec::new()),
            tx,
            hw_last: Mutex::new(None),
            hw_json: RwLock::new(None),
            hw_tx,
        })
    }

    /// Publishes the hardware status if it changed (ignoring `age_s`) or was last sent
    /// [`HW_REPEAT`] ago; returns whether it was broadcast.
    pub fn push_hardware(&self, h: HardwareStatus) -> bool {
        let now = Instant::now();
        let mut key = h.clone();
        key.age_s = None;
        {
            let mut last = self.hw_last.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((k, at)) = last.as_ref()
                && *k == key
                && now.duration_since(*at) < HW_REPEAT
            {
                return false;
            }
            *last = Some((key, now));
        }
        let json: Arc<str> = match serde_json::to_string(&h) {
            Ok(j) => j.into(),
            Err(e) => {
                tracing::error!("serializing the hardware status: {e}");
                return false;
            }
        };
        *self.hw_json.write().unwrap_or_else(|e| e.into_inner()) = Some(json.clone());
        let _ = self.hw_tx.send(json);
        true
    }

    /// The last broadcast hardware status, serialized.
    pub fn latest_hardware_json(&self) -> Option<Arc<str>> {
        self.hw_json
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn subscribe_hardware(&self) -> broadcast::Receiver<Arc<str>> {
        self.hw_tx.subscribe()
    }

    pub fn push(&self, sample: MetricsSample) {
        let json: Arc<str> = match serde_json::to_string(&sample) {
            Ok(j) => j.into(),
            Err(e) => {
                tracing::error!("serializing a sample: {e}");
                return;
            }
        };
        {
            let mut ring = self.ring.write().unwrap_or_else(|e| e.into_inner());
            let packed = ring::Packed::pack(&sample, ring.back().map(ring::Packed::series));
            if ring.len() == RING_CAPACITY {
                ring.pop_front();
            }
            ring.push_back(packed);
        }
        {
            let mut z = self.zram.write().unwrap_or_else(|e| e.into_inner());
            if *z != sample.zram {
                z.clone_from(&sample.zram);
            }
        }
        *self.latest_json.write().unwrap_or_else(|e| e.into_inner()) = Some(json.clone());
        // no receivers is fine: nobody is watching the dashboard
        let _ = self.tx.send(json);
    }

    pub fn latest_json(&self) -> Option<Arc<str>> {
        self.latest_json
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<str>> {
        self.tx.subscribe()
    }

    /// The last `n` samples, averaged down to at most `max_points`. Samples are unpacked one
    /// bucket at a time, so the transient memory is one bucket, not the whole hour.
    pub fn history(&self, n: usize, max_points: usize) -> Vec<MetricsSample> {
        let ring = self.ring.read().unwrap_or_else(|e| e.into_inner());
        let start = ring.len().saturating_sub(n);
        let len = ring.len() - start;
        let mut out: Vec<MetricsSample> = history::buckets(len, max_points)
            .into_iter()
            .map(|r| {
                let bucket: Vec<MetricsSample> = ring
                    .range(start + r.start..start + r.end)
                    .map(ring::Packed::unpack)
                    .collect();
                let refs: Vec<&MetricsSample> = bucket.iter().collect();
                history::average(&refs)
            })
            .collect();
        if let Some(last) = out.last_mut() {
            last.zram = self.zram.read().unwrap_or_else(|e| e.into_inner()).clone();
        }
        out
    }

    /// Whether the collector saw I/O on this disk within `within`.
    pub fn disk_active_within(&self, disk: &str, within: Duration) -> bool {
        self.last_io
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(disk)
            .is_some_and(|t| t.elapsed() <= within)
    }

    fn mark_io(&self, disks: &[String], now: Instant) {
        if disks.is_empty() {
            return;
        }
        let mut m = self.last_io.write().unwrap_or_else(|e| e.into_inner());
        for d in disks {
            m.insert(d.clone(), now);
        }
    }
}

/// Reads a small file into a reused buffer (no allocation per tick).
fn read_into(path: &Path, buf: &mut String) -> bool {
    buf.clear();
    match std::fs::File::open(path) {
        Ok(mut f) => f.read_to_string(buf).is_ok(),
        Err(_) => false,
    }
}

fn read_small(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// One decimal is all a chart needs, and it keeps the JSON short.
fn round1(v: f32) -> f32 {
    (v * 10.0).round() / 10.0
}

fn rate(cur: u64, prev: u64, dt: f64) -> u64 {
    if dt <= 0.0 {
        return 0;
    }
    (cur.saturating_sub(prev) as f64 / dt).round() as u64
}

pub struct Collector {
    store: Arc<Store>,
    hw: Arc<dyn HardwareProvider>,
    net: net::NetClassifier,
    temps: temps::Temps,
    zram: zram::Zram,
    buf: String,
    prev_at: Option<Instant>,
    prev_cpu: (Option<CpuTimes>, Vec<CpuTimes>),
    prev_net: HashMap<String, (u64, u64)>,
    prev_disk: HashMap<String, DiskCounters>,
}

impl Collector {
    pub fn new(store: Arc<Store>, hw: Arc<dyn HardwareProvider>) -> Self {
        Self {
            store,
            hw,
            net: net::NetClassifier::new("/sys/class/net"),
            temps: temps::Temps::new("/sys/class/hwmon"),
            zram: zram::Zram::new("/sys/block"),
            buf: String::with_capacity(8192),
            prev_at: None,
            prev_cpu: (None, Vec::new()),
            prev_net: HashMap::new(),
            prev_disk: HashMap::new(),
        }
    }

    /// Starts the sampling thread. The first sample is published after one interval, so
    /// that every published sample has real rates.
    pub fn spawn(mut self) -> std::io::Result<std::thread::JoinHandle<()>> {
        std::thread::Builder::new()
            .name("metrics".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                let start = Instant::now();
                self.sample(start);
                let mut n: u32 = 1;
                loop {
                    let next = start + INTERVAL * n;
                    let now = Instant::now();
                    if next > now {
                        std::thread::sleep(next - now);
                    } else if now - next > INTERVAL * 5 {
                        // suspended or starved: resynchronize instead of bursting
                        n = ((now - start).as_secs_f64() / INTERVAL.as_secs_f64()) as u32;
                    }
                    n += 1;
                    let s = self.sample(Instant::now());
                    self.store.push(s);
                }
            })
    }

    /// Takes one sample; rates are computed against the previous call.
    pub fn sample(&mut self, now: Instant) -> MetricsSample {
        let dt = self
            .prev_at
            .map_or(0.0, |p| now.duration_since(p).as_secs_f64());
        self.prev_at = Some(now);
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let mut s = MetricsSample {
            ts_ms,
            ..Default::default()
        };

        // CPU
        if read_into(Path::new("/proc/stat"), &mut self.buf) {
            let (total, per) = procfs::parse_stat(&self.buf);
            if let (Some(t), Some(p)) = (total, self.prev_cpu.0) {
                s.cpu_pct = round1(t.pct_since(&p));
            }
            s.cpu_per_thread = per
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    self.prev_cpu
                        .1
                        .get(i)
                        .map_or(0.0, |p| round1(c.pct_since(p)))
                })
                .collect();
            self.prev_cpu = (total, per);
        }

        if read_into(Path::new("/proc/loadavg"), &mut self.buf) {
            s.load = procfs::parse_loadavg(&self.buf);
        }

        if read_into(Path::new("/proc/meminfo"), &mut self.buf) {
            let m = procfs::parse_meminfo(&self.buf);
            s.mem = MemUsage {
                total: m.total,
                used: m.total.saturating_sub(m.available),
                cache: m.cached + m.sreclaimable,
                swap_total: m.swap_total,
                swap_used: m.swap_total.saturating_sub(m.swap_free),
            };
        }
        s.zram = self.zram.read();

        // network
        if read_into(Path::new("/proc/net/dev"), &mut self.buf) {
            let counters = procfs::parse_net_dev(&self.buf);
            let mut seen = HashMap::with_capacity(counters.len());
            for c in counters {
                if !self.net.shown(&c.name, now) {
                    continue;
                }
                let (rx_bps, tx_bps) = match self.prev_net.get(&c.name) {
                    Some(&(rx, tx)) => (rate(c.rx_bytes, rx, dt), rate(c.tx_bytes, tx, dt)),
                    None => (0, 0),
                };
                let dir = self.net.root().join(&c.name);
                let oper = read_small(&dir.join("operstate")).unwrap_or_default();
                let up = oper == "up"
                    || (oper == "unknown"
                        && read_small(&dir.join("carrier")).as_deref() == Some("1"));
                let speed_mbps = read_small(&dir.join("speed"))
                    .and_then(|v| v.parse::<i64>().ok())
                    .filter(|&v| v > 0)
                    .map(|v| v as u32);
                s.net.push(NetRate {
                    name: c.name.clone(),
                    rx_bps,
                    tx_bps,
                    up,
                    speed_mbps,
                });
                seen.insert(c.name, (c.rx_bytes, c.tx_bytes));
            }
            s.net.sort_by(|a, b| a.name.cmp(&b.name));
            self.prev_net = seen;
        }

        // disks
        let mut active = Vec::new();
        if read_into(Path::new("/proc/diskstats"), &mut self.buf) {
            let counters = procfs::parse_diskstats(&self.buf);
            let mut seen = HashMap::with_capacity(counters.len());
            for c in counters {
                let mut d = DiskRate {
                    name: c.name.clone(),
                    ..Default::default()
                };
                if let Some(p) = self.prev_disk.get(&c.name) {
                    d.read_bps = rate(c.sectors_read, p.sectors_read, dt) * 512;
                    d.write_bps = rate(c.sectors_written, p.sectors_written, dt) * 512;
                    if dt > 0.0 {
                        let ticks = c.io_ticks.saturating_sub(p.io_ticks) as f64;
                        d.busy_pct = round1((ticks / (dt * 1000.0) * 100.0).min(100.0) as f32);
                    }
                    if c.sectors_read != p.sectors_read
                        || c.sectors_written != p.sectors_written
                        || c.io_ticks != p.io_ticks
                    {
                        active.push(c.name.clone());
                    }
                }
                s.disks.push(d);
                seen.insert(c.name.clone(), c);
            }
            self.prev_disk = seen;
        }
        self.store.mark_io(&active, now);

        // hardware (broadcast on change) and temperatures
        let bays = match self.hw.status() {
            Ok(Some(h)) => {
                let bays = h.bays.clone();
                self.store.push_hardware(h);
                bays
            }
            Ok(None) => {
                self.store.push_hardware(HardwareStatus::default());
                Vec::new()
            }
            // the provider failed: keep the last status (`GET /hardware` reports the error)
            Err(_) => Vec::new(),
        };
        let store = &self.store;
        s.temps = self.temps.read(now, &bays, |disk| {
            store.disk_active_within(disk, temps::DRIVE_ACTIVE)
        });
        s
    }
}
