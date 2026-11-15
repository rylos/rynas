//! LVM facts from `pvs`, `vgs` and `lvs --reportformat json` (lvm2 2.03 on Debian 13), for
//! the flexible RAID volumes (md tiers joined by a volume group).
//!
//! The structure (which logical volume sits on which arrays) comes from lsblk without any
//! disk access; these commands add what only the LVM metadata knows: physical volume and
//! group sizes, free space, missing devices. LVM reads its metadata from the devices, so the
//! commands are restricted with `--devices` to the physical volumes of one layout (also
//! sidestepping a devices file that does not list them) and the caller runs them only when
//! no disk under those devices sleeps, like `mdadm --detail`.

use serde_json::Value;

/// Options common to the three reports: JSON, sizes in bytes without suffix.
const COMMON: &[&str] = &["--reportformat", "json", "--units", "b", "--nosuffix"];
pub const PVS_FIELDS: &str = "pv_name,pv_uuid,vg_name,pv_size,pv_free,dev_size,pv_attr";
pub const VGS_FIELDS: &str =
    "vg_name,vg_uuid,vg_size,vg_free,vg_extent_size,pv_count,lv_count,vg_attr";
pub const LVS_FIELDS: &str = "lv_name,vg_name,lv_uuid,lv_size,lv_attr,lv_dm_path,segtype,devices";

/// The arguments of `pvs`, `vgs` or `lvs` (`program`) over exactly `devices`.
pub fn args(program: &str, devices: &[String]) -> Vec<String> {
    let fields = match program {
        "pvs" => PVS_FIELDS,
        "vgs" => VGS_FIELDS,
        _ => LVS_FIELDS,
    };
    let mut v: Vec<String> = COMMON.iter().map(|s| s.to_string()).collect();
    v.push("--devices".into());
    v.push(devices.join(","));
    v.push("-o".into());
    v.push(fields.into());
    v
}

/// A physical volume.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pv {
    /// Device path as LVM found it, e.g. "/dev/md127".
    pub name: String,
    pub uuid: String,
    /// Empty for an orphan physical volume.
    pub vg: String,
    pub size: u64,
    pub free: u64,
    /// Size of the device under it: larger than `size` when the array grew and `pvresize`
    /// has not run.
    pub dev_size: u64,
    /// The device is missing (`pv_attr` third character `m`).
    pub missing: bool,
}

/// A volume group.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Vg {
    pub name: String,
    pub uuid: String,
    pub size: u64,
    pub free: u64,
    pub extent_size: u64,
    pub pv_count: u32,
    pub lv_count: u32,
    /// One of its physical volumes is missing (`vg_attr` fourth character `p`).
    pub partial: bool,
}

/// A logical volume (its segments merged).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Lv {
    pub name: String,
    pub vg: String,
    pub uuid: String,
    pub size: u64,
    pub attr: String,
    /// e.g. "/dev/mapper/rynas_vol1-data".
    pub dm_path: String,
    /// Segment types in order ("linear", "striped", "raid1"...).
    pub segtypes: Vec<String>,
    /// Physical volumes it uses, in segment order, each once.
    pub devices: Vec<String>,
}

/// The three reports together.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LvmReport {
    pub pvs: Vec<Pv>,
    pub vgs: Vec<Vg>,
    pub lvs: Vec<Lv>,
}

impl LvmReport {
    pub fn is_empty(&self) -> bool {
        self.pvs.is_empty() && self.vgs.is_empty() && self.lvs.is_empty()
    }

    pub fn vg(&self, name: &str) -> Option<&Vg> {
        self.vgs.iter().find(|v| v.name == name)
    }

    pub fn lv(&self, vg: &str, name: &str) -> Option<&Lv> {
        self.lvs.iter().find(|l| l.vg == vg && l.name == name)
    }

    /// Adds the reports of another set of devices (one layout at a time).
    pub fn extend(&mut self, other: LvmReport) {
        self.pvs.extend(other.pvs);
        self.vgs.extend(other.vgs);
        self.lvs.extend(other.lvs);
    }
}

/// The rows of one report (`pv`, `vg` or `lv`).
fn rows(json: &str, key: &str) -> Result<Vec<Value>, String> {
    let root: Value = serde_json::from_str(json).map_err(|e| format!("{key}s: {e}"))?;
    let reports = root
        .get("report")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{key}s: no report"))?;
    Ok(reports
        .iter()
        .filter_map(|r| r.get(key).and_then(Value::as_array))
        .flatten()
        .cloned()
        .collect())
}

fn text(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// A size: "8001427406848", or with the unit suffix when `--nosuffix` was not honored.
fn bytes(v: &Value, key: &str) -> u64 {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s
            .trim()
            .trim_end_matches(['B', 'b'])
            .parse::<f64>()
            .map(|f| f as u64)
            .unwrap_or(0),
        _ => 0,
    }
}

fn attr_char(attr: &str, i: usize) -> Option<char> {
    attr.chars().nth(i)
}

pub fn parse_pvs(json: &str) -> Result<Vec<Pv>, String> {
    Ok(rows(json, "pv")?
        .iter()
        .map(|r| {
            let attr = text(r, "pv_attr");
            Pv {
                name: text(r, "pv_name"),
                uuid: text(r, "pv_uuid"),
                vg: text(r, "vg_name"),
                size: bytes(r, "pv_size"),
                free: bytes(r, "pv_free"),
                dev_size: bytes(r, "dev_size"),
                missing: attr_char(&attr, 2) == Some('m'),
            }
        })
        .collect())
}

pub fn parse_vgs(json: &str) -> Result<Vec<Vg>, String> {
    Ok(rows(json, "vg")?
        .iter()
        .map(|r| {
            let attr = text(r, "vg_attr");
            Vg {
                name: text(r, "vg_name"),
                uuid: text(r, "vg_uuid"),
                size: bytes(r, "vg_size"),
                free: bytes(r, "vg_free"),
                extent_size: bytes(r, "vg_extent_size"),
                pv_count: bytes(r, "pv_count") as u32,
                lv_count: bytes(r, "lv_count") as u32,
                partial: attr_char(&attr, 3) == Some('p'),
            }
        })
        .collect())
}

/// `lvs` prints one row per segment when `devices` or `segtype` is asked: the rows of one
/// volume are merged.
pub fn parse_lvs(json: &str) -> Result<Vec<Lv>, String> {
    let mut out: Vec<Lv> = Vec::new();
    for r in rows(json, "lv")? {
        let uuid = text(&r, "lv_uuid");
        let i = match out.iter().position(|l| l.uuid == uuid && !uuid.is_empty()) {
            Some(i) => i,
            None => {
                out.push(Lv {
                    name: text(&r, "lv_name"),
                    vg: text(&r, "vg_name"),
                    uuid,
                    size: bytes(&r, "lv_size"),
                    attr: text(&r, "lv_attr"),
                    dm_path: text(&r, "lv_dm_path"),
                    ..Default::default()
                });
                out.len() - 1
            }
        };
        let lv = &mut out[i];
        let seg = text(&r, "segtype");
        if !seg.is_empty() {
            lv.segtypes.push(seg);
        }
        // "/dev/md127(0),/dev/md128(0)": device and first extent
        for d in text(&r, "devices").split(',') {
            let dev = d.split('(').next().unwrap_or("").trim();
            if !dev.is_empty() && !lv.devices.iter().any(|x| x == dev) {
                lv.devices.push(dev.to_string());
            }
        }
    }
    Ok(out)
}

/// The three reports, each parsed on its own (a failed one stays empty).
pub fn parse(pvs: Option<&str>, vgs: Option<&str>, lvs: Option<&str>) -> LvmReport {
    LvmReport {
        pvs: pvs.and_then(|s| parse_pvs(s).ok()).unwrap_or_default(),
        vgs: vgs.and_then(|s| parse_vgs(s).ok()).unwrap_or_default(),
        lvs: lvs.and_then(|s| parse_lvs(s).ok()).unwrap_or_default(),
    }
}

/// Splits a device-mapper name into (volume group, logical volume): LVM doubles every `-`
/// inside the names and joins them with a single one ("my--vg-data" → ("my-vg", "data")).
pub fn split_dm_name(name: &str) -> Option<(String, String)> {
    let b = name.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'-' {
            if b.get(i + 1) == Some(&b'-') {
                i += 2;
                continue;
            }
            let (vg, lv) = (&name[..i], &name[i + 1..]);
            if vg.is_empty() || lv.is_empty() {
                return None;
            }
            return Some((vg.replace("--", "-"), lv.replace("--", "-")));
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const PVS: &str = include_str!("../tests/fixtures/lvm-pvs-flex.json");
    const VGS: &str = include_str!("../tests/fixtures/lvm-vgs-flex.json");
    const LVS: &str = include_str!("../tests/fixtures/lvm-lvs-flex.json");

    #[test]
    fn reports() {
        let r = parse(Some(PVS), Some(VGS), Some(LVS));
        assert_eq!(r.pvs.len(), 2);
        let p = &r.pvs[0];
        assert_eq!(p.name, "/dev/md126");
        assert_eq!(p.vg, "rynas_vol2");
        assert_eq!(p.free, 0);
        assert!(p.dev_size >= p.size && !p.missing);
        let vg = r.vg("rynas_vol2").unwrap();
        assert_eq!((vg.pv_count, vg.lv_count, vg.partial), (2, 1, false));
        assert_eq!(vg.extent_size, 4 << 20);
        assert_eq!(vg.free, 0);
        let lv = r.lv("rynas_vol2", "data").unwrap();
        // two linear segments, one per tier, merged
        assert_eq!(lv.segtypes, ["linear", "linear"]);
        assert_eq!(lv.devices, ["/dev/md126", "/dev/md127"]);
        assert_eq!(lv.dm_path, "/dev/mapper/rynas_vol2-data");
        assert_eq!(lv.size, vg.size);
    }

    #[test]
    fn missing_and_partial_flags_and_suffixes() {
        let pvs = r#"{"report":[{"pv":[{"pv_name":"[unknown]","pv_uuid":"x","vg_name":"v","pv_size":"100B","pv_free":"0B","dev_size":"0B","pv_attr":"a-m"}]}]}"#;
        let p = &parse_pvs(pvs).unwrap()[0];
        assert!(p.missing);
        assert_eq!(p.size, 100);
        let vgs = r#"{"report":[{"vg":[{"vg_name":"v","vg_attr":"wz-pn-","pv_count":"2","lv_count":"1"}]}],"log":[]}"#;
        let v = &parse_vgs(vgs).unwrap()[0];
        assert!(v.partial);
        assert_eq!(v.pv_count, 2);
        assert!(parse_pvs("not json").is_err());
        assert!(parse(Some("x"), None, None).is_empty());
    }

    #[test]
    fn device_mapper_names() {
        assert_eq!(
            split_dm_name("rynas_vol2-data"),
            Some(("rynas_vol2".into(), "data".into()))
        );
        assert_eq!(
            split_dm_name("my--vg-my--lv"),
            Some(("my-vg".into(), "my-lv".into()))
        );
        assert_eq!(split_dm_name("rynas-vol1"), Some(("rynas".into(), "vol1".into())));
        assert_eq!(split_dm_name("plain"), None);
        assert_eq!(split_dm_name("-x"), None);
    }

    #[test]
    fn arguments() {
        let a = args("pvs", &["/dev/md126".into(), "/dev/md127".into()]);
        assert_eq!(
            a.join(" "),
            format!("--reportformat json --units b --nosuffix --devices /dev/md126,/dev/md127 -o {PVS_FIELDS}")
        );
    }
}
