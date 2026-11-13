//! btrbk's view of the PC, for the btrfs method (`docs/research/pc-backup.md` §11.9).
//!
//! - [`parse_conf`] reads the subvolume sections of a `btrbk.conf` (inherited `snapshot_dir`,
//!   `snapshot_name`, targets); [`manifest_block`] turns them into the manifest's btrbk block
//!   (btrbk name → subvolume path from the top level → mount point) for PCs that still push
//!   with btrbk over SSH (`rynas-agent manifest`);
//! - [`layout`] finds the subvolumes worth sending from the mount table (generic: no names,
//!   only mount points decide what is cache or log); the agent's own sender
//!   ([`crate::send`]) uses it, and keeps btrbk's names and snapshot folder when a
//!   `btrbk.conf` has them, so a PC can switch between btrbk and the agent without a new
//!   full send;
//! - [`boot_mounts`]: the ESP or a separate `/boot` outside every sent subvolume (copied by
//!   [`crate::bootcopy`]).

use std::path::Path;

use rynas_api_types::pcagent::{BtrbkLayout, BtrbkSubvolume, Manifest};

use crate::mounts::{self, Mount};

/// btrbk's configuration, relative to the root.
pub const CONF: &str = "etc/btrbk/btrbk.conf";

/// `btrbk.conf` under `root`: `Ok(None)` when there is none; an error when it exists but
/// cannot be read or is not UTF-8 (never taken as "no btrbk").
pub fn read_conf(root: &Path) -> Result<Option<String>, String> {
    let p = root.join(CONF);
    match std::fs::read(&p) {
        Ok(b) => String::from_utf8(b).map(Some).map_err(|_| format!("{}: not UTF-8 text", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", p.display())),
    }
}

/// A `btrbk.conf` line without its comment: `#` at the start or after any whitespace.
fn strip_comment(line: &str) -> &str {
    let b = line.as_bytes();
    let cut = (0..b.len()).find(|&i| b[i] == b'#' && (i == 0 || b[i - 1].is_ascii_whitespace()));
    cut.map(|i| &line[..i]).unwrap_or(line).trim()
}

/// Mount points whose subvolume is cache, logs or other rebuildable data: not sent, recreated
/// empty by a restore (the manifest lists them as not captured).
const NOT_SENT: &[&str] = &["/var/log", "/var/cache", "/var/tmp", "/tmp", "/swap", "/var/swap", "/var/lib/docker", "/var/lib/containers", "/srv/swap"];

// ------------------------------------------------------------------ btrbk.conf

/// One `subvolume` section of a `btrbk.conf`, inherited options resolved.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfSubvolume {
    /// Absolute path of the subvolume on the PC.
    pub dir: String,
    /// Absolute `snapshot_dir` (empty: snapshots next to the subvolume).
    pub snapshot_dir: String,
    /// btrbk's name of its snapshots.
    pub name: String,
    pub targets: Vec<String>,
}

fn join_abs(base: &str, rel: &str) -> String {
    if rel.starts_with('/') || base.is_empty() {
        return normalize(rel);
    }
    normalize(&format!("{base}/{rel}"))
}

/// `/a/./b//c/` → `/a/b/c` (no `..` handling: btrbk paths do not use it).
fn normalize(p: &str) -> String {
    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
    format!("/{}", parts.join("/"))
}

pub(crate) fn basename(p: &str) -> &str {
    p.trim_end_matches('/').rsplit('/').next().unwrap_or(p)
}

/// The subvolume sections of a `btrbk.conf`. Options are inherited global → volume →
/// subvolume; a target of a volume applies to each of its subvolumes.
pub fn parse_conf(text: &str) -> Vec<ConfSubvolume> {
    let mut out: Vec<ConfSubvolume> = Vec::new();
    let mut global_snap = String::new();
    let mut global_targets: Vec<String> = Vec::new();
    // (volume dir, its snapshot_dir, its targets, first subvolume index)
    let mut volume: Option<(String, String, Vec<String>, usize)> = None;
    let mut current: Option<usize> = None;
    let mut in_target = false;
    let close_volume = |out: &mut Vec<ConfSubvolume>, v: &Option<(String, String, Vec<String>, usize)>| {
        if let Some((_, _, t, first)) = v {
            for s in out.iter_mut().skip(*first) {
                for x in t {
                    if !s.targets.contains(x) {
                        s.targets.push(x.clone());
                    }
                }
            }
        }
    };
    for line in text.lines() {
        let line = strip_comment(line);
        if line.is_empty() {
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        let (key, value) = (words[0], words.get(1).copied().unwrap_or(""));
        match key {
            "volume" => {
                close_volume(&mut out, &volume);
                volume = Some((normalize(value), String::new(), Vec::new(), out.len()));
                current = None;
                in_target = false;
            }
            "subvolume" => {
                let base = volume.as_ref().map(|v| v.0.clone()).unwrap_or_default();
                let snap = volume.as_ref().map(|v| v.1.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| global_snap.clone());
                let dir = join_abs(&base, value);
                let name = if dir == "/" { "ROOT".to_string() } else { basename(&dir).to_string() };
                out.push(ConfSubvolume { dir, snapshot_dir: snap, name, targets: Vec::new() });
                current = Some(out.len() - 1);
                in_target = false;
            }
            "target" => {
                // `target [send-receive|raw] <url>`: the URL is the last word
                let url = words.last().copied().unwrap_or("").to_string();
                match (current, &mut volume) {
                    (Some(i), _) => out[i].targets.push(url),
                    (None, Some(v)) => v.2.push(url),
                    (None, None) => global_targets.push(url),
                }
                in_target = true;
            }
            "snapshot_dir" if !in_target => {
                let base = volume.as_ref().map(|v| v.0.clone()).unwrap_or_default();
                let abs = join_abs(&base, value);
                match (current, &mut volume) {
                    (Some(i), _) => out[i].snapshot_dir = abs,
                    (None, Some(v)) => v.1 = abs,
                    (None, None) => global_snap = abs,
                }
            }
            "snapshot_name" if !in_target => {
                if let Some(i) = current {
                    out[i].name = value.to_string();
                }
            }
            _ => {}
        }
    }
    close_volume(&mut out, &volume);
    for s in &mut out {
        if s.targets.is_empty() {
            s.targets = global_targets.clone();
        }
    }
    out
}

/// The btrfs mount `dir` lies on (longest mount point prefix): (device, path from the top
/// level of that filesystem).
pub fn locate(dir: &str, all: &[Mount]) -> Option<(String, String)> {
    let dir = normalize(dir);
    let m = all
        .iter()
        .filter(|m| m.path == "/" || m.path == dir || dir.starts_with(&format!("{}/", m.path)))
        .max_by_key(|m| m.path.len())?;
    if m.fstype != "btrfs" {
        return None;
    }
    let rest = dir.strip_prefix(&m.path).unwrap_or("").trim_start_matches('/');
    let base = m.root.trim_matches('/');
    let path = match (base.is_empty(), rest.is_empty()) {
        (true, _) => rest.to_string(),
        (false, true) => base.to_string(),
        (false, false) => format!("{base}/{rest}"),
    };
    Some((m.dev.clone(), path))
}

/// Where the subvolume at `path` (from the top level) of filesystem `dev` is reachable: its
/// own mount, or below the mount of a subvolume it is nested in; empty when nowhere.
pub fn mount_of(path: &str, dev: &str, all: &[Mount]) -> String {
    let btr = || all.iter().filter(|m| m.fstype == "btrfs" && m.dev == dev);
    if let Some(m) = btr().filter(|m| m.root.trim_matches('/') == path).min_by_key(|m| m.path.len()) {
        return m.path.clone();
    }
    btr()
        .filter_map(|m| {
            let r = m.root.trim_matches('/');
            let rest = if r.is_empty() { Some(path) } else { path.strip_prefix(&format!("{r}/")) }?;
            Some((r.len(), join_abs(&m.path, rest)))
        })
        .max_by_key(|(l, _)| *l)
        .map(|(_, p)| p)
        .unwrap_or_default()
}

/// The manifest's btrbk block from a `btrbk.conf`: every subvolume with a target, located
/// on the mount table. `None` when there is none.
pub fn manifest_block(conf: &[ConfSubvolume], all: &[Mount]) -> Option<BtrbkLayout> {
    let mut subs = Vec::new();
    for c in conf.iter().filter(|c| !c.targets.is_empty()) {
        let Some((dev, path)) = locate(&c.dir, all) else { continue };
        if subs.iter().any(|s: &BtrbkSubvolume| s.name == c.name) {
            continue;
        }
        subs.push(BtrbkSubvolume { name: c.name.clone(), mount: mount_of(&path, &dev, all), path });
    }
    if subs.is_empty() {
        return None;
    }
    let snapshot_dir = conf.iter().find(|c| !c.targets.is_empty()).map(|c| c.snapshot_dir.clone()).unwrap_or_default();
    Some(BtrbkLayout { snapshot_dir, subvolumes: subs, boot_copies: Vec::new(), agent: false })
}

/// The file method's subvolume list told which ones btrbk sends: `captured` = sent; local
/// btrbk snapshots below the snapshot folder are not part of the layout.
pub fn mark_captured(m: &mut Manifest, b: &BtrbkLayout, all: &[Mount]) {
    let snap = locate(&b.snapshot_dir, all).map(|(_, p)| p).filter(|p| !p.is_empty());
    for f in m.filesystems.iter_mut().filter(|f| f.fstype == "btrfs") {
        if let Some(s) = &snap {
            f.subvolumes.retain(|v| !v.path.starts_with(&format!("{s}/")));
        }
        for v in &mut f.subvolumes {
            v.captured = b.subvolumes.iter().any(|s| s.path == v.path);
        }
    }
}

// ------------------------------------------------------------------ the PC's layout

/// A mounted subvolume of the root filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Path from the top level (`@home`).
    pub path: String,
    pub mount: String,
    /// Why it is not sent (`None`: sent).
    pub skip: Option<&'static str>,
}

/// The root filesystem's mounted subvolumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// `major:minor` of the root filesystem.
    pub dev: String,
    pub subvolumes: Vec<Candidate>,
}

/// The topmost mount at `path`.
fn top_mount<'a>(all: &'a [Mount], path: &str) -> Option<&'a Mount> {
    all.iter().rev().find(|m| m.path == path)
}

/// The subvolumes of the root filesystem worth sending, from the mount table. Cache, logs,
/// swap, container storage and snapshot folders are listed but not sent.
pub fn layout(all: &[Mount]) -> Result<Layout, String> {
    let root = top_mount(all, "/").ok_or("no root mount in /proc/self/mountinfo")?;
    if root.fstype != "btrfs" {
        return Err(format!("the root filesystem is {}, not btrfs: use the file method (rynas-agent pair)", root.fstype));
    }
    if root.root.trim_matches('/').is_empty() {
        return Err("/ is the top level of its btrfs filesystem, not a subvolume: btrbk restores need / in a subvolume".into());
    }
    let mut subs: Vec<Candidate> = Vec::new();
    for m in all.iter().filter(|m| m.fstype == "btrfs" && m.dev == root.dev) {
        // a later mount on the same path hides this one
        if top_mount(all, &m.path).is_some_and(|t| !std::ptr::eq(t, m)) {
            continue;
        }
        let path = m.root.trim_matches('/').to_string();
        // the top level itself, or a bind mount of a folder inside a subvolume
        if path.is_empty() || mounts::subvol(m) != path {
            continue;
        }
        if let Some(c) = subs.iter_mut().find(|c| c.path == path) {
            if m.path.len() < c.mount.len() {
                c.mount = m.path.clone();
            }
            continue;
        }
        let under = |p: &str| m.path == p || m.path.starts_with(&format!("{p}/"));
        let skip = if NOT_SENT.iter().any(|p| under(p)) {
            Some("cache, logs or rebuildable data")
        } else if basename(&m.path) == ".snapshots" || crate::btrfs::excluded_subvolume(&path) {
            Some("snapshot folder")
        } else if m.path.chars().any(|c| c.is_whitespace() || c == '#' || c == '"' || c == '\'') || path.chars().any(char::is_whitespace) {
            Some("a name btrbk.conf cannot hold")
        } else {
            None
        };
        subs.push(Candidate { path, mount: m.path.clone(), skip });
    }
    subs.sort_by(|a, b| a.mount.cmp(&b.mount));
    Ok(Layout { dev: root.dev.clone(), subvolumes: subs })
}

/// Boot mount points outside every sent subvolume (the ESP; a separate `/boot` on another
/// filesystem or on a subvolume not sent): these are copied into the root subvolume. An
/// `autofs` entry (systemd-gpt-auto, `x-systemd.automount`) stands for the filesystem it
/// mounts on first access: the real mount on the same path wins when it is there, otherwise
/// the path is still listed and the copy opens it to trigger the mount.
pub fn boot_mounts(all: &[Mount], sent: &[BtrbkSubvolume]) -> Vec<String> {
    ["/boot", "/boot/efi", "/efi"]
        .iter()
        .filter_map(|p| {
            all.iter().rev().find(|m| m.path == *p && m.fstype != "autofs").or_else(|| top_mount(all, p))
        })
        .filter(|m| {
            let inside = m.fstype == "btrfs" && sent.iter().any(|s| {
                let r = m.root.trim_matches('/');
                r == s.path || r.starts_with(&format!("{}/", s.path))
            });
            !inside
        })
        .map(|m| m.path.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARCH: &str = "22 1 0:25 /@ / rw,noatime shared:1 - btrfs /dev/nvme0n1p2 rw,compress=zstd:3,ssd,space_cache=v2,subvolid=256,subvol=/@
40 22 0:25 /@home /home rw,noatime shared:2 - btrfs /dev/nvme0n1p2 rw,compress=zstd:3,subvolid=257,subvol=/@home
41 40 0:25 /@games /home/user/games rw,noatime shared:3 - btrfs /dev/nvme0n1p2 rw,subvolid=260,subvol=/@games
42 22 0:25 /@log /var/log rw,noatime shared:4 - btrfs /dev/nvme0n1p2 rw,subvolid=258,subvol=/@log
43 22 0:25 /@pkg /var/cache/pacman/pkg rw,noatime shared:5 - btrfs /dev/nvme0n1p2 rw,subvolid=259,subvol=/@pkg
44 22 0:25 / /mnt/btr_pool rw,noatime shared:6 - btrfs /dev/nvme0n1p2 rw,subvolid=5,subvol=/
45 22 259:1 / /boot rw,relatime shared:7 - vfat /dev/nvme0n1p1 rw,fmask=0077,dmask=0077
46 22 0:25 /@snapshots /.snapshots rw,noatime shared:8 - btrfs /dev/nvme0n1p2 rw,subvolid=261,subvol=/@snapshots
47 22 0:25 /@/srv/web /srv/web rw shared:9 - btrfs /dev/nvme0n1p2 rw,subvolid=256,subvol=/@
48 22 0:40 / /data rw shared:10 - btrfs /dev/sda1 rw,subvolid=5,subvol=/
";

    const ARCH_CONF: &str = "# hand-written
transaction_log /var/log/btrbk.log
volume /mnt/btr_pool
  snapshot_dir btrbk_snapshots
  target ssh://old-nas:2222/volume1/backup/pc
  subvolume @
  subvolume @home
    snapshot_name home_old
";

    const DEBIAN: &str = "21 1 0:31 /@rootfs / rw,relatime shared:1 - btrfs /dev/vda2 rw,space_cache=v2,subvolid=256,subvol=/@rootfs
30 21 0:44 / /boot/efi rw,relatime shared:2 - autofs systemd-1 rw,fd=50,pgrp=1,timeout=120,minproto=5,maxproto=5,direct
";

    const FEDORA: &str = "60 1 0:33 /root / rw,relatime shared:1 - btrfs /dev/nvme0n1p3 rw,compress=zstd:1,subvolid=257,subvol=/root
61 60 0:33 /home /home rw,relatime shared:2 - btrfs /dev/nvme0n1p3 rw,compress=zstd:1,subvolid=256,subvol=/home
62 60 259:2 / /boot rw,relatime shared:3 - ext4 /dev/nvme0n1p2 rw
63 62 0:50 / /boot/efi rw,relatime shared:4 - autofs systemd-1 rw,fd=51
64 63 259:1 / /boot/efi rw,relatime shared:5 - vfat /dev/nvme0n1p1 rw,fmask=0077
";

    #[test]
    fn conf_sections() {
        let c = parse_conf(ARCH_CONF);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0], ConfSubvolume { dir: "/mnt/btr_pool/@".into(), snapshot_dir: "/mnt/btr_pool/btrbk_snapshots".into(), name: "@".into(), targets: vec!["ssh://old-nas:2222/volume1/backup/pc".into()] });
        assert_eq!(c[1].name, "home_old");
        // volume-less, absolute paths, target per subvolume, `send-receive` kind; comments
        // after a space, a tab, or at the start of a line
        let c = parse_conf("snapshot_dir /.snaps\t# where\nsubvolume /\n  snapshot_name @\n  target send-receive ssh://n/x\nsubvolume /home # comment\n#subvolume /srv\n  # target ssh://other/y\n");
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].dir, "/");
        assert_eq!(c[0].targets, ["ssh://n/x"]);
        assert_eq!(c[1].name, "home");
        assert!(c[1].targets.is_empty());
        assert_eq!(c[1].snapshot_dir, "/.snaps");
        // a `#` inside a word is not a comment
        assert_eq!(parse_conf("subvolume /data#1\n  target ssh://n/x\n")[0].dir, "/data#1");
        assert_eq!(strip_comment("\tsnapshot_dir /a\t#x"), "snapshot_dir /a");
    }

    #[test]
    fn conf_read_tells_missing_from_unreadable() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(read_conf(t.path()), Ok(None));
        std::fs::create_dir_all(t.path().join("etc/btrbk")).unwrap();
        std::fs::write(t.path().join(CONF), [0xffu8, 0xfe, b'\n']).unwrap();
        assert!(read_conf(t.path()).unwrap_err().contains("not UTF-8"));
        std::fs::write(t.path().join(CONF), "subvolume /\n").unwrap();
        assert_eq!(read_conf(t.path()), Ok(Some("subvolume /\n".into())));
        // a folder where the file should be: an error, not "no btrbk"
        let t2 = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t2.path().join(CONF)).unwrap();
        assert!(read_conf(t2.path()).is_err());
    }

    #[test]
    fn locating_subvolumes() {
        let all = mounts::parse(ARCH);
        assert_eq!(locate("/mnt/btr_pool/@home", &all), Some(("0:25".into(), "@home".into())));
        assert_eq!(locate("/home", &all), Some(("0:25".into(), "@home".into())));
        assert_eq!(locate("/boot", &all), None);
        assert_eq!(mount_of("@home", "0:25", &all), "/home");
        assert_eq!(mount_of("@/var/lib/machines", "0:25", &all), "/var/lib/machines");
        assert_eq!(mount_of("@other", "0:25", &all), "/mnt/btr_pool/@other");
    }

    #[test]
    fn layouts_of_common_installs() {
        let all = mounts::parse(ARCH);
        let l = layout(&all).unwrap();
        let skipped: Vec<_> = l.subvolumes.iter().filter(|c| c.skip.is_some()).map(|c| c.path.as_str()).collect();
        assert_eq!(skipped, ["@snapshots", "@pkg", "@log"]);
        // the bind mount and the other filesystem are not subvolumes of this one
        let sent: Vec<_> = l.subvolumes.iter().filter(|c| c.skip.is_none()).map(|c| (c.path.as_str(), c.mount.as_str())).collect();
        assert_eq!(sent, [("@", "/"), ("@home", "/home"), ("@games", "/home/user/games")]);
        // the legacy manifest block keeps btrbk's names
        let b = manifest_block(&parse_conf(ARCH_CONF), &all).unwrap();
        assert_eq!(b.snapshot_dir, "/mnt/btr_pool/btrbk_snapshots");
        assert!(!b.agent);
        let names: Vec<_> = b.subvolumes.iter().map(|s| (s.name.as_str(), s.path.as_str(), s.mount.as_str())).collect();
        assert_eq!(names, [("@", "@", "/"), ("home_old", "@home", "/home")]);
        let sent = vec![BtrbkSubvolume { name: "@".into(), path: "@".into(), mount: "/".into() }];
        assert_eq!(boot_mounts(&all, &sent), ["/boot"]);
        // Debian: one root subvolume; the ESP behind an automount is still copied
        let all = mounts::parse(DEBIAN);
        let l = layout(&all).unwrap();
        assert_eq!(l.subvolumes.len(), 1);
        assert_eq!(boot_mounts(&all, &[]), ["/boot/efi"]);
        // Fedora: ext4 /boot and the triggered ESP (the vfat mount wins over its autofs)
        let all = mounts::parse(FEDORA);
        assert_eq!(layout(&all).unwrap().subvolumes.len(), 2);
        assert_eq!(boot_mounts(&all, &[]), ["/boot", "/boot/efi"]);
        // the top level as / is refused, a non-btrfs root too
        let top = mounts::parse("21 1 0:31 / / rw - btrfs /dev/vda2 rw,subvolid=5,subvol=/\n");
        assert!(layout(&top).unwrap_err().contains("top level"));
        let ext = mounts::parse("21 1 8:2 / / rw - ext4 /dev/vda2 rw\n");
        assert!(layout(&ext).unwrap_err().contains("file method"));
    }
}
