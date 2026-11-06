//! Where and how a removable volume is mounted (decisions: `docs/research/removable-drives.md`).
//!
//! - **Where**: `/media/<name>`, the FHS place for removable media. `<name>` is the label
//!   cleaned the way udisks does it (`/`, quotes, backslashes and control characters become
//!   `_`; a leading dot too, so the folder is not hidden), else the filesystem UUID, else the
//!   kernel name. A taken name gets `-2`, `-3`… The name chosen for a UUID is remembered
//!   ([`Names`]), so the same drive comes back at the same path (shares, scripts, backups).
//! - **How**: `nosuid,nodev,noexec,noatime` everywhere (nothing on a stick runs on the NAS,
//!   no device nodes, no setuid; no access-time writes on flash). Filesystems without Unix
//!   owners (FAT, exFAT, NTFS, HFS+, ISO 9660, UDF) are owned by root and the access group
//!   of the settings, group read-write (`fmask=0117,dmask=0007`), nothing for others; ext4
//!   keeps its owners and remounts read-only on errors; HFS+, ISO 9660 and UDF are mounted
//!   read-only.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where removable volumes are mounted.
pub const MOUNT_ROOT: &str = "/media";

/// Options every removable mount gets.
pub const COMMON: [&str; 4] = ["nosuid", "nodev", "noexec", "noatime"];

/// A mount to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountSpec {
    /// `-t` type (the driver: `ntfs3`, `ntfs-3g`, `vfat`...).
    pub fs_type: String,
    pub options: Vec<String>,
    pub read_only: bool,
}

impl MountSpec {
    pub fn options_arg(&self) -> String {
        self.options.join(",")
    }
}

/// Filesystems the kernel can only mount read-only, or that rynas mounts read-only.
pub fn always_read_only(fs: &str) -> bool {
    matches!(fs, "hfsplus" | "iso9660" | "udf")
}

/// The mount of a filesystem of type `fs` with the driver `driver`, owned by group `gid`.
pub fn spec(fs: &str, driver: &str, gid: u32, read_only: bool) -> MountSpec {
    let read_only = read_only || always_read_only(fs);
    let mut o: Vec<String> = Vec::new();
    if read_only {
        o.push("ro".into());
    }
    o.extend(COMMON.iter().map(|s| s.to_string()));
    let owners = || vec!["uid=0".to_string(), format!("gid={gid}")];
    let masks = ["fmask=0117", "dmask=0007"];
    match driver {
        "vfat" => {
            o.extend(owners());
            o.extend(masks.iter().map(|s| s.to_string()));
            o.extend(["utf8=1", "shortname=mixed", "flush", "errors=remount-ro"].map(String::from));
        }
        "exfat" => {
            o.extend(owners());
            o.extend(masks.iter().map(|s| s.to_string()));
            o.extend(["iocharset=utf8", "errors=remount-ro"].map(String::from));
        }
        "exfat-fuse" => {
            o.extend(owners());
            o.extend(masks.iter().map(|s| s.to_string()));
        }
        "ntfs3" => {
            o.extend(owners());
            o.extend(masks.iter().map(|s| s.to_string()));
            o.extend(["iocharset=utf8", "windows_names"].map(String::from));
        }
        "ntfs-3g" => {
            o.extend(owners());
            o.extend(masks.iter().map(|s| s.to_string()));
            o.push("windows_names".into());
        }
        "ext2" | "ext3" | "ext4" => o.push("errors=remount-ro".into()),
        "hfsplus" => {
            o.extend(owners());
            o.push("umask=0007".into());
            o.push("nls=utf8".into());
        }
        "iso9660" => {
            o.extend(owners());
            o.extend(["iocharset=utf8", "mode=0440", "dmode=0550"].map(String::from));
        }
        "udf" => {
            o.extend(owners());
            o.push("iocharset=utf8".into());
        }
        _ => {}
    }
    MountSpec {
        fs_type: driver.to_string(),
        options: o,
        read_only,
    }
}

/// The ntfs3 refusal of a volume that Windows left dirty or hibernated (Fast Startup):
/// it can still be mounted read-only.
pub fn ntfs_needs_windows(reason: &str) -> bool {
    let r = reason.to_ascii_lowercase();
    r.contains("dirty")
        || r.contains("can't mount rw")
        || r.contains("hiberna")
        || r.contains("unclean")
}

/// A label made safe as one path component.
pub fn clean_name(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == '"' || c == '\'' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    out = out.trim().to_string();
    if out.starts_with('.') {
        out.replace_range(..1, "_");
    }
    // 64 characters at most, cut on a character boundary
    if let Some((i, _)) = out.char_indices().nth(64) {
        out.truncate(i);
    }
    out
}

/// The base name of the mount point of a volume.
pub fn base_name(label: &str, uuid: Option<&str>, kernel: &str) -> String {
    let l = clean_name(label);
    if !l.is_empty() {
        return l;
    }
    match uuid.map(clean_name) {
        Some(u) if !u.is_empty() => u,
        _ => kernel.to_string(),
    }
}

/// Names chosen for filesystem UUIDs, so a drive comes back at the same path. Kept in
/// rynasd's state directory (`removable/names.json`), at most [`Names::CAP`] entries (the
/// least recently used go first).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Names {
    /// uuid → (name, last use in ms).
    pub by_uuid: BTreeMap<String, (String, u64)>,
}

impl Names {
    pub const CAP: usize = 256;

    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = path.with_extension("json.new");
        std::fs::write(&tmp, serde_json::to_vec(self).unwrap_or_default())?;
        std::fs::rename(tmp, path)
    }

    /// The mount point for a volume: its remembered name when free, else the base name
    /// with the first free suffix. `taken` are the mount points in use; `exists` tells
    /// whether a path is an existing non-empty directory (not usable either).
    pub fn choose(
        &mut self,
        label: &str,
        uuid: Option<&str>,
        kernel: &str,
        taken: &HashSet<String>,
        exists: &dyn Fn(&str) -> bool,
        now_ms: u64,
    ) -> String {
        let free = |name: &str| {
            let p = format!("{MOUNT_ROOT}/{name}");
            !taken.contains(&p) && !exists(&p)
        };
        let base = base_name(label, uuid, kernel);
        let remembered = uuid
            .and_then(|u| self.by_uuid.get(u))
            .map(|(n, _)| n.clone())
            // a relabelled drive gets its new label
            .filter(|n| n == &base || n.starts_with(&format!("{base}-")));
        let name = match remembered {
            Some(n) if free(&n) => n,
            _ => {
                let mut n = base.clone();
                let mut i = 2;
                while !free(&n) && i < 1000 {
                    n = format!("{base}-{i}");
                    i += 1;
                }
                n
            }
        };
        if let Some(u) = uuid {
            self.by_uuid.insert(u.to_string(), (name.clone(), now_ms));
            while self.by_uuid.len() > Self::CAP {
                let oldest = self
                    .by_uuid
                    .iter()
                    .min_by_key(|(_, (_, t))| *t)
                    .map(|(k, _)| k.clone());
                match oldest {
                    Some(k) => self.by_uuid.remove(&k),
                    None => break,
                };
            }
        }
        format!("{MOUNT_ROOT}/{name}")
    }
}

/// `true` when `path` (below `root`) is a directory with something in it.
pub fn non_empty_dir(root: &Path, path: &str) -> bool {
    let p: PathBuf = root.join(path.trim_start_matches('/'));
    std::fs::read_dir(p)
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
}

/// The commands that mount `dev` at `dir`: a transient systemd mount unit (bound to the
/// device: unplugging stops it) or, without systemd as PID 1, plain `mount`.
pub fn mount_argv(systemd: bool, dev: &str, dir: &str, spec: &MountSpec) -> Vec<String> {
    let mut v: Vec<String> = if systemd {
        vec![
            "systemd-mount".into(),
            "--no-ask-password".into(),
            "--collect".into(),
            "--fsck=no".into(),
            "-t".into(),
            spec.fs_type.clone(),
            "-o".into(),
            spec.options_arg(),
        ]
    } else {
        vec![
            "mount".into(),
            "-t".into(),
            spec.fs_type.clone(),
            "-o".into(),
            spec.options_arg(),
        ]
    };
    v.push(dev.into());
    v.push(dir.into());
    v
}

/// The command that unmounts `dir` (never lazy: a busy volume stays mounted).
pub fn umount_argv(systemd: bool, dir: &str) -> Vec<String> {
    if systemd {
        vec!["systemd-mount".into(), "--umount".into(), dir.into()]
    } else {
        vec!["umount".into(), dir.into()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options() {
        let s = spec("vfat", "vfat", 100, false);
        assert_eq!(
            s.options_arg(),
            "nosuid,nodev,noexec,noatime,uid=0,gid=100,fmask=0117,dmask=0007,utf8=1,shortname=mixed,flush,errors=remount-ro"
        );
        let s = spec("ntfs", "ntfs3", 7, true);
        assert!(s.read_only && s.options[0] == "ro");
        assert!(s.options.contains(&"windows_names".to_string()));
        let s = spec("ext4", "ext4", 100, false);
        assert_eq!(
            s.options_arg(),
            "nosuid,nodev,noexec,noatime,errors=remount-ro"
        );
        let s = spec("iso9660", "iso9660", 100, false);
        assert!(s.read_only && s.options_arg().starts_with("ro,"));
        assert_eq!(
            spec("btrfs", "btrfs", 1, false).options_arg(),
            "nosuid,nodev,noexec,noatime"
        );
    }

    #[test]
    fn ntfs_refusals() {
        assert!(ntfs_needs_windows(
            "ntfs3: sdc1: volume is dirty and \"force\" flag is not set!"
        ));
        assert!(ntfs_needs_windows(
            "ntfs3(sdc1): It is recommended to use chkdsk. Can't mount rw!"
        ));
        assert!(!ntfs_needs_windows("wrong fs type"));
    }

    #[test]
    fn names() {
        assert_eq!(clean_name(" My/Stick\"s "), "My_Stick_s");
        assert_eq!(clean_name(".hidden"), "_hidden");
        assert_eq!(clean_name(&"x".repeat(80)).len(), 64);
        assert_eq!(base_name("", Some("1234-ABCD"), "sdc1"), "1234-ABCD");
        assert_eq!(base_name("", None, "sdc1"), "sdc1");

        let mut n = Names::default();
        let none = |_: &str| false;
        let mut taken = HashSet::new();
        let a = n.choose("STICK", Some("u1"), "sdc1", &taken, &none, 1);
        assert_eq!(a, "/media/STICK");
        taken.insert(a);
        // another drive with the same label
        let b = n.choose("STICK", Some("u2"), "sdd1", &taken, &none, 2);
        assert_eq!(b, "/media/STICK-2");
        // u2 alone later: it keeps its remembered name
        let c = n.choose("STICK", Some("u2"), "sdd1", &HashSet::new(), &none, 3);
        assert_eq!(c, "/media/STICK-2");
        // a non-empty directory is not used
        let busy = |p: &str| p == "/media/DATA";
        assert_eq!(
            n.choose("DATA", None, "sde1", &HashSet::new(), &busy, 4),
            "/media/DATA-2"
        );
        // relabelled: the new label wins
        assert_eq!(
            n.choose("NEW", Some("u1"), "sdc1", &HashSet::new(), &none, 5),
            "/media/NEW"
        );
    }

    #[test]
    fn names_are_capped() {
        let mut n = Names::default();
        for i in 0..(Names::CAP + 10) {
            n.choose(
                "x",
                Some(&format!("u{i}")),
                "sdc1",
                &HashSet::new(),
                &|_| false,
                i as u64,
            );
        }
        assert_eq!(n.by_uuid.len(), Names::CAP);
        assert!(!n.by_uuid.contains_key("u0"));
    }

    #[test]
    fn commands() {
        let s = spec("vfat", "vfat", 100, false);
        let a = mount_argv(true, "/dev/sdc1", "/media/S", &s);
        assert_eq!(a[0], "systemd-mount");
        assert!(a.contains(&"--fsck=no".to_string()));
        assert_eq!(&a[a.len() - 2..], ["/dev/sdc1", "/media/S"]);
        assert_eq!(mount_argv(false, "/dev/sdc1", "/media/S", &s)[0], "mount");
        assert_eq!(
            umount_argv(true, "/media/S"),
            ["systemd-mount", "--umount", "/media/S"]
        );
        assert_eq!(umount_argv(false, "/media/S"), ["umount", "/media/S"]);
    }
}
