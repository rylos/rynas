//! The mount table (`/proc/self/mountinfo`) and which mounts a backup covers.

/// One line of mountinfo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub id: u32,
    pub parent: u32,
    /// `major:minor`.
    pub dev: String,
    /// The path inside the filesystem that is mounted (btrfs: the subvolume).
    pub root: String,
    pub path: String,
    pub options: String,
    pub fstype: String,
    pub source: String,
    pub super_options: String,
}

/// Undoes the octal escapes of mountinfo (`\040` is a space).
pub fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && let Some(oct) = b.get(i + 1..i + 4)
            && let Ok(v) = u8::from_str_radix(std::str::from_utf8(oct).unwrap_or("x"), 8)
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn parse(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|l| {
            let (a, b) = l.split_once(" - ")?;
            let f: Vec<&str> = a.split(' ').collect();
            let g: Vec<&str> = b.split(' ').collect();
            if f.len() < 6 || g.len() < 3 {
                return None;
            }
            Some(Mount {
                id: f[0].parse().ok()?,
                parent: f[1].parse().ok()?,
                dev: f[2].to_string(),
                root: unescape(f[3]),
                path: unescape(f[4]),
                options: f[5].to_string(),
                fstype: g[0].to_string(),
                source: unescape(g[1]),
                super_options: g[2].to_string(),
            })
        })
        .collect()
}

pub fn read() -> Vec<Mount> {
    parse(&std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default())
}

/// Filesystems a backup reads: disk-backed, local, the kinds the agent knows.
pub const BACKED_UP: &[&str] = &["btrfs", "ext2", "ext3", "ext4", "xfs", "vfat", "f2fs"];

/// Mount points never followed (they are not the PC's data).
const SKIPPED_PATHS: &[&str] = &["/proc", "/sys", "/dev", "/run", "/tmp", "/var/tmp", "/media", "/mnt", "/var/lib/docker", "/var/lib/containers", "/snap"];

/// The mounts to back up: the known disk filesystems, on a block device, not below a skipped
/// path, one per mount point (the topmost when stacked), and not removable media.
pub fn backup_mounts(all: &[Mount]) -> Vec<Mount> {
    let mut out: Vec<Mount> = Vec::new();
    for m in all {
        if !BACKED_UP.contains(&m.fstype.as_str()) || !m.source.starts_with("/dev/") {
            continue;
        }
        if m.path != "/" && SKIPPED_PATHS.iter().any(|s| m.path == *s || m.path.starts_with(&format!("{s}/"))) {
            continue;
        }
        if removable(&m.dev) {
            continue;
        }
        // a later mount on the same path hides the earlier one
        out.retain(|o| o.path != m.path);
        out.push(m.clone());
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// The block device `maj:min` (or the disk it is a partition of) is removable or USB.
pub fn removable(dev: &str) -> bool {
    let base = format!("/sys/dev/block/{dev}");
    let Ok(real) = std::fs::canonicalize(&base) else {
        return false;
    };
    let s = real.to_string_lossy();
    if s.contains("/usb") {
        return true;
    }
    let mut p = real.clone();
    for _ in 0..2 {
        if std::fs::read_to_string(p.join("removable")).is_ok_and(|v| v.trim() == "1") {
            return true;
        }
        if !p.pop() {
            break;
        }
    }
    false
}

/// The subvolume of a btrfs mount (`subvol=/@home` → `@home`; the root `/` → empty).
pub fn subvol(m: &Mount) -> String {
    m.super_options
        .split(',')
        .chain(m.options.split(','))
        .find_map(|o| o.strip_prefix("subvol="))
        .map(|s| s.trim_start_matches('/').to_string())
        .unwrap_or_else(|| m.root.trim_start_matches('/').to_string())
}

/// A path below a top level or a root (a manifest's, a mount point inside a subvolume), as
/// plain names joined by `/` (the leading and trailing `/` dropped; `""` is the top itself).
/// `None` for `..`, `.`, an empty component or a NUL.
pub fn safe_rel(p: &str) -> Option<String> {
    if p.contains('\0') {
        return None;
    }
    let t = p.trim_matches('/');
    if t.is_empty() {
        return Some(String::new());
    }
    t.split('/').all(|c| !c.is_empty() && c != "." && c != "..").then(|| t.to_string())
}

/// `child` is `parent` or below it: mount points (`/home`, `/home/me`) and paths from a top
/// level (`@`, `@/var/lib`) alike, with or without the slashes at the ends; `/` (or `""`)
/// holds everything. `/home` does not hold `/homework`.
pub fn under(parent: &str, child: &str) -> bool {
    let (p, c) = (parent.trim_matches('/'), child.trim_matches('/'));
    p.is_empty() || c == p || c.strip_prefix(p).is_some_and(|r| r.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MI: &str = "22 1 0:21 / / rw,relatime shared:1 - btrfs /dev/vda2 rw,compress=zstd:3,space_cache=v2,subvolid=256,subvol=/@
23 22 0:21 /@home /home rw,relatime shared:2 - btrfs /dev/vda2 rw,subvolid=257,subvol=/@home
24 22 253:1 / /boot/efi rw,relatime shared:3 - vfat /dev/vda1 rw,fmask=0022
25 22 0:5 / /proc rw - proc proc rw
26 22 0:30 / /run rw - tmpfs tmpfs rw
27 22 253:3 / /mnt/usb rw - ext4 /dev/sdb1 rw
28 22 253:4 / /srv/my\\040data rw - xfs /dev/vdb rw
";

    #[test]
    fn mounts() {
        let all = parse(MI);
        assert_eq!(all.len(), 7);
        assert_eq!(all[6].path, "/srv/my data");
        let b = backup_mounts(&all);
        let paths: Vec<&str> = b.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, vec!["/", "/boot/efi", "/home", "/srv/my data"]);
        assert_eq!(subvol(&b[0]), "@");
        assert_eq!(subvol(&b[2]), "@home");
        assert_eq!(subvol(&b[1]), "");
    }
}
