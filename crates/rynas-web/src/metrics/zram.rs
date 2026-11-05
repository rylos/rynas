//! zram devices (`/sys/block/zram*`): the algorithm and the `mm_stat` sizes (D-ZRAM, shown in
//! the dashboard's Memory widget). Uninitialized devices (`disksize` 0) are left out.

use std::path::{Path, PathBuf};

use rynas_api_types::ZramDevice;

/// The selected algorithm of `comp_algorithm` ("lzo-rle lzo [lz4] zstd" → "lz4").
pub fn selected_algorithm(s: &str) -> Option<&str> {
    s.split_ascii_whitespace()
        .find_map(|w| w.strip_prefix('[')?.strip_suffix(']'))
}

/// `mm_stat`: orig_data_size compr_data_size mem_used_total mem_limit mem_used_max same_pages
/// pages_compacted huge_pages [huge_pages_since] → (data, compressed, mem_used, huge_pages).
pub fn parse_mm_stat(s: &str) -> Option<(u64, u64, u64, u64)> {
    let v: Vec<u64> = s
        .split_ascii_whitespace()
        .map_while(|f| f.parse().ok())
        .collect();
    (v.len() >= 3).then(|| (v[0], v[1], v[2], v.get(7).copied().unwrap_or(0)))
}

fn read(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok()
}

/// Reads the zram devices below `block_root` (`/sys/block`), sorted by name.
pub struct Zram {
    root: PathBuf,
}

impl Zram {
    pub fn new(block_root: impl Into<PathBuf>) -> Self {
        Self {
            root: block_root.into(),
        }
    }

    pub fn read(&self) -> Vec<ZramDevice> {
        let Ok(dir) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut out: Vec<ZramDevice> = dir
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let n = name.strip_prefix("zram")?;
                if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                let d = self.root.join(&name);
                let disksize: u64 = read(&d.join("disksize"))?.trim().parse().ok()?;
                if disksize == 0 {
                    return None;
                }
                let (data, compressed, mem_used, huge_pages) =
                    parse_mm_stat(&read(&d.join("mm_stat"))?)?;
                let algorithm = read(&d.join("comp_algorithm"))
                    .and_then(|a| selected_algorithm(&a).map(str::to_string))
                    .unwrap_or_default();
                Some(ZramDevice {
                    name,
                    algorithm,
                    disksize,
                    data,
                    compressed,
                    mem_used,
                    huge_pages,
                })
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn algorithm_in_brackets() {
        assert_eq!(
            selected_algorithm("lzo-rle lzo [lz4] lz4hc zstd deflate 842\n"),
            Some("lz4")
        );
        assert_eq!(selected_algorithm("[zstd]"), Some("zstd"));
        assert_eq!(selected_algorithm("lzo lz4"), None);
    }

    #[test]
    fn mm_stat_columns() {
        let s = "12473589760 5382274290 5488381952        0 5588070400    37527   699809   223737   494364\n";
        assert_eq!(
            parse_mm_stat(s),
            Some((12_473_589_760, 5_382_274_290, 5_488_381_952, 223_737))
        );
        // older kernels: no huge_pages column
        assert_eq!(
            parse_mm_stat("4096 1024 8192 0 8192 0 0"),
            Some((4096, 1024, 8192, 0))
        );
        assert_eq!(parse_mm_stat("garbage"), None);
    }

    #[test]
    fn reads_initialized_devices_only() {
        let d = tempfile::tempdir().unwrap();
        let dev = |name: &str, size: &str, alg: &str, mm: &str| {
            let p = d.path().join(name);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("disksize"), size).unwrap();
            std::fs::write(p.join("comp_algorithm"), alg).unwrap();
            std::fs::write(p.join("mm_stat"), mm).unwrap();
        };
        dev(
            "zram1",
            "4294967296\n",
            "lzo [zstd]\n",
            "1000 300 400 0 400 0 0 2 0\n",
        );
        dev(
            "zram0",
            "8589934592\n",
            "[lz4] zstd\n",
            "2000 900 1000 0 1000 0 0 5 0\n",
        );
        dev("zram2", "0\n", "[lzo-rle]\n", "0 0 0 0 0 0 0 0 0\n");
        std::fs::create_dir_all(d.path().join("sda")).unwrap();
        std::fs::create_dir_all(d.path().join("zramx")).unwrap();
        let v = Zram::new(d.path()).read();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "zram0");
        assert_eq!(v[0].algorithm, "lz4");
        assert_eq!(
            (
                v[0].disksize,
                v[0].data,
                v[0].compressed,
                v[0].mem_used,
                v[0].huge_pages
            ),
            (8_589_934_592, 2000, 900, 1000, 5)
        );
        assert_eq!(v[1].algorithm, "zstd");
        assert!(Zram::new(d.path().join("missing")).read().is_empty());
    }
}
