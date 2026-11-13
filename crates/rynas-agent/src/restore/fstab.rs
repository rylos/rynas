//! The PC's `/etc/fstab` (and the btrfs options of its kernel command line) as the restore
//! reads and changes them: one parser for every reader (the legacy layout, the commented
//! lines, the checks, the subvolume ID fixes).

/// One active fstab line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FstabLine {
    pub spec: String,
    pub path: String,
    pub fstype: String,
    pub options: String,
}

impl FstabLine {
    /// An active line (not a comment) with at least spec, mount point and type.
    pub fn parse(line: &str) -> Option<FstabLine> {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            return None;
        }
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() >= 3).then(|| FstabLine {
            spec: f[0].to_string(),
            path: f[1].to_string(),
            fstype: f[2].to_string(),
            options: f.get(3).copied().unwrap_or("defaults").to_string(),
        })
    }
    fn opt(&self, key: &str) -> Option<&str> {
        self.options.split(',').find_map(|o| o.strip_prefix(key)?.strip_prefix('='))
    }
    /// `subvol=/@home` → `@home`.
    pub fn subvol(&self) -> Option<String> {
        self.opt("subvol").map(|s| s.trim_start_matches('/').to_string())
    }
    pub fn subvolid(&self) -> Option<u64> {
        self.opt("subvolid").and_then(|n| n.parse().ok())
    }
    /// Mounts the top level (`subvolid=5`, `subvol=/`).
    pub fn top_level(&self) -> bool {
        self.subvolid() == Some(5) || self.subvol().is_some_and(|s| s.is_empty())
    }
    /// The ID of a `UUID=`, `PARTUUID=`, `LABEL=` spec, quotes dropped.
    pub fn id(&self, kind: &str) -> Option<String> {
        self.spec.strip_prefix(kind).map(|s| s.trim_matches('"').to_string())
    }
}

pub fn parse(text: &str) -> Vec<FstabLine> {
    text.lines().filter_map(FstabLine::parse).collect()
}

/// Every line through `f`, the final newline kept.
pub fn map_lines(text: &str, mut f: impl FnMut(&str) -> String) -> String {
    let mut t = text.lines().map(&mut f).collect::<Vec<_>>().join("\n");
    if text.ends_with('\n') {
        t.push('\n');
    }
    t
}

/// The line with its whitespace-separated field `idx` replaced (the rest as written).
fn replace_field(line: &str, idx: usize, new: &str) -> String {
    let mut n = 0;
    let mut in_field = false;
    for (i, ch) in line.char_indices() {
        if ch.is_whitespace() {
            in_field = false;
            continue;
        }
        if !in_field {
            in_field = true;
            if n == idx {
                let end = line[i..].find(char::is_whitespace).map_or(line.len(), |e| i + e);
                return format!("{}{new}{}", &line[..i], &line[end..]);
            }
            n += 1;
        }
    }
    line.to_string()
}

/// A btrfs option list without its subvolume ID (≠ 5): a restore makes every subvolume
/// anew (received, then a writable snapshot), so the IDs of the PC no longer match and the
/// kernel refuses `subvol=X,subvolid=N` ("does not match") or mounts another subvolume. With
/// `subvol=` present the ID goes; alone it becomes `subvol=/<path>` from `ids` (old ID,
/// path from the top level), and stays (for the checks to report) when unknown.
pub fn fix_subvolid_opts(opts: &str, ids: &[(u64, String)]) -> String {
    let has_subvol = opts.split(',').any(|o| o.starts_with("subvol="));
    let mut out: Vec<String> = Vec::new();
    for o in opts.split(',') {
        match o.strip_prefix("subvolid=").and_then(|n| n.parse::<u64>().ok()) {
            None | Some(5) => out.push(o.to_string()),
            Some(_) if has_subvol => {}
            Some(n) => match ids.iter().find(|(i, _)| *i == n) {
                Some((_, p)) => out.push(format!("subvol=/{}", p.trim_start_matches('/'))),
                None => out.push(o.to_string()),
            },
        }
    }
    out.join(",")
}

/// [`fix_subvolid_opts`] on every active btrfs line; `ids_for(spec)` gives the IDs of the
/// filesystem a line names.
pub fn fix_subvolids(fstab: &str, ids_for: &dyn Fn(&str) -> Vec<(u64, String)>) -> String {
    map_lines(fstab, |l| match FstabLine::parse(l) {
        Some(f) if f.fstype == "btrfs" && f.subvolid().is_some_and(|n| n != 5) => replace_field(l, 3, &fix_subvolid_opts(&f.options, &ids_for(&f.spec))),
        _ => l.to_string(),
    })
}

/// [`fix_subvolid_opts`] on every `rootflags=` of a kernel command line (also inside
/// `GRUB_CMDLINE_LINUX="…"` and a loader entry's `options` line).
pub fn fix_rootflags(text: &str, ids: &[(u64, String)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("rootflags=") {
        let start = i + "rootflags=".len();
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = tail.find(|c: char| c.is_whitespace() || c == '"' || c == '\'').unwrap_or(tail.len());
        out.push_str(&fix_subvolid_opts(&tail[..end], ids));
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// A subvolume ID other than the top level's (5) still named: on an active btrfs fstab
/// line, or in a `rootflags=` of a command line. Such an ID points at a subvolume that is
/// not there after a restore.
pub fn stale_subvolids(fstab: Option<&str>, cmdline: Option<&str>) -> Vec<String> {
    let mut v = Vec::new();
    for f in parse(fstab.unwrap_or("")) {
        if f.fstype == "btrfs"
            && let Some(n) = f.subvolid().filter(|n| *n != 5)
        {
            v.push(format!("{} still mounts subvolid={n}", f.path));
        }
    }
    let mut rest = cmdline.unwrap_or("");
    while let Some(i) = rest.find("rootflags=") {
        let tail = &rest[i + "rootflags=".len()..];
        let end = tail.find(|c: char| c.is_whitespace() || c == '"' || c == '\'').unwrap_or(tail.len());
        for n in tail[..end].split(',').filter_map(|o| o.strip_prefix("subvolid=")?.parse::<u64>().ok()) {
            if n != 5 {
                v.push(format!("rootflags still names subvolid={n}"));
            }
        }
        rest = &tail[end..];
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_and_options() {
        let l = FstabLine::parse("UUID=\"ab-cd\"\t/home  btrfs rw,subvolid=257,subvol=/@home 0 0").unwrap();
        assert_eq!(l.id("UUID=").as_deref(), Some("ab-cd"));
        assert_eq!(l.subvol().as_deref(), Some("@home"));
        assert_eq!(l.subvolid(), Some(257));
        assert!(!l.top_level());
        assert!(FstabLine::parse("LABEL=data /mnt/pool btrfs subvolid=5 0 0").unwrap().top_level());
        assert_eq!(FstabLine::parse("LABEL=data /data ext4").unwrap().options, "defaults");
        assert!(FstabLine::parse("# UUID=x / btrfs rw 0 0").is_none());
        // `subvolume=` is not `subvol=`
        assert_eq!(FstabLine::parse("x /a btrfs subvolume=q 0 0").unwrap().subvol(), None);
    }

    #[test]
    fn subvolume_ids_go() {
        let ids = vec![(256, "@".to_string()), (257, "@home".to_string())];
        assert_eq!(fix_subvolid_opts("rw,noatime,subvolid=256,subvol=/@", &ids), "rw,noatime,subvol=/@");
        assert_eq!(fix_subvolid_opts("rw,subvolid=257", &ids), "rw,subvol=/@home");
        assert_eq!(fix_subvolid_opts("rw,subvolid=999", &ids), "rw,subvolid=999");
        assert_eq!(fix_subvolid_opts("rw,subvolid=5", &ids), "rw,subvolid=5");
        // genfstab's lines, whitespace kept
        let fstab = "# /dev/nvme0n1p2\nUUID=2d5c\t/\tbtrfs\trw,relatime,ssd,subvolid=256,subvol=/@\t0 0\nUUID=2d5c /home btrfs rw,subvolid=257 0 0\nUUID=91BC /boot vfat rw 0 2\n";
        let fixed = fix_subvolids(fstab, &|_| ids.clone());
        assert_eq!(fixed, "# /dev/nvme0n1p2\nUUID=2d5c\t/\tbtrfs\trw,relatime,ssd,subvol=/@\t0 0\nUUID=2d5c /home btrfs rw,subvol=/@home 0 0\nUUID=91BC /boot vfat rw 0 2\n");
        assert!(stale_subvolids(Some(&fixed), None).is_empty());
        assert_eq!(stale_subvolids(Some(fstab), None).len(), 2);
        // the command line
        let grub = "GRUB_CMDLINE_LINUX=\"root=UUID=2d5c rootflags=subvol=@,subvolid=256 quiet\"";
        let g = fix_rootflags(grub, &ids);
        assert_eq!(g, "GRUB_CMDLINE_LINUX=\"root=UUID=2d5c rootflags=subvol=@ quiet\"");
        assert_eq!(fix_rootflags("root=UUID=x rootflags=subvolid=256", &ids), "root=UUID=x rootflags=subvol=/@");
        assert!(stale_subvolids(None, Some(&g)).is_empty());
        assert_eq!(stale_subvolids(None, Some("rootflags=subvolid=300")).len(), 1);
    }
}
