//! `GET /api/v1/system`.

use std::fs;

use rynas_api_types::SystemInfo;

use crate::metrics::procfs::parse_meminfo;

fn read_trim(p: &str) -> String {
    fs::read_to_string(p)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// `PRETTY_NAME` of an os-release file, unquoted.
pub fn pretty_name(os_release: &str) -> Option<String> {
    let v = os_release
        .lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))?;
    Some(v.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
}

/// (model name of the first CPU, number of logical CPUs) from `/proc/cpuinfo`.
pub fn cpu_info(cpuinfo: &str) -> (String, u32) {
    let mut model = String::new();
    let mut threads = 0;
    for line in cpuinfo.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        match k.trim() {
            "processor" => threads += 1,
            // "model name" on x86, "Model"/"Hardware" on some arm boards
            "model name" | "Model" | "Hardware" if model.is_empty() => model = v.trim().to_string(),
            _ => {}
        }
    }
    (model, threads)
}

pub fn system_info() -> SystemInfo {
    let os = fs::read_to_string("/etc/os-release")
        .or_else(|_| fs::read_to_string("/usr/lib/os-release"))
        .ok()
        .and_then(|s| pretty_name(&s))
        .unwrap_or_default();
    let (cpu_model, mut cpu_threads) =
        cpu_info(&fs::read_to_string("/proc/cpuinfo").unwrap_or_default());
    if cpu_threads == 0 {
        cpu_threads = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
    }
    let uptime_s = read_trim("/proc/uptime")
        .split_ascii_whitespace()
        .next()
        .and_then(|v| v.parse::<f64>().ok())
        .map_or(0, |v| v as u64);
    SystemInfo {
        hostname: read_trim("/proc/sys/kernel/hostname"),
        os,
        kernel: read_trim("/proc/sys/kernel/osrelease"),
        model: crate::hardware::dmi_model(),
        cpu_model,
        cpu_threads,
        mem_total: parse_meminfo(&fs::read_to_string("/proc/meminfo").unwrap_or_default()).total,
        uptime_s,
        rynas_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release() {
        let s =
            "NAME=\"Debian GNU/Linux\"\nPRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\nID=debian\n";
        assert_eq!(
            pretty_name(s).as_deref(),
            Some("Debian GNU/Linux 13 (trixie)")
        );
    }

    #[test]
    fn cpuinfo() {
        let s = "processor\t: 0\nmodel name\t: AMD Ryzen Embedded R1600\n\nprocessor\t: 1\nmodel name\t: AMD Ryzen Embedded R1600\n\nprocessor\t: 2\nprocessor\t: 3\n";
        assert_eq!(cpu_info(s), ("AMD Ryzen Embedded R1600".to_string(), 4));
    }
}
