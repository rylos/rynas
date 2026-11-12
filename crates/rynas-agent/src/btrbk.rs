//! The btrfs method on the PC (B2 of `docs/research/pc-backup.md` §11): btrbk pushes the
//! snapshots to rynas over SSH, the agent only describes and prepares.
//!
//! - [`parse_conf`] reads the subvolume sections of a `btrbk.conf` (inherited `snapshot_dir`,
//!   `snapshot_name`, targets); [`manifest_block`] turns them into the manifest's btrbk block
//!   (btrbk name → subvolume path from the top level → mount point);
//! - [`layout`] finds the subvolumes worth sending from the mount table (generic: no names,
//!   only mount points decide what is cache or log), [`plan`] writes the `btrbk.conf` the NAS
//!   expects and the manifest hooks (btrbk drop-in, pacman / apt / dnf / kernel-install);
//! - [`changes`] + [`apply`]: nothing is overwritten without a diff and a confirmation, the
//!   old file is kept;
//! - [`copy_boot`]: the ESP or a separate `/boot` outside every sent subvolume, copied into
//!   `/.rynas/boot/<name>/` (temporary folder + rename, one copy).
//!
//! Every path written is under an injectable root, so tests never touch the system.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use rynas_api_types::pcagent::{BootCopy, BtrbkLayout, BtrbkSubvolume, Manifest};
use sha2::{Digest, Sha256};

use crate::mounts::{self, Mount};

/// btrbk's configuration, relative to the root.
pub const CONF: &str = "etc/btrbk/btrbk.conf";
/// The PC's key for the NAS (the NAS' snippet uses the same path).
pub const KEY: &str = "etc/btrbk/ssh/id_ed25519";
/// The NAS account PCs push as (`rynas_btrfs::receive::ACCOUNT`).
pub const ACCOUNT: &str = "rynas-pcbackup";
/// Where local snapshots go when no `snapshot_dir` is set yet (inside the root subvolume).
pub const DEFAULT_SNAPSHOT_DIR: &str = "/.btrbk_snapshots";
/// The first line of every file this module writes.
const MARK: &str = "# Written by rynas-agent btrbk-setup";

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

fn basename(p: &str) -> &str {
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
        let line = line.split(" #").next().unwrap_or("").trim();
        if line.is_empty() || line.starts_with('#') {
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
    Some(BtrbkLayout { snapshot_dir, subvolumes: subs, boot_copies: Vec::new() })
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
/// filesystem or on a subvolume not sent): these are copied into the root subvolume.
pub fn boot_mounts(all: &[Mount], sent: &[BtrbkSubvolume]) -> Vec<String> {
    ["/boot", "/boot/efi", "/efi"]
        .iter()
        .filter_map(|p| top_mount(all, p))
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

// ------------------------------------------------------------------ the files written

/// What `btrbk-setup` is asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opts {
    pub device: String,
    /// `ssh://host[:port]/path` of the PC's folder on the NAS.
    pub target: String,
    /// The NAS deletes old snapshots (the PC can only add).
    pub nas_retention: bool,
    /// The agent's absolute path, for the hooks.
    pub agent: String,
}

/// The package managers found under the root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Distro {
    pub pacman: bool,
    pub apt: bool,
    /// dnf with the actions plugin (dnf5) / post-transaction-actions (dnf4) configured.
    pub dnf: bool,
    pub dnf5_actions: bool,
    pub dnf4_actions: bool,
    pub btrbk_unit: bool,
}

impl Distro {
    pub fn detect(root: &Path) -> Distro {
        let e = |p: &str| root.join(p).exists();
        Distro {
            pacman: e("etc/pacman.conf"),
            apt: e("etc/apt/apt.conf.d"),
            dnf: e("etc/dnf"),
            dnf5_actions: e("etc/dnf/libdnf5-plugins/actions.conf"),
            dnf4_actions: e("etc/dnf/plugins/post-transaction-actions.conf"),
            btrbk_unit: ["usr/lib/systemd/system/btrbk.service", "lib/systemd/system/btrbk.service", "etc/systemd/system/btrbk.service"].iter().any(|p| e(p)),
        }
    }
}

/// One file to write, relative to the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub rel: String,
    pub content: String,
    pub mode: u32,
}

/// Everything `btrbk-setup` would write and say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub files: Vec<PlannedFile>,
    pub snapshot_dir: String,
    pub sent: Vec<BtrbkSubvolume>,
    /// Things the user should know (missing plugin, no btrbk unit…).
    pub notes: Vec<String>,
}

/// A target URL the generated file can hold: `ssh://host[:port]/path`.
pub fn valid_target(t: &str) -> bool {
    let Some(rest) = t.strip_prefix("ssh://") else { return false };
    let Some(slash) = rest.find('/') else { return false };
    let (host, path) = rest.split_at(slash);
    !host.is_empty()
        && host.bytes().all(|b| b.is_ascii_alphanumeric() || b".:-[]".contains(&b))
        && path.len() > 1
        && path.bytes().all(|b| b.is_ascii_alphanumeric() || b"._/-@".contains(&b))
        && !path.split('/').any(|p| p == "..")
}

/// (host, port) of a target URL, for the first-connection command.
pub fn target_host(t: &str) -> (String, Option<u16>) {
    let rest = t.strip_prefix("ssh://").unwrap_or(t);
    let host = rest.split('/').next().unwrap_or("");
    // `[v6]:port`, `host:port`, `host`
    if let Some(h) = host.strip_prefix('[')
        && let Some((a, p)) = h.split_once(']')
    {
        return (a.to_string(), p.strip_prefix(':').and_then(|p| p.parse().ok()));
    }
    match host.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => (h.to_string(), p.parse().ok()),
        _ => (host.to_string(), None),
    }
}

/// The `btrbk.conf`, the hooks and the drop-in for this layout. Names and the snapshot
/// folder already in `existing` are kept, so a chain on the NAS goes on incrementally.
pub fn plan(o: &Opts, l: &Layout, existing: &[ConfSubvolume], all: &[Mount], d: Distro) -> Result<Plan, String> {
    if !rynas_api_types::pcagent::valid_device(&o.device) {
        return Err(format!("{}: not a device name (a-z, 0-9, -)", o.device));
    }
    if !valid_target(&o.target) {
        return Err(format!("{}: expected ssh://host[:port]/path, as the NAS shows it", o.target));
    }
    // names already used for these subvolumes, by path from the top level
    let mut old_names: BTreeMap<String, String> = BTreeMap::new();
    for c in existing {
        if let Some((dev, p)) = locate(&c.dir, all)
            && dev == l.dev
        {
            old_names.entry(p).or_insert_with(|| c.name.clone());
        }
    }
    let snapshot_dir = existing
        .iter()
        .map(|c| c.snapshot_dir.clone())
        .find(|s| !s.is_empty() && locate(s, all).is_some_and(|(dev, _)| dev == l.dev))
        .unwrap_or_else(|| DEFAULT_SNAPSHOT_DIR.to_string());
    let mut sent: Vec<BtrbkSubvolume> = Vec::new();
    for c in l.subvolumes.iter().filter(|c| c.skip.is_none()) {
        let mut name = old_names.get(&c.path).cloned().unwrap_or_else(|| basename(&c.path).to_string());
        if sent.iter().any(|s| s.name == name) {
            name = c.path.replace('/', "_");
        }
        if name.is_empty() || name.contains(['/', ' ', '#']) {
            return Err(format!("{}: no usable snapshot name", c.path));
        }
        sent.push(BtrbkSubvolume { name, path: c.path.clone(), mount: c.mount.clone() });
    }
    if !sent.iter().any(|s| s.mount == "/") {
        return Err("the root subvolume would not be sent".into());
    }
    let retention = if o.nas_retention {
        "# old snapshots on the NAS are deleted by the NAS (this PC can only add)\n\
         target_preserve_min    all\n\
         target_preserve        no\n"
    } else {
        "target_preserve_min    no\n\
         target_preserve        20d 10w 6m\n"
    };
    let mut conf = format!(
        "{MARK} for {device}: snapshots of this PC go to its\n\
         # folder on the rynas NAS. Run the command again after adding a subvolume.\n\
         ssh_identity           /{KEY}\n\
         ssh_user               {ACCOUNT}\n\
         backend_remote         btrfs-progs-sudo\n\
         stream_compress        zstd\n\
         \n\
         snapshot_preserve_min  2d\n\
         snapshot_preserve      14d\n\
         {retention}\
         \n\
         snapshot_dir           {snapshot_dir}\n",
        device = o.device
    );
    for s in &sent {
        conf.push_str(&format!("\nsubvolume {}\n  snapshot_name  {}\n  target         {}\n", s.mount, s.name, o.target));
    }
    let agent = &o.agent;
    let run = format!("{agent} manifest --quiet");
    let mut mounts_needed: Vec<&str> = sent.iter().map(|s| s.mount.as_str()).collect();
    mounts_needed.push(&snapshot_dir);
    let mut files = vec![
        PlannedFile { rel: CONF.into(), content: conf, mode: 0o644 },
        PlannedFile {
            rel: "etc/systemd/system/btrbk.service.d/rynas.conf".into(),
            content: format!(
                "{MARK}: the layout manifest is refreshed before every btrbk\n\
                 # run, so each snapshot of the root subvolume carries the layout of its moment.\n\
                 [Unit]\n\
                 RequiresMountsFor={}\n\
                 \n\
                 [Service]\n\
                 ExecStartPre=-{run}\n",
                mounts_needed.join(" ")
            ),
            mode: 0o644,
        },
    ];
    let mut notes = Vec::new();
    if d.pacman {
        files.push(PlannedFile {
            rel: "etc/pacman.d/hooks/95-rynas-manifest.hook".into(),
            content: format!(
                "{MARK}: kernels, initramfs and boot loader changes\n\
                 # refresh the layout manifest (after mkinitcpio's own hooks).\n\
                 [Trigger]\n\
                 Operation = Install\n\
                 Operation = Upgrade\n\
                 Operation = Remove\n\
                 Type = Path\n\
                 Target = usr/lib/modules/*/vmlinuz\n\
                 Target = usr/lib/initcpio/*\n\
                 Target = etc/mkinitcpio.conf\n\
                 Target = etc/mkinitcpio.d/*\n\
                 Target = usr/lib/systemd/boot/efi/*\n\
                 Target = usr/lib/grub/*\n\
                 Target = boot/*\n\
                 \n\
                 [Action]\n\
                 Description = Refreshing the rynas layout manifest...\n\
                 When = PostTransaction\n\
                 Exec = {run}\n"
            ),
            mode: 0o644,
        });
    }
    if d.apt {
        files.push(PlannedFile {
            rel: "etc/apt/apt.conf.d/95rynas-manifest".into(),
            content: format!(
                "{MARK}: refresh the layout manifest after every dpkg run\n\
                 # (unchanged boot files are not copied again).\n\
                 DPkg::Post-Invoke {{ \"if [ -x {agent} ]; then {run} || true; fi\"; }};\n"
            ),
            mode: 0o644,
        });
    }
    if d.dnf {
        if d.dnf5_actions {
            files.push(PlannedFile {
                rel: "etc/dnf/libdnf5-plugins/actions.d/rynas-manifest.actions".into(),
                content: format!(
                    "{MARK}: refresh the layout manifest after kernel and\n\
                     # boot loader updates (libdnf5 actions plugin).\n\
                     post_transaction:kernel*:in::{run}\n\
                     post_transaction:grub2-*:in::{run}\n\
                     post_transaction:shim-*:in::{run}\n\
                     post_transaction:systemd-boot*:in::{run}\n"
                ),
                mode: 0o644,
            });
        } else if d.dnf4_actions {
            files.push(PlannedFile {
                rel: "etc/dnf/plugins/post-transaction-actions.d/rynas-manifest.action".into(),
                content: format!(
                    "{MARK}: refresh the layout manifest after kernel and\n\
                     # boot loader updates (dnf post-transaction-actions plugin).\n\
                     kernel*:in:{run}\n\
                     grub2-*:in:{run}\n\
                     shim-*:in:{run}\n"
                ),
                mode: 0o644,
            });
        } else {
            notes.push("dnf: no actions plugin configured. Kernel changes refresh the manifest through kernel-install; for boot loader updates too, install libdnf5-plugin-actions (dnf5) or python3-dnf-plugin-post-transaction-actions (dnf4) and run this command again.".into());
        }
        if !d.dnf5_actions && !d.dnf4_actions {
            files.push(PlannedFile {
                rel: "etc/kernel/install.d/95-rynas-manifest.install".into(),
                content: format!(
                    "#!/bin/sh\n\
                     {MARK}: refresh the layout manifest when kernel-install\n\
                     # adds or removes a kernel (after the initramfs plugins).\n\
                     [ -x {agent} ] && {run}\n\
                     exit 0\n"
                ),
                mode: 0o755,
            });
        }
    }
    if !d.pacman && !d.apt && !d.dnf {
        notes.push("no pacman, apt or dnf found: after kernel or boot loader changes, run `rynas-agent manifest` (btrbk runs refresh it anyway).".into());
    }
    if !d.btrbk_unit {
        notes.push("btrbk's systemd unit was not found (is btrbk installed?): whatever starts btrbk should run `rynas-agent manifest` first.".into());
    }
    if !agent.starts_with("/usr/") && !agent.starts_with("/opt/") && !agent.starts_with("/bin/") {
        notes.push(format!("the hooks call {agent}: install rynas-agent from a package (or into /usr/local/bin) and run this command again from there, so the path stays valid."));
    }
    Ok(Plan { files, snapshot_dir, sent, notes })
}

// ------------------------------------------------------------------ diff and write

/// A planned file that differs from what is on disk (`old`: the current text, `None` when
/// the file does not exist).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub file: PlannedFile,
    pub old: Option<String>,
}

impl Change {
    /// It replaces a file this module did not write: needs a confirmation.
    pub fn replaces_foreign(&self) -> bool {
        self.old.as_deref().is_some_and(|o| !o.starts_with(MARK) && !o.starts_with(&format!("#!/bin/sh\n{MARK}")))
    }
}

/// The planned files that are new or different under `root`.
pub fn changes(files: &[PlannedFile], root: &Path) -> Vec<Change> {
    files
        .iter()
        .filter_map(|f| {
            let old = std::fs::read_to_string(root.join(&f.rel)).ok();
            (old.as_deref() != Some(f.content.as_str())).then(|| Change { file: f.clone(), old })
        })
        .collect()
}

/// A line diff (`-` old, `+` new, two lines of context), by longest common subsequence.
pub fn diff(old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    if a.len() * b.len() > 4_000_000 {
        return format!("(files too long to compare: {} → {} lines)\n", a.len(), b.len());
    }
    let mut l = vec![vec![0u32; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            l[i][j] = if a[i] == b[j] { l[i + 1][j + 1] + 1 } else { l[i + 1][j].max(l[i][j + 1]) };
        }
    }
    let mut ops: Vec<(char, &str)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            ops.push((' ', a[i]));
            i += 1;
            j += 1;
        } else if i < a.len() && (j == b.len() || l[i + 1][j] >= l[i][j + 1]) {
            ops.push(('-', a[i]));
            i += 1;
        } else {
            ops.push(('+', b[j]));
            j += 1;
        }
    }
    let near = |k: usize| ops[k.saturating_sub(2)..(k + 3).min(ops.len())].iter().any(|o| o.0 != ' ');
    let mut out = String::new();
    let mut gap = false;
    for (k, (c, t)) in ops.iter().enumerate() {
        if *c != ' ' || near(k) {
            out.push_str(&format!("{c} {t}\n"));
            gap = false;
        } else if !gap {
            out.push_str("  ...\n");
            gap = true;
        }
    }
    out
}

/// Writes the changes under `root` (temporary file + fsync + rename); a file that existed
/// is kept first as `<file>.rynas-<stamp>.bak`. Returns the backups made.
pub fn apply(changes: &[Change], root: &Path, stamp: &str) -> Result<Vec<PathBuf>, String> {
    let mut kept = Vec::new();
    for c in changes {
        let p = root.join(&c.file.rel);
        if let Some(parent) = p.parent() {
            std::fs::DirBuilder::new().recursive(true).mode(0o755).create(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        if let Some(old) = &c.old {
            let bak = PathBuf::from(format!("{}.rynas-{stamp}.bak", p.display()));
            crate::state::write_atomic(&bak, old.as_bytes(), 0o600)?;
            kept.push(bak);
        }
        crate::state::write_atomic(&p, c.file.content.as_bytes(), c.file.mode)?;
        // write_atomic creates with the mode masked by the umask
        let _ = std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(c.file.mode));
    }
    Ok(kept)
}

// ------------------------------------------------------------------ boot copies

/// Names of files never copied: key material some setups keep on `/boot`.
fn secret_name(n: &str) -> bool {
    let l = n.to_ascii_lowercase();
    l.contains("keyfile") || l.ends_with(".key") || l.ends_with(".keys")
}

/// One entry of a boot tree, relative to its mount.
enum Item {
    Dir(PathBuf),
    File(PathBuf),
    Link(PathBuf, PathBuf),
}

/// The tree under `src` on its own filesystem (other mounts below it are left out), its
/// signature (names, sizes, times) and the files left out as secrets.
fn scan(src: &Path) -> Result<(Vec<Item>, String, Vec<String>), String> {
    let dev = std::fs::symlink_metadata(src).map_err(|e| format!("{}: {e}", src.display()))?.dev();
    let mut items = Vec::new();
    let mut skipped = Vec::new();
    let mut h = Sha256::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let dir = src.join(&rel);
        let mut entries: Vec<_> = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let r = rel.join(e.file_name());
            let Ok(md) = std::fs::symlink_metadata(e.path()) else { continue };
            if md.dev() != dev {
                continue;
            }
            if secret_name(&e.file_name().to_string_lossy()) {
                skipped.push(r.to_string_lossy().into_owned());
                continue;
            }
            h.update(format!("{}\0{}\0{}\0{}.{}\n", r.display(), md.mode(), md.len(), md.mtime(), md.mtime_nsec()).as_bytes());
            if md.is_dir() {
                items.push(Item::Dir(r.clone()));
                stack.push(r);
            } else if md.file_type().is_symlink() {
                if let Ok(t) = std::fs::read_link(e.path()) {
                    items.push(Item::Link(r, t));
                }
            } else if md.is_file() {
                items.push(Item::File(r));
            }
        }
    }
    let sig = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok((items, sig, skipped))
}

fn sync_dir(p: &Path) {
    if let Ok(d) = std::fs::File::open(p) {
        let _ = d.sync_all();
    }
}

/// The folder name of a mount point's copy: `/boot/efi` → `boot-efi`.
pub fn copy_name(mount: &str) -> String {
    let n = mount.trim_matches('/').replace('/', "-");
    if n.is_empty() { "root".into() } else { n }
}

/// Copies the boot filesystem at `src` (mounted at `mount`) into `<boot_dir>/<name>/`:
/// into a temporary folder first, then renamed over the previous copy. When the tree is
/// unchanged since `previous` (same signature, copy present) nothing is copied.
/// `shown_dir`: `boot_dir` as the PC sees it (`/.rynas/boot`).
pub fn copy_boot(src: &Path, mount: &str, boot_dir: &Path, shown_dir: &str, previous: Option<&BootCopy>) -> Result<BootCopy, String> {
    let name = copy_name(mount);
    let dest = boot_dir.join(&name);
    let (items, signature, skipped) = scan(src)?;
    if let Some(p) = previous
        && p.signature == signature
        && p.mount == mount
        && dest.is_dir()
    {
        return Ok(p.clone());
    }
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(boot_dir).map_err(|e| format!("{}: {e}", boot_dir.display()))?;
    let tmp = boot_dir.join(format!(".{name}.tmp"));
    let old = boot_dir.join(format!(".{name}.old"));
    for p in [&tmp, &old] {
        if p.exists() {
            std::fs::remove_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
        }
    }
    let mk = |p: &Path| std::fs::DirBuilder::new().mode(0o700).create(p).map_err(|e| format!("{}: {e}", p.display()));
    mk(&tmp)?;
    let (mut bytes, mut files) = (0u64, 0u64);
    for it in &items {
        match it {
            Item::Dir(r) => mk(&tmp.join(r))?,
            Item::Link(r, t) => std::os::unix::fs::symlink(t, tmp.join(r)).map_err(|e| format!("{}: {e}", r.display()))?,
            Item::File(r) => {
                let from = src.join(r);
                let to = tmp.join(r);
                let mut i = std::fs::File::open(&from).map_err(|e| format!("{}: {e}", from.display()))?;
                let mut o = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&to)
                    .map_err(|e| format!("{}: {e}", to.display()))?;
                bytes += std::io::copy(&mut i, &mut o).map_err(|e| format!("{}: {e}", from.display()))?;
                o.flush().and_then(|_| o.sync_all()).map_err(|e| format!("{}: {e}", to.display()))?;
                files += 1;
            }
        }
    }
    sync_dir(&tmp);
    if dest.exists() {
        std::fs::rename(&dest, &old).map_err(|e| format!("{}: {e}", dest.display()))?;
    }
    std::fs::rename(&tmp, &dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    sync_dir(boot_dir);
    let _ = std::fs::remove_dir_all(&old);
    Ok(BootCopy { mount: mount.to_string(), dir: format!("{}/{name}", shown_dir.trim_end_matches('/')), bytes, files, signature, skipped })
}

/// Copies that are no longer wanted (mount gone) are removed; the others stay.
pub fn prune_copies(boot_dir: &Path, keep: &[BootCopy]) {
    let Ok(rd) = std::fs::read_dir(boot_dir) else { return };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if !keep.iter().any(|k| copy_name(&k.mount) == n) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
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
30 21 254:1 / /boot/efi rw,relatime shared:2 - vfat /dev/vda1 rw,fmask=0077
";

    const FEDORA: &str = "60 1 0:33 /root / rw,relatime shared:1 - btrfs /dev/nvme0n1p3 rw,compress=zstd:1,subvolid=257,subvol=/root
61 60 0:33 /home /home rw,relatime shared:2 - btrfs /dev/nvme0n1p3 rw,compress=zstd:1,subvolid=256,subvol=/home
62 60 259:2 / /boot rw,relatime shared:3 - ext4 /dev/nvme0n1p2 rw
63 62 259:1 / /boot/efi rw,relatime shared:4 - vfat /dev/nvme0n1p1 rw,fmask=0077
";

    fn opts(nas: bool) -> Opts {
        Opts { device: "pc-1".into(), target: "ssh://nas.example.lan:2222/volume1/backup/pc-1".into(), nas_retention: nas, agent: "/usr/bin/rynas-agent".into() }
    }

    fn names(p: &Plan) -> Vec<(&str, &str, &str)> {
        p.sent.iter().map(|s| (s.name.as_str(), s.path.as_str(), s.mount.as_str())).collect()
    }

    #[test]
    fn conf_sections() {
        let c = parse_conf(ARCH_CONF);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0], ConfSubvolume { dir: "/mnt/btr_pool/@".into(), snapshot_dir: "/mnt/btr_pool/btrbk_snapshots".into(), name: "@".into(), targets: vec!["ssh://old-nas:2222/volume1/backup/pc".into()] });
        assert_eq!(c[1].name, "home_old");
        // volume-less, absolute paths, target per subvolume, `send-receive` kind
        let c = parse_conf("snapshot_dir /.snaps\nsubvolume /\n  snapshot_name @\n  target send-receive ssh://n/x\nsubvolume /home # comment\n");
        assert_eq!(c[0].dir, "/");
        assert_eq!(c[0].targets, ["ssh://n/x"]);
        assert_eq!(c[1].name, "home");
        assert!(c[1].targets.is_empty());
        assert_eq!(c[1].snapshot_dir, "/.snaps");
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
    fn arch_with_nested_games_and_existing_conf() {
        let all = mounts::parse(ARCH);
        let l = layout(&all).unwrap();
        let skipped: Vec<_> = l.subvolumes.iter().filter(|c| c.skip.is_some()).map(|c| c.path.as_str()).collect();
        assert_eq!(skipped, ["@snapshots", "@pkg", "@log"]);
        let p = plan(&opts(true), &l, &parse_conf(ARCH_CONF), &all, Distro { pacman: true, btrbk_unit: true, ..Default::default() }).unwrap();
        // the chain's names and snapshot folder are kept; @games is new; the bind mount and
        // the other filesystem are not subvolumes of this one
        assert_eq!(names(&p), [("@", "@", "/"), ("home_old", "@home", "/home"), ("@games", "@games", "/home/user/games")]);
        assert_eq!(p.snapshot_dir, "/mnt/btr_pool/btrbk_snapshots");
        let conf = &p.files[0].content;
        assert!(conf.contains("snapshot_dir           /mnt/btr_pool/btrbk_snapshots\n"));
        assert!(conf.contains("subvolume /home/user/games\n  snapshot_name  @games\n  target         ssh://nas.example.lan:2222/volume1/backup/pc-1\n"));
        assert!(conf.contains("ssh_user               rynas-pcbackup\n") && conf.contains("backend_remote         btrfs-progs-sudo\n"));
        assert!(conf.contains("ssh_identity           /etc/btrbk/ssh/id_ed25519\n"));
        assert!(conf.contains("target_preserve_min    all\ntarget_preserve        no\n"));
        // what the generated file says is what the manifest reads back
        let back = manifest_block(&parse_conf(conf), &all).unwrap();
        assert_eq!(back.subvolumes, p.sent);
        assert_eq!(back.snapshot_dir, p.snapshot_dir);
        let rels: Vec<_> = p.files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, [CONF, "etc/systemd/system/btrbk.service.d/rynas.conf", "etc/pacman.d/hooks/95-rynas-manifest.hook"]);
        assert!(p.files[1].content.contains("ExecStartPre=-/usr/bin/rynas-agent manifest --quiet\n"));
        assert!(p.files[1].content.contains("RequiresMountsFor=/ /home /home/user/games /mnt/btr_pool/btrbk_snapshots\n"));
        assert!(p.files[2].content.contains("When = PostTransaction\nExec = /usr/bin/rynas-agent manifest --quiet\n"));
        assert!(p.notes.is_empty());
        // the ESP at /boot is outside every sent subvolume
        assert_eq!(boot_mounts(&all, &p.sent), ["/boot"]);
    }

    #[test]
    fn debian_single_root() {
        let all = mounts::parse(DEBIAN);
        let p = plan(&opts(false), &layout(&all).unwrap(), &[], &all, Distro { apt: true, ..Default::default() }).unwrap();
        assert_eq!(names(&p), [("@rootfs", "@rootfs", "/")]);
        assert_eq!(p.snapshot_dir, DEFAULT_SNAPSHOT_DIR);
        assert!(p.files[0].content.contains("target_preserve        20d 10w 6m\n"));
        let apt = &p.files[2];
        assert_eq!(apt.rel, "etc/apt/apt.conf.d/95rynas-manifest");
        assert!(apt.content.contains("DPkg::Post-Invoke { \"if [ -x /usr/bin/rynas-agent ]; then /usr/bin/rynas-agent manifest --quiet || true; fi\"; };\n"));
        // no btrbk unit found
        assert_eq!(p.notes.len(), 1);
        assert_eq!(boot_mounts(&all, &p.sent), ["/boot/efi"]);
        // the top level as / is refused, a non-btrfs root too
        let top = mounts::parse("21 1 0:31 / / rw - btrfs /dev/vda2 rw,subvolid=5,subvol=/\n");
        assert!(layout(&top).unwrap_err().contains("top level"));
        let ext = mounts::parse("21 1 8:2 / / rw - ext4 /dev/vda2 rw\n");
        assert!(layout(&ext).unwrap_err().contains("file method"));
    }

    #[test]
    fn fedora_with_ext4_boot() {
        let all = mounts::parse(FEDORA);
        let d = Distro { dnf: true, btrbk_unit: true, ..Default::default() };
        let p = plan(&opts(true), &layout(&all).unwrap(), &[], &all, d).unwrap();
        assert_eq!(names(&p), [("root", "root", "/"), ("home", "home", "/home")]);
        assert_eq!(boot_mounts(&all, &p.sent), ["/boot", "/boot/efi"]);
        // no actions plugin: kernel-install fallback and a note
        let ki = p.files.iter().find(|f| f.rel == "etc/kernel/install.d/95-rynas-manifest.install").unwrap();
        assert_eq!(ki.mode, 0o755);
        assert!(ki.content.starts_with("#!/bin/sh\n"));
        assert!(p.notes[0].contains("libdnf5-plugin-actions"));
        let p5 = plan(&opts(true), &layout(&all).unwrap(), &[], &all, Distro { dnf5_actions: true, ..d }).unwrap();
        let a = p5.files.iter().find(|f| f.rel.ends_with("rynas-manifest.actions")).unwrap();
        assert!(a.content.contains("post_transaction:kernel*:in::/usr/bin/rynas-agent manifest --quiet\n"));
        assert!(!p5.files.iter().any(|f| f.rel.contains("kernel/install.d")));
        let p4 = plan(&opts(true), &layout(&all).unwrap(), &[], &all, Distro { dnf4_actions: true, ..d }).unwrap();
        assert!(p4.files.iter().any(|f| f.content.contains("kernel*:in:/usr/bin/rynas-agent manifest --quiet\n")));
    }

    #[test]
    fn existing_snapshot_dir_and_bad_input() {
        let all = mounts::parse(FEDORA);
        let l = layout(&all).unwrap();
        let old = parse_conf("snapshot_dir /.snapshots-btrbk\nsubvolume /\n  target ssh://x/y\n");
        let p = plan(&opts(true), &l, &old, &all, Distro::default()).unwrap();
        assert_eq!(p.snapshot_dir, "/.snapshots-btrbk");
        // a name btrbk had chosen for / is kept
        assert_eq!(p.sent[0].name, "ROOT");
        let mut o = opts(true);
        o.target = "ssh://nas/x; rm -rf /".into();
        assert!(plan(&o, &l, &[], &all, Distro::default()).is_err());
        o.target = "https://nas/x".into();
        assert!(plan(&o, &l, &[], &all, Distro::default()).is_err());
        o = opts(true);
        o.device = "Bad Name".into();
        assert!(plan(&o, &l, &[], &all, Distro::default()).is_err());
        assert_eq!(target_host("ssh://nas.example.lan:2222/v/b"), ("nas.example.lan".into(), Some(2222)));
        assert_eq!(target_host("ssh://[fd00::1]:22/v"), ("fd00::1".into(), Some(22)));
        assert_eq!(target_host("ssh://nas/v"), ("nas".into(), None));
        let mut o = opts(true);
        o.agent = "/home/u/Downloads/rynas-agent".into();
        assert!(plan(&o, &l, &[], &all, Distro::default()).unwrap().notes.iter().any(|n| n.contains("Downloads")));
    }

    #[test]
    fn diff_and_no_silent_overwrite() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        let all = mounts::parse(DEBIAN);
        let p = plan(&opts(true), &layout(&all).unwrap(), &[], &all, Distro { apt: true, ..Default::default() }).unwrap();
        std::fs::create_dir_all(root.join("etc/btrbk")).unwrap();
        std::fs::write(root.join(CONF), "volume /mnt/x\n  subvolume @\n").unwrap();
        let ch = changes(&p.files, root);
        assert_eq!(ch.len(), 3);
        assert!(ch[0].replaces_foreign());
        assert!(!ch[1].replaces_foreign() && ch[1].old.is_none());
        let d = diff(ch[0].old.as_deref().unwrap(), &ch[0].file.content);
        assert!(d.contains("- volume /mnt/x\n") && d.contains("+ subvolume /\n"));
        let kept = apply(&ch, root, "1").unwrap();
        assert_eq!(kept, [root.join("etc/btrbk/btrbk.conf.rynas-1.bak")]);
        assert_eq!(std::fs::read_to_string(&kept[0]).unwrap(), "volume /mnt/x\n  subvolume @\n");
        assert_eq!(std::fs::read_to_string(root.join(CONF)).unwrap(), p.files[0].content);
        assert!(!root.join("etc/btrbk/btrbk.tmp").exists());
        // a second run finds nothing to do; our own file changed is not "foreign"
        assert!(changes(&p.files, root).is_empty());
        let p2 = plan(&opts(false), &layout(&all).unwrap(), &[], &all, Distro { apt: true, ..Default::default() }).unwrap();
        let ch = changes(&p2.files, root);
        assert_eq!(ch.len(), 1);
        assert!(!ch[0].replaces_foreign());
        assert_eq!(diff("a\nb\nc\nd\ne\nf\ng\nh\n", "a\nb\nc\nd\nX\nf\ng\nh\n"), "  ...\n  c\n  d\n- e\n+ X\n  f\n  g\n  ...\n");
        assert_eq!(diff("same\n", "same\n").trim(), "...");
    }

    #[test]
    fn boot_copy_atomic_and_skips_keys() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("esp");
        std::fs::create_dir_all(src.join("EFI/systemd")).unwrap();
        std::fs::write(src.join("EFI/systemd/systemd-bootx64.efi"), vec![1u8; 1000]).unwrap();
        std::fs::write(src.join("vmlinuz-linux"), vec![2u8; 500]).unwrap();
        std::fs::write(src.join("crypto_keyfile.bin"), b"secret").unwrap();
        let boot = t.path().join("root/.rynas/boot");
        let c = copy_boot(&src, "/boot", &boot, "/.rynas/boot", None).unwrap();
        assert_eq!((c.bytes, c.files), (1500, 2));
        assert_eq!(c.dir, "/.rynas/boot/boot");
        assert_eq!(c.skipped, ["crypto_keyfile.bin"]);
        assert!(!boot.join("boot/crypto_keyfile.bin").exists());
        assert_eq!(std::fs::read(boot.join("boot/vmlinuz-linux")).unwrap(), vec![2u8; 500]);
        // unchanged: the same record, nothing copied (a marker survives)
        std::fs::write(boot.join("boot/marker"), b"").unwrap();
        let again = copy_boot(&src, "/boot", &boot, "/.rynas/boot", Some(&c)).unwrap();
        assert_eq!(again, c);
        assert!(boot.join("boot/marker").exists());
        // changed: one new copy replaces the old whole, no temporary folders left
        std::fs::write(src.join("vmlinuz-linux"), vec![3u8; 700]).unwrap();
        let c2 = copy_boot(&src, "/boot", &boot, "/.rynas/boot", Some(&c)).unwrap();
        assert_ne!(c2.signature, c.signature);
        assert_eq!(c2.bytes, 1700);
        assert!(!boot.join("boot/marker").exists());
        let names: Vec<_> = std::fs::read_dir(&boot).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
        assert_eq!(names, ["boot"]);
        prune_copies(&boot, &[]);
        assert!(!boot.join("boot").exists());
        assert_eq!(copy_name("/boot/efi"), "boot-efi");
    }
}
