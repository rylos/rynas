//! Which network interfaces the dashboard shows.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What sysfs tells about an interface, enough to decide whether to show it.
#[derive(Debug, Default, Clone)]
pub struct IfaceFacts<'a> {
    /// `/sys/class/net/<if>/device` exists: backed by real (or paravirtual) hardware.
    pub has_device: bool,
    /// `DEVTYPE` from the interface's `uevent` (vlan, bond, bridge, wlan, wireguard, …).
    pub devtype: Option<&'a str>,
    pub is_bond: bool,
    pub is_bridge: bool,
}

/// Physical NICs, bonds, bridges and VLANs; not loopback, veth, container/VM bridges, tun/tap.
pub fn is_shown(name: &str, f: &IfaceFacts) -> bool {
    const HIDDEN_PREFIXES: [&str; 8] = [
        "lo",
        "veth",
        "docker",
        "virbr",
        "vnet",
        "tun",
        "tap",
        "tailscale",
    ];
    if HIDDEN_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return false;
    }
    // docker user networks: br-<12 hex>
    if let Some(id) = name.strip_prefix("br-")
        && id.len() == 12
        && id.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return false;
    }
    f.has_device
        || f.is_bond
        || f.is_bridge
        || matches!(f.devtype, Some("vlan" | "bond" | "bridge"))
}

/// Caches [`is_shown`] per interface (sysfs probes once a minute, not every second).
pub struct NetClassifier {
    root: PathBuf,
    cache: HashMap<String, bool>,
    since: Instant,
}

impl NetClassifier {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            cache: HashMap::new(),
            since: Instant::now(),
        }
    }

    pub fn shown(&mut self, name: &str, now: Instant) -> bool {
        if now.duration_since(self.since) > Duration::from_secs(60) {
            self.cache.clear();
            self.since = now;
        }
        if let Some(&v) = self.cache.get(name) {
            return v;
        }
        let dir = self.root.join(name);
        let uevent = std::fs::read_to_string(dir.join("uevent")).unwrap_or_default();
        let devtype = uevent.lines().find_map(|l| l.strip_prefix("DEVTYPE="));
        let facts = IfaceFacts {
            has_device: dir.join("device").exists(),
            devtype,
            is_bond: dir.join("bonding").is_dir(),
            is_bridge: dir.join("bridge").is_dir(),
        };
        let v = is_shown(name, &facts);
        self.cache.insert(name.to_string(), v);
        v
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter() {
        let phys = IfaceFacts {
            has_device: true,
            ..Default::default()
        };
        let virt = IfaceFacts::default();
        let bridge = IfaceFacts {
            is_bridge: true,
            ..Default::default()
        };
        let vlan = IfaceFacts {
            devtype: Some("vlan"),
            ..Default::default()
        };
        let wg = IfaceFacts {
            devtype: Some("wireguard"),
            ..Default::default()
        };
        assert!(is_shown("eth0", &phys));
        assert!(is_shown("enp3s0", &phys));
        assert!(is_shown(
            "bond0",
            &IfaceFacts {
                is_bond: true,
                ..Default::default()
            }
        ));
        assert!(is_shown("br0", &bridge));
        assert!(is_shown("eth0.10", &vlan));
        assert!(!is_shown("lo", &virt));
        assert!(!is_shown("veth1a2b3c4", &virt));
        assert!(!is_shown("docker0", &bridge));
        assert!(!is_shown("virbr0", &bridge));
        assert!(!is_shown("br-0123456789ab", &bridge));
        assert!(!is_shown("tailscale0", &virt));
        assert!(!is_shown("wg0", &wg));
    }
}
