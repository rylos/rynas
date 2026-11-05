//! LUKS layers. Locked/unlocked comes from the block graph (a `crypto_LUKS` device with or
//! without a `crypt` child); details of an open mapping from `cryptsetup status <name>`, which
//! reads the device-mapper table only (no disk I/O).

use crate::lsblk::BlockGraph;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct CryptStatus {
    pub active: bool,
    pub in_use: bool,
    /// "LUKS2", "LUKS1", "PLAIN", ...
    pub kind: Option<String>,
    pub cipher: Option<String>,
    pub key_bits: Option<u32>,
    pub device: Option<String>,
    pub sector_size: Option<u32>,
    pub read_only: bool,
    /// e.g. ["discards", "no_read_workqueue"].
    pub flags: Vec<String>,
}

pub fn parse_status(s: &str) -> CryptStatus {
    let mut c = CryptStatus::default();
    for (i, line) in s.lines().enumerate() {
        if i == 0 {
            c.active = line.contains(" is active");
            c.in_use = line.contains("in use");
            continue;
        }
        let Some((k, v)) = line.trim().split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim() {
            "type" => c.kind = Some(v.to_string()),
            "cipher" => c.cipher = Some(v.to_string()),
            "keysize" => c.key_bits = v.split_whitespace().next().and_then(|n| n.parse().ok()),
            "device" => c.device = Some(v.to_string()),
            "sector size" => c.sector_size = v.parse().ok(),
            "mode" => c.read_only = v.contains("readonly") || v == "read-only",
            "flags" => c.flags = v.split_whitespace().map(str::to_string).collect(),
            _ => {}
        }
    }
    c
}

/// A LUKS container found in the block graph.
#[derive(Debug, Clone, PartialEq)]
pub struct Container {
    /// Kernel name of the encrypted device, e.g. "md2".
    pub device: String,
    pub uuid: Option<String>,
    pub version: Option<String>,
    /// Open mapping (kernel name, mapper name), None when locked.
    pub mapping: Option<(String, String)>,
}

pub fn containers(g: &BlockGraph) -> Vec<Container> {
    g.devs
        .values()
        .filter(|d| d.fstype.as_deref() == Some("crypto_LUKS"))
        .map(|d| Container {
            device: d.kname.clone(),
            uuid: d.uuid.clone(),
            version: d.fsver.clone(),
            mapping: d
                .children
                .iter()
                .filter_map(|c| g.get(c))
                .find(|c| c.kind == "crypt")
                .map(|c| (c.kname.clone(), c.name.clone())),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status() {
        let c = parse_status(include_str!("../tests/fixtures/cryptsetup-status.txt"));
        assert!(c.active && c.in_use);
        assert_eq!(c.kind.as_deref(), Some("LUKS2"));
        assert_eq!(c.cipher.as_deref(), Some("aes-xts-plain64"));
        assert_eq!(c.key_bits, Some(512));
        assert_eq!(c.device.as_deref(), Some("/dev/md2"));
        assert_eq!(c.sector_size, Some(4096));
        assert!(!c.read_only);
        assert_eq!(c.flags, ["discards"]);
    }

    #[test]
    fn locked_and_open() {
        let g = BlockGraph::parse(include_str!("../tests/fixtures/lsblk-nas.json")).unwrap();
        let v = containers(&g);
        let md2 = v.iter().find(|c| c.device == "md2").unwrap();
        assert_eq!(md2.mapping, Some(("dm-0".into(), "rynas-vol1".into())));
        let sde1 = v.iter().find(|c| c.device == "sde1").unwrap();
        assert_eq!(sde1.mapping, None);
        assert_eq!(sde1.version.as_deref(), Some("2"));
    }
}
