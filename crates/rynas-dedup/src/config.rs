//! Checks of the `dedup` key.

use rynas_api_types::dedup::{DedupConfig, VolumeDedup, job_key_valid, window_valid};
#[cfg(test)]
use rynas_api_types::dedup::ShareDedup;

/// At most this many jobs (filesystems).
pub const MAX_JOBS: usize = 64;

/// Checks one job's settings.
pub fn check_job(key: &str, v: &VolumeDedup) -> Result<(), String> {
    if !job_key_valid(key) {
        return Err(format!("invalid deduplication job {key:?}: a volume label or uuid.<uuid>"));
    }
    if !window_valid(&v.window) {
        return Err(format!(
            "job {key}: the window is 168 characters 0 or 1 with at least one hour on"
        ));
    }
    if !(4 << 10..=1 << 30).contains(&v.min_file_size) {
        return Err(format!("job {key}: the minimum file size is 4 KiB to 1 GiB"));
    }
    if !v.block_size.is_power_of_two() || !(4 << 10..=1 << 20).contains(&v.block_size) {
        return Err(format!("job {key}: the block size is a power of two from 4 KiB to 1 MiB"));
    }
    for (what, n) in [("CPU", v.cpu_threads), ("IO", v.io_threads)] {
        if n.is_some_and(|n| !(1..=8).contains(&n)) {
            return Err(format!("job {key}: {what} threads are 1 to 8"));
        }
    }
    if !(256..=16384).contains(&v.memory_max_mib) {
        return Err(format!("job {key}: the memory limit is 256 to 16384 MiB"));
    }
    Ok(())
}

/// Checks a whole document.
pub fn normalize(c: DedupConfig) -> Result<DedupConfig, String> {
    if c.jobs.len() > MAX_JOBS {
        return Err(format!("at most {MAX_JOBS} deduplication jobs"));
    }
    for (k, v) in &c.jobs {
        check_job(k, v)?;
    }
    Ok(c)
}

pub use rynas_api_types::dedup::normalize_share;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_pass_and_limits_hold() {
        let mut c = DedupConfig::default();
        c.jobs.insert("vol1".into(), VolumeDedup::default());
        assert!(normalize(c.clone()).is_ok());
        let bad = |f: &dyn Fn(&mut VolumeDedup)| {
            let mut c = c.clone();
            f(c.jobs.get_mut("vol1").unwrap());
            normalize(c).unwrap_err()
        };
        assert!(bad(&|v| v.window = "0".repeat(168)).contains("window"));
        assert!(bad(&|v| v.block_size = 100_000).contains("block size"));
        assert!(bad(&|v| v.block_size = 2 << 20).contains("block size"));
        assert!(bad(&|v| v.min_file_size = 1).contains("minimum"));
        assert!(bad(&|v| v.cpu_threads = Some(9)).contains("CPU"));
        assert!(bad(&|v| v.memory_max_mib = 64).contains("memory"));
        let mut c2 = DedupConfig::default();
        c2.jobs.insert("Vol 1".into(), VolumeDedup::default());
        assert!(normalize(c2).is_err());
    }

    #[test]
    fn share_settings_are_trimmed_and_checked() {
        let d = normalize_share(Some(ShareDedup {
            enabled: true,
            excludes: vec![" cache/ ".into(), "cache/".into(), "*.iso".into()],
            include_recycle: false,
        }))
        .unwrap()
        .unwrap();
        assert_eq!(d.excludes, ["cache/", "*.iso"]);
        assert_eq!(normalize_share(Some(ShareDedup::default())).unwrap(), None);
        for bad in ["!keep", "../x", "a/../b", "/", "**", "*", ""] {
            let r = normalize_share(Some(ShareDedup { enabled: true, excludes: vec![bad.into()], include_recycle: false }));
            assert!(r.is_err(), "{bad}");
        }
    }
}
