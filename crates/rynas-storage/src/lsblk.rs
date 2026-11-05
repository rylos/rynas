//! Block device graph from `lsblk --json` (util-linux 2.41 on Debian 13).
//!
//! lsblk prints the stack upside down and repeats shared children: an md array built on
//! `sda3` and `sdb3` shows up under both partitions. The graph flattens that into one node
//! per kernel name with parent/child edges. lsblk only reads sysfs and the udev database, so
//! it never wakes a disk.

use std::collections::BTreeMap;

use serde_json::Value;

/// Columns asked from lsblk. `MAJ:MIN` comes back as the key "maj:min".
pub const LSBLK_ARGS: &[&str] = &[
    "--json",
    "--bytes",
    "--output",
    "NAME,KNAME,PATH,TYPE,FSTYPE,FSVER,LABEL,UUID,PARTLABEL,SIZE,ROTA,TRAN,MODEL,SERIAL,WWN,REV,MOUNTPOINTS,RM,HOTPLUG,MAJ:MIN",
];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BlockDev {
    /// Kernel name, e.g. "sda3", "md2", "dm-0".
    pub kname: String,
    /// lsblk name: the mapper name for dm devices ("luks-..."), else the kernel name.
    pub name: String,
    pub path: String,
    /// "disk", "part", "raid1", "raid5", "crypt", "lvm", "loop", "rom", ...
    pub kind: String,
    pub fstype: Option<String>,
    pub fsver: Option<String>,
    pub label: Option<String>,
    pub uuid: Option<String>,
    pub partlabel: Option<String>,
    pub size: u64,
    pub rotational: bool,
    pub transport: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub wwn: Option<String>,
    pub firmware: Option<String>,
    pub mountpoints: Vec<String>,
    pub removable: bool,
    pub hotplug: bool,
    pub majmin: Option<String>,
    /// Devices below (e.g. the partitions of an md array).
    pub parents: Vec<String>,
    /// Devices stacked on top.
    pub children: Vec<String>,
}

impl BlockDev {
    pub fn is_disk(&self) -> bool {
        self.kind == "disk"
    }
    pub fn is_md(&self) -> bool {
        self.kind.starts_with("raid") || self.kind == "linear" || self.kname.starts_with("md")
    }
}

/// Graph keyed by kernel name.
#[derive(Debug, Clone, Default)]
pub struct BlockGraph {
    pub devs: BTreeMap<String, BlockDev>,
    /// Attached loop devices count as disks (tests on loop files only, never in production):
    /// see [`BlockGraph::promote_loops`].
    pub loop_disks: bool,
}

fn s(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn u(v: &Value, key: &str) -> u64 {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// lsblk prints booleans as JSON booleans since 2.33, as "0"/"1" before.
fn b(v: &Value, key: &str) -> bool {
    match v.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "1",
        Some(Value::Number(n)) => n.as_u64() == Some(1),
        _ => false,
    }
}

fn mountpoints(v: &Value) -> Vec<String> {
    match v.get("mountpoints") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|m| m.as_str().map(str::to_string))
            .collect(),
        _ => s(v, "mountpoint").into_iter().collect(),
    }
}

impl BlockGraph {
    pub fn parse(json: &str) -> Result<Self, String> {
        let root: Value = serde_json::from_str(json).map_err(|e| format!("lsblk: {e}"))?;
        let list = root
            .get("blockdevices")
            .and_then(Value::as_array)
            .ok_or("lsblk: no blockdevices")?;
        let mut g = BlockGraph::default();
        for d in list {
            g.add(d, None);
        }
        Ok(g)
    }

    fn add(&mut self, v: &Value, parent: Option<&str>) {
        let Some(kname) = s(v, "kname").or_else(|| s(v, "name")) else {
            return;
        };
        let entry = self.devs.entry(kname.clone()).or_insert_with(|| BlockDev {
            kname: kname.clone(),
            name: s(v, "name").unwrap_or_else(|| kname.clone()),
            path: s(v, "path").unwrap_or_else(|| format!("/dev/{kname}")),
            kind: s(v, "type").unwrap_or_default(),
            fstype: s(v, "fstype"),
            fsver: s(v, "fsver"),
            label: s(v, "label"),
            uuid: s(v, "uuid"),
            partlabel: s(v, "partlabel"),
            size: u(v, "size"),
            rotational: b(v, "rota"),
            transport: s(v, "tran"),
            model: s(v, "model"),
            serial: s(v, "serial"),
            wwn: s(v, "wwn"),
            firmware: s(v, "rev"),
            mountpoints: mountpoints(v),
            removable: b(v, "rm"),
            hotplug: b(v, "hotplug"),
            majmin: s(v, "maj:min"),
            parents: Vec::new(),
            children: Vec::new(),
        });
        if let Some(p) = parent
            && !entry.parents.iter().any(|x| x == p)
        {
            entry.parents.push(p.to_string());
        }
        if let Some(p) = parent
            && let Some(pd) = self.devs.get_mut(p)
            && !pd.children.contains(&kname)
        {
            pd.children.push(kname.clone());
        }
        if let Some(Value::Array(children)) = v.get("children") {
            for c in children {
                self.add(c, Some(&kname));
            }
        }
    }

    pub fn get(&self, kname: &str) -> Option<&BlockDev> {
        self.devs.get(kname)
    }

    /// Find by kernel name, lsblk name, device path or `/dev/mapper/<name>`.
    pub fn find(&self, any: &str) -> Option<&BlockDev> {
        let bare = any
            .strip_prefix("/dev/mapper/")
            .or_else(|| any.strip_prefix("/dev/"))
            .unwrap_or(any);
        self.devs.get(bare).or_else(|| {
            self.devs
                .values()
                .find(|d| d.path == any || d.name == bare || d.name == any)
        })
    }

    /// Attached loop devices become disks (tests of the write side on loop files).
    pub fn promote_loops(&mut self) {
        self.loop_disks = true;
        for d in self.devs.values_mut() {
            if d.kind == "loop" && d.size > 0 {
                d.kind = "disk".into();
            }
        }
    }

    /// Whole disks, sorted the natural way (sda, sdb, ..., sdaa, nvme0n1).
    pub fn disks(&self) -> Vec<&BlockDev> {
        let mut v: Vec<&BlockDev> = self
            .devs
            .values()
            .filter(|d| d.is_disk() && !is_virtual_disk(&d.kname))
            .collect();
        if self.loop_disks {
            v.extend(
                self.devs
                    .values()
                    .filter(|d| d.is_disk() && d.kname.starts_with("loop")),
            );
        }
        v.sort_by(|a, b| {
            a.kname
                .len()
                .cmp(&b.kname.len())
                .then_with(|| a.kname.cmp(&b.kname))
        });
        v
    }

    /// The whole disks under a device (itself if it is a disk).
    pub fn disks_under(&self, kname: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![kname.to_string()];
        let mut seen = std::collections::HashSet::new();
        while let Some(k) = stack.pop() {
            if !seen.insert(k.clone()) {
                continue;
            }
            let Some(d) = self.devs.get(&k) else { continue };
            if d.is_disk() {
                out.push(k);
            } else {
                stack.extend(d.parents.iter().cloned());
            }
        }
        out.sort();
        out
    }

    /// Everything stacked above a device (children, grandchildren, ...).
    pub fn above(&self, kname: &str) -> Vec<&BlockDev> {
        let mut out = Vec::new();
        let mut stack: Vec<String> = self
            .devs
            .get(kname)
            .map(|d| d.children.clone())
            .unwrap_or_default();
        let mut seen = std::collections::HashSet::new();
        while let Some(k) = stack.pop() {
            if !seen.insert(k.clone()) {
                continue;
            }
            if let Some(d) = self.devs.get(&k) {
                stack.extend(d.children.iter().cloned());
                out.push(d);
            }
        }
        out
    }
}

/// zram, loop, ram disks and optical drives are not storage for a NAS volume.
pub fn is_virtual_disk(kname: &str) -> bool {
    ["zram", "loop", "ram", "sr", "fd", "nbd", "dm-", "md"]
        .iter()
        .any(|p| kname.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nas_stack() {
        let g = BlockGraph::parse(include_str!("../tests/fixtures/lsblk-nas.json")).unwrap();
        let disks: Vec<_> = g.disks().iter().map(|d| d.kname.as_str()).collect();
        assert_eq!(
            disks,
            ["sda", "sdb", "sdc", "sdd", "sde", "nvme0n1", "nvme1n1"]
        );
        let md2 = g.get("md2").unwrap();
        assert_eq!(md2.kind, "raid5");
        assert_eq!(md2.parents, ["sda1", "sdb1", "sdc1"]);
        assert_eq!(md2.children, ["dm-0"]);
        let dm = g.get("dm-0").unwrap();
        assert_eq!(dm.name, "rynas-vol1");
        assert_eq!(dm.kind, "crypt");
        assert_eq!(dm.fstype.as_deref(), Some("btrfs"));
        assert_eq!(dm.mountpoints, ["/vol1"]);
        assert_eq!(g.disks_under("dm-0"), ["sda", "sdb", "sdc"]);
        assert!(g.get("sda").unwrap().rotational);
        assert!(!g.get("nvme0n1").unwrap().rotational);
        assert_eq!(g.find("/dev/mapper/rynas-vol1").unwrap().kname, "dm-0");
        assert_eq!(g.find("/dev/md2").unwrap().kname, "md2");
        let above: Vec<_> = g.above("sda").iter().map(|d| d.kname.clone()).collect();
        assert!(above.contains(&"md2".to_string()) && above.contains(&"dm-0".to_string()));
        // sdd is blank, sde holds a locked LUKS container
        assert!(g.get("sdd").unwrap().children.is_empty());
        assert_eq!(
            g.get("sde1").unwrap().fstype.as_deref(),
            Some("crypto_LUKS")
        );
    }

    #[test]
    fn old_style_values() {
        let j = r#"{"blockdevices":[{"name":"sda","kname":"sda","type":"disk","size":"1000","rota":"1","rm":"0","mountpoint":null}]}"#;
        let g = BlockGraph::parse(j).unwrap();
        let d = g.get("sda").unwrap();
        assert_eq!(d.size, 1000);
        assert!(d.rotational);
        assert_eq!(d.path, "/dev/sda");
    }
}
