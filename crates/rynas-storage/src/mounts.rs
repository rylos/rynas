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

pub fn parse_mountinfo(s: &str) -> Vec<Mount> {
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
}
