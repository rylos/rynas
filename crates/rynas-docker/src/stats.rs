//! The aggregated stats sampler of the dashboard widget: CPU and memory of every running
//! container from its cgroup v2 files (`cpu.stat`, `memory.current`, `memory.stat`), the
//! way `docker stats` computes them, without one API stream per container. A few small file
//! reads per container per sample: cheap on a 4 GB NAS.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rynas_api_types::docker::{ContainerStat, DockerStats};

/// How many containers the widget lists.
pub const TOP: usize = 5;

/// A running container to sample: (full id, name, stack).
pub type Target = (String, String, Option<String>);

/// Keeps the previous CPU counters between samples.
pub struct Sampler {
    /// `/sys/fs/cgroup` (below a test root in tests).
    root: PathBuf,
    prev: HashMap<String, (u64, u64)>,
}

/// `usage_usec` of a `cpu.stat` text.
pub fn cpu_usage_usec(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix("usage_usec "))
        .and_then(|v| v.trim().parse().ok())
}

/// A field of a `memory.stat` text.
pub fn mem_stat(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(' ')?;
        (k == key).then(|| v.trim().parse().ok()).flatten()
    })
}

/// `MemTotal` of `/proc/meminfo`, bytes.
pub fn mem_total(meminfo: &str) -> u64 {
    meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

impl Sampler {
    pub fn new(cgroup_root: impl Into<PathBuf>) -> Self {
        Self {
            root: cgroup_root.into(),
            prev: HashMap::new(),
        }
    }

    /// The cgroup folder of a container: the systemd driver's scope, or cgroupfs's folder.
    pub fn cgroup_dir(&self, id: &str) -> Option<PathBuf> {
        [
            format!("system.slice/docker-{id}.scope"),
            format!("docker/{id}"),
            format!("system.slice/docker.service/docker/{id}"),
        ]
        .into_iter()
        .map(|p| self.root.join(p))
        .find(|p| p.join("cpu.stat").is_file())
    }

    fn read(p: &Path, f: &str) -> Option<String> {
        std::fs::read_to_string(p.join(f)).ok()
    }

    /// One sample at `now_us` (a monotonic clock in microseconds). CPU needs two samples:
    /// a container seen for the first time counts 0 %.
    pub fn sample(&mut self, targets: &[Target], now_us: u64) -> Vec<ContainerStat> {
        let mut next = HashMap::new();
        let mut out = Vec::new();
        for (id, name, stack) in targets {
            let Some(dir) = self.cgroup_dir(id) else {
                continue;
            };
            let usage = Self::read(&dir, "cpu.stat").and_then(|t| cpu_usage_usec(&t));
            let cur = Self::read(&dir, "memory.current").and_then(|t| t.trim().parse::<u64>().ok());
            let inactive = Self::read(&dir, "memory.stat")
                .and_then(|t| mem_stat(&t, "inactive_file"))
                .unwrap_or(0);
            let cpu_pct = match (usage, self.prev.get(id)) {
                (Some(u), Some(&(pu, pt))) if now_us > pt && u >= pu => {
                    ((u - pu) as f64 / (now_us - pt) as f64 * 100.0) as f32
                }
                _ => 0.0,
            };
            if let Some(u) = usage {
                next.insert(id.clone(), (u, now_us));
            }
            out.push(ContainerStat {
                id: id.clone(),
                name: name.clone(),
                stack: stack.clone(),
                cpu_pct,
                mem_bytes: cur.map(|c| c.saturating_sub(inactive)).unwrap_or(0),
            });
        }
        // containers that stopped are forgotten
        self.prev = next;
        out
    }
}

/// The widget's sample from the per-container numbers.
pub fn aggregate(
    mut stats: Vec<ContainerStat>,
    running: u32,
    total: u32,
    cpus: u32,
    mem_total_bytes: u64,
    at_ms: u64,
) -> DockerStats {
    let cpu_pct = stats.iter().map(|s| s.cpu_pct).sum::<f32>();
    let mem_bytes = stats.iter().map(|s| s.mem_bytes).sum();
    // the same reading for every container: no extra cgroup reads
    let usage = stats
        .iter()
        .map(|s| rynas_api_types::docker::ContainerUsage {
            id: s.id.clone(),
            cpu: (s.cpu_pct * 10.0).round() / 10.0,
            mem: s.mem_bytes,
        })
        .collect();
    stats.sort_by(|a, b| {
        b.cpu_pct
            .partial_cmp(&a.cpu_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.mem_bytes.cmp(&a.mem_bytes))
            .then(a.name.cmp(&b.name))
    });
    stats.truncate(TOP);
    for s in &mut stats {
        s.cpu_pct = (s.cpu_pct * 10.0).round() / 10.0;
    }
    DockerStats {
        running,
        total,
        cpu_pct: (cpu_pct * 10.0).round() / 10.0,
        cpus,
        mem_bytes,
        mem_total_bytes,
        top: stats,
        at_ms,
        usage,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cg(root: &Path, rel: &str, usage: u64, mem: u64, inactive: u64) {
        let d = root.join(rel);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("cpu.stat"),
            format!("usage_usec {usage}\nuser_usec 1\nsystem_usec 2\n"),
        )
        .unwrap();
        std::fs::write(d.join("memory.current"), format!("{mem}\n")).unwrap();
        std::fs::write(
            d.join("memory.stat"),
            format!("anon 5\nfile 9\ninactive_file {inactive}\n"),
        )
        .unwrap();
    }

    #[test]
    fn two_samples_give_cpu() {
        let d = tempfile::tempdir().unwrap();
        cg(
            d.path(),
            "system.slice/docker-aaa.scope",
            1_000_000,
            300 << 20,
            100 << 20,
        );
        cg(d.path(), "docker/bbb", 0, 50 << 20, 0);
        let mut s = Sampler::new(d.path());
        let t: Vec<Target> = vec![
            ("aaa".into(), "web".into(), Some("site".into())),
            ("bbb".into(), "db".into(), None),
            ("ccc".into(), "gone".into(), None),
        ];
        let first = s.sample(&t, 1_000_000);
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|x| x.cpu_pct == 0.0));
        assert_eq!(
            first[0].mem_bytes,
            200 << 20,
            "inactive file cache is not counted"
        );
        cg(
            d.path(),
            "system.slice/docker-aaa.scope",
            1_500_000,
            300 << 20,
            100 << 20,
        );
        cg(d.path(), "docker/bbb", 2_000_000, 50 << 20, 0);
        let second = s.sample(&t, 2_000_000);
        assert!((second[0].cpu_pct - 50.0).abs() < 0.01);
        assert!((second[1].cpu_pct - 200.0).abs() < 0.01);
        let agg = aggregate(second, 2, 3, 4, 4 << 30, 7);
        assert_eq!((agg.running, agg.total, agg.cpus), (2, 3, 4));
        assert!((agg.cpu_pct - 250.0).abs() < 0.01);
        assert_eq!(agg.mem_bytes, 250 << 20);
        assert_eq!(agg.top[0].name, "db");
        // every running container, not only the top: the containers table
        assert_eq!(agg.usage.len(), 2);
        let total: u64 = agg.usage.iter().map(|u| u.mem).sum();
        assert_eq!(total, agg.mem_bytes);
        assert!(agg.usage.iter().any(|u| u.id == second_ids(&t)[1] && (u.cpu - 200.0).abs() < 0.01));
    }

    fn second_ids(t: &[Target]) -> Vec<String> {
        t.iter().map(|x| x.0.clone()).collect()
    }

    #[test]
    fn parsers() {
        assert_eq!(cpu_usage_usec("usage_usec 42\n"), Some(42));
        assert_eq!(
            mem_stat("anon 1\ninactive_file 7\n", "inactive_file"),
            Some(7)
        );
        assert_eq!(mem_total("MemTotal:        3884532 kB\n"), 3_884_532 * 1024);
    }
}
