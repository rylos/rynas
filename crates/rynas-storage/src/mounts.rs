//! `/proc/self/mountinfo` with the mount and superblock options (btrfs puts `compress=`,
//! `subvolid=` and `subvol=` there).

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Mount {
    /// "major:minor" of the superblock (btrfs: an anonymous device, not the block device).
    pub majmin: String,
    pub root: String,
    pub mountpoint: String,
    pub fstype: String,
    pub source: String,
    /// Per-mount options followed by superblock options.
    pub options: Vec<String>,
}

impl Mount {
    pub fn option(&self, key: &str) -> Option<&str> {
        self.options
            .iter()
            .find_map(|o| o.strip_prefix(key)?.strip_prefix('='))
    }
    /// btrfs subvolume id of this mount, 5 for the top level.
    pub fn subvolid(&self) -> Option<u64> {
        self.option("subvolid").and_then(|v| v.parse().ok())
    }
}

/// Undo the octal escapes of mountinfo (`\040` = space).
pub fn unescape(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The system's mount table: init's, not this process' own.
pub const HOST_MOUNTINFO: &str = "/proc/1/mountinfo";
/// This process' own mount table (its mount namespace).
pub const OWN_MOUNTINFO: &str = "/proc/self/mountinfo";

/// What the system has mounted, as the host sees it. rynasd runs in a sandboxed mount
/// namespace (`PrivateTmp=`, `ProtectSystem=`, `ReadWritePaths=`) where systemd adds bind
/// mounts of the same filesystems: read from there an ext4 root also looks mounted on
/// `/var/tmp` (from a private subdirectory), `/usr`, `/boot` and the state folders, and a
/// volume list shows a foreign volume named by its UUID (GitHub issue #14). init's table
/// (`/proc/1/mountinfo`, readable by rynasd as root) has none of these. Falls back to our own
/// table when init's cannot be read (tests, containers without the right).
pub fn read_host_mountinfo(read: impl Fn(&str) -> Option<String>) -> String {
    read(HOST_MOUNTINFO)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| read(OWN_MOUNTINFO))
        .unwrap_or_default()
}

/// [`read_host_mountinfo`] from the real `/proc`.
pub fn host_mountinfo() -> String {
    read_host_mountinfo(|p| std::fs::read_to_string(p).ok())
}

/// Parses mountinfo. A mount point listed twice with the same superblock (a mount stacked on
/// the same filesystem, e.g. an ESP mounted twice) is kept once, as the topmost (last) entry.
pub fn parse_mountinfo(s: &str) -> Vec<Mount> {
    let all = parse_all(s);
    let mut out: Vec<Mount> = Vec::with_capacity(all.len());
    for m in all {
        match out.iter_mut().find(|o| o.majmin == m.majmin && o.mountpoint == m.mountpoint) {
            Some(o) => *o = m,
            None => out.push(m),
        }
    }
    out
}

fn parse_all(s: &str) -> Vec<Mount> {
    s.lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let l: Vec<&str> = left.split(' ').collect();
            let r: Vec<&str> = right.split(' ').collect();
            if l.len() < 6 || r.len() < 2 {
                return None;
            }
            let mut options: Vec<String> = l[5].split(',').map(str::to_string).collect();
            if let Some(sup) = r.get(2) {
                for o in sup.split(',') {
                    if !options.iter().any(|x| x == o) {
                        options.push(o.to_string());
                    }
                }
            }
            Some(Mount {
                majmin: l[2].to_string(),
                root: unescape(l[3]),
                mountpoint: unescape(l[4]),
                fstype: r[0].to_string(),
                source: unescape(r[1]),
                options,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn btrfs_options() {
        let m = parse_mountinfo(include_str!("../tests/fixtures/proc-self-mountinfo.txt"));
        let vol = m.iter().find(|m| m.mountpoint == "/vol1").unwrap();
        assert_eq!(vol.fstype, "btrfs");
        assert_eq!(vol.source, "/dev/mapper/rynas-vol1");
        assert_eq!(vol.subvolid(), Some(5));
        assert_eq!(vol.option("compress"), Some("zstd:1"));
        assert!(vol.options.iter().any(|o| o == "noatime"));
        let root = m.iter().find(|m| m.mountpoint == "/").unwrap();
        assert_eq!(root.root, "/@rootfs");
        assert_eq!(root.subvolid(), Some(256));
        let share = m.iter().find(|m| m.mountpoint == "/srv/nas share").unwrap();
        assert_eq!(share.fstype, "cifs");
    }

    /// GitHub issue #14: an ext4 root seen from rynasd's sandboxed namespace is also mounted
    /// on `/var/tmp` (from a private subdirectory) and other places; init's table is used.
    #[test]
    fn host_table_wins_over_the_sandbox_and_duplicates_go() {
        let own = "\
21 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw
22 21 259:2 /var/tmp/systemd-private-x-rynasd.service-y/tmp /var/tmp rw shared:2 - ext4 /dev/nvme0n1p2 rw
23 21 259:2 /usr /usr ro shared:3 - ext4 /dev/nvme0n1p2 rw
24 21 259:1 / /boot/efi rw shared:4 - vfat /dev/nvme0n1p1 rw
25 24 259:1 / /boot/efi rw shared:5 - vfat /dev/nvme0n1p1 rw
";
        let host = "\
21 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw
24 21 259:1 / /boot/efi rw shared:4 - vfat /dev/nvme0n1p1 rw
25 24 259:1 / /boot/efi rw shared:5 - vfat /dev/nvme0n1p1 rw
";
        let read = |p: &str| match p {
            HOST_MOUNTINFO => Some(host.to_string()),
            OWN_MOUNTINFO => Some(own.to_string()),
            _ => None,
        };
        let m = parse_mountinfo(&read_host_mountinfo(read));
        let points: Vec<&str> = m.iter().map(|m| m.mountpoint.as_str()).collect();
        // no /var/tmp or /usr bind, the ESP once
        assert_eq!(points, ["/", "/boot/efi"]);
        // init's table unreadable: our own, still deduplicated
        let only_own = |p: &str| (p == OWN_MOUNTINFO).then(|| own.to_string());
        let m = parse_mountinfo(&read_host_mountinfo(only_own));
        assert_eq!(m.iter().filter(|m| m.mountpoint == "/boot/efi").count(), 1);
        assert_eq!(m.len(), 4);
    }
}
