//! Block devices from sysfs and the superblocks the manifest and the restore need: ext2/3/4,
//! btrfs, XFS, FAT, swap and LUKS (version, cipher, UUID, label; never a key slot). Read
//! directly, so neither blkid nor cryptsetup has to be there.

use std::fs::File;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

/// What a device holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Probe {
    pub fstype: String,
    pub uuid: String,
    pub label: String,
    pub features: Vec<String>,
    /// LUKS: version, cipher, key bits, pbkdf.
    pub luks: Option<(u32, String, u32, String)>,
}

fn read_at(f: &File, off: u64, n: usize) -> Option<Vec<u8>> {
    let mut b = vec![0u8; n];
    f.read_exact_at(&mut b, off).ok()?;
    Some(b)
}

fn uuid_text(b: &[u8]) -> String {
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Reads the superblock of a device or image.
pub fn probe(dev: &Path) -> Option<Probe> {
    let f = File::open(dev).ok()?;
    probe_file(&f)
}

pub fn probe_file(f: &File) -> Option<Probe> {
    // LUKS
    if let Some(h) = read_at(f, 0, 512)
        && &h[0..6] == b"LUKS\xba\xbe"
    {
        let version = u16::from_be_bytes([h[6], h[7]]) as u32;
        let uuid = cstr(&h[168..208]);
        if version == 1 {
            let cipher = format!("{}-{}", cstr(&h[8..40]), cstr(&h[40..72]));
            let bits = u32::from_be_bytes([h[108], h[109], h[110], h[111]]) * 8;
            return Some(Probe {
                fstype: "crypto_LUKS".into(),
                uuid,
                luks: Some((1, cipher, bits, "pbkdf2".into())),
                ..Default::default()
            });
        }
        let label = cstr(&h[24..72]);
        let hdr_size = u64::from_be_bytes(h[8..16].try_into().ok()?);
        let json = read_at(f, 4096, (hdr_size.saturating_sub(4096)).min(1 << 20) as usize).unwrap_or_default();
        let v: serde_json::Value = serde_json::from_slice(&json[..json.iter().position(|&c| c == 0).unwrap_or(json.len())]).unwrap_or_default();
        let seg = v["segments"].as_object().and_then(|m| m.values().next().cloned()).unwrap_or_default();
        let cipher = seg["encryption"].as_str().unwrap_or("").to_string();
        let slot = v["keyslots"].as_object().and_then(|m| m.values().next().cloned()).unwrap_or_default();
        let bits = slot["key_size"].as_u64().unwrap_or(0) as u32 * 8;
        let pbkdf = slot["kdf"]["type"].as_str().unwrap_or("").to_string();
        return Some(Probe {
            fstype: "crypto_LUKS".into(),
            uuid,
            label,
            luks: Some((2, cipher, bits, pbkdf)),
            ..Default::default()
        });
    }
    // ext2/3/4
    if let Some(sb) = read_at(f, 1024, 1024)
        && sb[56..58] == [0x53, 0xEF]
    {
        let compat = le32(&sb, 92);
        let incompat = le32(&sb, 96);
        let ro = le32(&sb, 100);
        let mut features = Vec::new();
        for (bit, name, set) in [
            (0x4, "has_journal", compat),
            (0x40, "extents", incompat),
            (0x80, "64bit", incompat),
            (0x200, "flex_bg", incompat),
            (0x400, "metadata_csum", ro),
            (0x1, "sparse_super", ro),
            (0x8, "huge_file", ro),
        ] {
            if set & bit != 0 {
                features.push(name.to_string());
            }
        }
        let fstype = if incompat & (0x40 | 0x80 | 0x200) != 0 || ro & 0x400 != 0 {
            "ext4"
        } else if compat & 0x4 != 0 {
            "ext3"
        } else {
            "ext2"
        };
        return Some(Probe {
            fstype: fstype.into(),
            uuid: uuid_text(&sb[104..120]),
            label: cstr(&sb[120..136]),
            features,
            luks: None,
        });
    }
    // btrfs
    if let Some(sb) = read_at(f, 65536, 4096)
        && &sb[64..72] == b"_BHRfS_M"
    {
        return Some(Probe {
            fstype: "btrfs".into(),
            uuid: uuid_text(&sb[32..48]),
            label: cstr(&sb[299..299 + 256]),
            ..Default::default()
        });
    }
    if let Some(sb) = read_at(f, 0, 4096) {
        // XFS
        if &sb[0..4] == b"XFSB" {
            return Some(Probe {
                fstype: "xfs".into(),
                uuid: uuid_text(&sb[32..48]),
                label: cstr(&sb[108..120]),
                ..Default::default()
            });
        }
        // swap (4 KiB pages)
        if &sb[4086..4096] == b"SWAPSPACE2" {
            return Some(Probe {
                fstype: "swap".into(),
                uuid: uuid_text(&sb[1036..1052]),
                label: cstr(&sb[1052..1068]),
                ..Default::default()
            });
        }
        // FAT
        if sb[510..512] == [0x55, 0xAA] {
            let fat32 = &sb[82..87] == b"FAT32";
            let fat16 = &sb[54..57] == b"FAT";
            if fat32 || fat16 {
                let (id_at, label_at) = if fat32 { (67, 71) } else { (39, 43) };
                let id = le32(&sb, id_at);
                let label = cstr(&sb[label_at..label_at + 11]);
                return Some(Probe {
                    fstype: "vfat".into(),
                    uuid: format!("{:04X}-{:04X}", id >> 16, id & 0xffff),
                    label: if label == "NO NAME" { String::new() } else { label },
                    features: vec![if fat32 { "fat32" } else { "fat16" }.to_string()],
                    luks: None,
                });
            }
        }
    }
    None
}

/// A block device as sysfs sees it.
#[derive(Debug, Clone, Default)]
pub struct Block {
    /// Kernel name (`nvme0n1`, `nvme0n1p2`, `dm-0`).
    pub name: String,
    pub sys: PathBuf,
    pub size_bytes: u64,
    pub logical_sector: u32,
    /// Partition number, for a partition.
    pub partition: Option<u32>,
    /// The disk of a partition.
    pub parent: Option<String>,
    /// `dm/uuid` of a device-mapper device (`CRYPT-LUKS2-…`, `LVM-…`).
    pub dm_uuid: Option<String>,
    pub dm_name: Option<String>,
    /// The devices below a dm or md device.
    pub slaves: Vec<String>,
}

fn read_trim(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// The block device of `maj:min`.
pub fn block_of_devnum(dev: &str) -> Option<Block> {
    let sys = std::fs::canonicalize(format!("/sys/dev/block/{dev}")).ok()?;
    block_at(&sys)
}

/// The block device of a `/dev/…` path.
pub fn block_of_path(p: &Path) -> Option<Block> {
    let m = std::fs::metadata(p).ok()?;
    let rdev = m.rdev();
    block_of_devnum(&format!("{}:{}", libc::major(rdev), libc::minor(rdev)))
}

pub fn block_at(sys: &Path) -> Option<Block> {
    let name = sys.file_name()?.to_string_lossy().into_owned();
    let size = read_trim(&sys.join("size")).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0) * 512;
    let partition = read_trim(&sys.join("partition")).and_then(|s| s.parse().ok());
    let parent = partition.and_then(|_| sys.parent()?.file_name().map(|n| n.to_string_lossy().into_owned()));
    let queue = match &parent {
        Some(_) => sys.parent()?.join("queue/logical_block_size"),
        None => sys.join("queue/logical_block_size"),
    };
    let slaves = std::fs::read_dir(sys.join("slaves"))
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    Some(Block {
        name,
        sys: sys.to_path_buf(),
        size_bytes: size,
        logical_sector: read_trim(&queue).and_then(|s| s.parse().ok()).unwrap_or(512),
        partition,
        parent,
        dm_uuid: read_trim(&sys.join("dm/uuid")),
        dm_name: read_trim(&sys.join("dm/name")),
        slaves,
    })
}

/// Whole disks: `/sys/block/*` without loop, ram, zram, dm, md, sr devices.
pub fn disks() -> Vec<Block> {
    let mut v: Vec<Block> = std::fs::read_dir("/sys/block")
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    let skip = ["loop", "ram", "zram", "dm-", "md", "sr", "fd", "nbd"].iter().any(|p| n.starts_with(p));
                    if skip {
                        return None;
                    }
                    block_at(&std::fs::canonicalize(e.path()).ok()?)
                })
                .collect()
        })
        .unwrap_or_default();
    v.retain(|b| b.size_bytes > 0);
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// Model, serial and WWN of a disk (sysfs, NVMe and SCSI layouts).
pub fn identity(b: &Block) -> (String, String, String) {
    let d = b.sys.join("device");
    let model = read_trim(&d.join("model")).unwrap_or_default();
    let serial = read_trim(&d.join("serial"))
        .or_else(|| read_trim(&b.sys.join("serial")))
        .or_else(|| {
            // virtio and SCSI: the serial is in the VPD page or the by-id link
            by_id_serial(&b.name)
        })
        .unwrap_or_default();
    let wwn = read_trim(&b.sys.join("wwid")).or_else(|| read_trim(&d.join("wwid"))).unwrap_or_default();
    (model, serial, wwn)
}

/// The serial part of a `/dev/disk/by-id/<bus>-<model>_<serial>` link pointing at `name`.
fn by_id_serial(name: &str) -> Option<String> {
    for e in std::fs::read_dir("/dev/disk/by-id").ok()?.flatten() {
        let link = std::fs::read_link(e.path()).ok()?;
        if link.file_name().is_some_and(|n| n.to_string_lossy() == name) {
            let id = e.file_name().to_string_lossy().into_owned();
            if id.starts_with("wwn-") || id.contains("-part") {
                continue;
            }
            if let Some(s) = id.strip_prefix("virtio-") {
                return Some(s.to_string());
            }
            return id.rsplit('_').next().map(str::to_string);
        }
    }
    None
}

/// The partitions of a disk, in order.
pub fn partitions(disk: &Block) -> Vec<Block> {
    let mut v: Vec<Block> = std::fs::read_dir(&disk.sys)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().join("partition").exists())
                .filter_map(|e| block_at(&e.path()))
                .collect()
        })
        .unwrap_or_default();
    v.sort_by_key(|b| b.partition.unwrap_or(0));
    v
}

/// The device node of a kernel name.
pub fn dev_path(name: &str) -> PathBuf {
    PathBuf::from("/dev").join(name)
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn image(f: impl FnOnce(&mut Vec<u8>)) -> tempfile::NamedTempFile {
        let mut v = vec![0u8; 1 << 17];
        f(&mut v);
        let mut t = tempfile::NamedTempFile::new().unwrap();
        t.write_all(&v).unwrap();
        t
    }

    #[test]
    fn superblocks() {
        let ext = image(|v| {
            v[1024 + 56] = 0x53;
            v[1024 + 57] = 0xEF;
            v[1024 + 96] = 0x40;
            v[1024 + 92] = 0x4;
            v[1024 + 104..1024 + 120].copy_from_slice(&[0x11; 16]);
            v[1024 + 120..1024 + 124].copy_from_slice(b"root");
        });
        let p = probe(ext.path()).unwrap();
        assert_eq!((p.fstype.as_str(), p.label.as_str()), ("ext4", "root"));
        assert_eq!(p.uuid, "11111111-1111-1111-1111-111111111111");
        let bt = image(|v| {
            v[65536 + 64..65536 + 72].copy_from_slice(b"_BHRfS_M");
            v[65536 + 32] = 0xab;
        });
        assert_eq!(probe(bt.path()).unwrap().fstype, "btrfs");
        let fat = image(|v| {
            v[82..87].copy_from_slice(b"FAT32");
            v[510] = 0x55;
            v[511] = 0xAA;
            v[67..71].copy_from_slice(&0x91BC_A784u32.to_le_bytes());
            v[71..82].copy_from_slice(b"EFI        ");
        });
        let p = probe(fat.path()).unwrap();
        assert_eq!((p.uuid.as_str(), p.label.as_str()), ("91BC-A784", "EFI"));
        let none = image(|_| {});
        assert!(probe(none.path()).is_none());
    }
}
