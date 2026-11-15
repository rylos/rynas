//! The storage configuration domains of `rynas-config`, so that what the write side creates
//! is versioned, drift-checked and can be rolled back like every other setting:
//!
//! - `storage.volumes` ([`VolumesDomain`]): the volumes rynas created, written as blocks of
//!   `/etc/fstab` (UUID, `nofail`, `noauto` when encrypted), `/etc/crypttab` (`noauto`: the
//!   volume is unlocked from the interface) and `/etc/mdadm/mdadm.conf`. Only operations of
//!   rynas change it ([`rynas_config::Config::apply_value`]).
//! - `storage.disks` ([`TuningDomain`]): per-disk-type defaults (I/O scheduler, read-ahead)
//!   and per-disk overrides (also APM and spin-down through `hdparm` from udev), md
//!   `stripe_cache_size` chosen per array, rendered to one udev rules file. Users stage it
//!   like any other key (`PUT /storage/disks/{name}/settings`).

use std::path::Path;

use rynas_api_types::storage::{
    DiskKind, DiskSettings, IoScheduler, StorageTuning, StorageVolumes, VolumeRecord,
};
use rynas_config::{Applier, ApplyCtx, Candidate, ManagedFile};
use rynas_tasks::Cmd;
use serde_json::Value;

use crate::exec::mapper_name;
use crate::plan::valid_label;

pub const VOLUMES_KEY: &str = "storage.volumes";
pub const TUNING_KEY: &str = "storage.disks";
pub const FSTAB: &str = "/etc/fstab";
pub const CRYPTTAB: &str = "/etc/crypttab";
pub const MDADM_CONF: &str = "/etc/mdadm/mdadm.conf";
/// After 60-persistent-storage (serials) and 63-md-raid-arrays (`MD_UUID`), and after the
/// distribution's own 60-* scheduler rules, so ours win.
pub const UDEV_RULES: &str = "/etc/udev/rules.d/69-rynas-storage.rules";

fn is_uuid(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() || b == b'-' || b == b':')
}

const UPDATE_INITRAMFS: &str = "/usr/sbin/update-initramfs";

/// The md arrays of a `storage.volumes` value (empty when absent or invalid).
fn md_uuids(v: Option<&Value>) -> std::collections::BTreeSet<String> {
    parse_volumes(v)
        .map(|c| {
            c.volumes
                .iter()
                .flat_map(|r| r.md_uuids())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn systemd_running() -> bool {
    Path::new("/run/systemd/system").exists()
}

// ------------------------------------------------------------------------------- initramfs

const LSINITRAMFS: &str = "/usr/bin/lsinitramfs";
/// mkinitramfs with `COMPRESS=xz` (`-9e`) runs for minutes on a slow NAS.
const INITRAMFS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);
const LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// Unit prefix of the transient units running update-initramfs.
pub const INITRAMFS_UNIT: &str = "rynas-initramfs";
/// Copy of the initramfs taken before it is regenerated (below the root).
const INITRAMFS_BACKUP_DIR: &str = "/var/lib/rynas/initramfs";

/// `update-initramfs` with `args`, outside rynasd's sandbox when systemd runs the machine.
///
/// It must never run inside rynasd's unit: `ProtectKernelModules=yes` hides `/lib/modules`
/// and `/usr/lib/modules` there, and mkinitramfs only warns about the missing directory and
/// writes an initramfs WITHOUT kernel modules (no ahci, nvme, md, btrfs, ext4) over the one
/// of the newest kernel (`backup_initramfs=no`): the machine no longer boots. Besides,
/// `xz -9e` needs ~700 MiB (rynasd's `daemon/` cgroup stops at 256M), `ldconfig -r` uses
/// `chroot` (outside `@system-service`) and it can take longer than the default timeout. A
/// transient unit ([`rynas_tasks::Outside`], `rynas-initramfs-*`) runs it in the host's
/// namespace without those limits; `keep_running`: a half-written initramfs is worse than a
/// late one. Without systemd (containers, tests) it runs directly.
fn update_initramfs_cmd(args: &[&str], outside: bool) -> Cmd {
    if outside {
        rynas_tasks::Outside::new(INITRAMFS_UNIT, "rynas: update-initramfs (storage)", INITRAMFS_TIMEOUT)
            .keep_running()
            .properties(&["Nice=5"])
            .cmd(std::iter::once(UPDATE_INITRAMFS).chain(args.iter().copied()))
    } else {
        Cmd::new(UPDATE_INITRAMFS)
            .args(args)
            .env_clear()
            .timeout(INITRAMFS_TIMEOUT)
    }
}

/// Compares kernel releases the way a version sort does: digit runs as numbers, the rest
/// byte by byte, `~` before anything (`6.12.9` < `6.12.10`, `7.0~rc1` < `7.0`).
fn cmp_release(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    let rank = |c: Option<&u8>| match c {
        Some(b'~') => -1i32,
        None => 0,
        Some(c) if c.is_ascii_alphabetic() => i32::from(*c),
        Some(c) => i32::from(*c) + 256,
    };
    loop {
        // non-digit part
        while a.first().is_some_and(|c| !c.is_ascii_digit()) || b.first().is_some_and(|c| !c.is_ascii_digit()) {
            let (ca, cb) = (a.first().filter(|c| !c.is_ascii_digit()), b.first().filter(|c| !c.is_ascii_digit()));
            match rank(ca).cmp(&rank(cb)) {
                Ordering::Equal => {}
                o => return o,
            }
            if ca.is_some() {
                a = &a[1..];
            }
            if cb.is_some() {
                b = &b[1..];
            }
        }
        if a.is_empty() && b.is_empty() {
            return Ordering::Equal;
        }
        // digit part
        let da = a.iter().take_while(|c| c.is_ascii_digit()).count();
        let db = b.iter().take_while(|c| c.is_ascii_digit()).count();
        let trim = |s: &[u8]| -> Vec<u8> { s.iter().skip_while(|c| **c == b'0').copied().collect() };
        let (na, nb) = (trim(&a[..da]), trim(&b[..db]));
        match na.len().cmp(&nb.len()).then_with(|| na.cmp(&nb)) {
            Ordering::Equal => {}
            o => return o,
        }
        a = &a[da..];
        b = &b[db..];
    }
}

/// The newest kernel release with an initramfs in `boot` (`initrd.img-<release>`, a regular
/// file): the one `update-initramfs -u` regenerates.
fn newest_initrd(boot: &Path) -> Option<String> {
    std::fs::read_dir(boot)
        .ok()?
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let release = name.strip_prefix("initrd.img-")?;
            (!release.is_empty() && !release.starts_with('.') && !release.contains('/'))
                .then(|| release.to_string())
        })
        .max_by(|a, b| cmp_release(a, b))
}

/// Kernel modules (`*.ko`, compressed or not) in the `lsinitramfs` listing `out`.
fn count_modules(out: &str) -> usize {
    out.lines()
        .map(str::trim)
        .filter(|l| {
            let l = l.strip_suffix(".xz").or_else(|| l.strip_suffix(".zst")).or_else(|| l.strip_suffix(".gz")).unwrap_or(l);
            l.ends_with(".ko")
        })
        .count()
}

/// The kernel modules in the initramfs at `path` (absolute, below the root), `None` when it
/// cannot be listed. lsinitramfs only reads the file and needs no `/lib/modules`.
fn initrd_modules(ctx: &ApplyCtx, path: &Path) -> Option<usize> {
    if ctx.root == Path::new("/") && !Path::new(LSINITRAMFS).exists() {
        return None;
    }
    let cmd = Cmd::new(LSINITRAMFS).arg(path).env_clear().timeout(LIST_TIMEOUT);
    ctx.system.run(&cmd).ok().map(|o| count_modules(&o.stdout))
}

/// What the module count of an initramfs before and after its regeneration says.
#[derive(Debug, PartialEq, Eq)]
enum InitrdCheck {
    /// It still carries kernel modules (or never had any: everything built in).
    Fine,
    /// Nothing to compare with: lsinitramfs is missing or the old file was unreadable.
    Unverified,
    /// It lost its modules or became unreadable: put the copy back.
    Broken(String),
}

fn check_initrd(before: Option<usize>, after: Option<usize>) -> InitrdCheck {
    match (before, after) {
        (_, Some(n)) if n > 0 => InitrdCheck::Fine,
        (Some(0), Some(0)) => InitrdCheck::Fine,
        (Some(n), Some(_)) => InitrdCheck::Broken(format!("it has no kernel modules left (it had {n})")),
        (Some(_), None) => InitrdCheck::Broken("it can no longer be read".into()),
        (None, _) => InitrdCheck::Unverified,
    }
}

/// Puts `backup` back as `target` (a copy next to it, then a rename: the initramfs is
/// never half-written).
fn restore_initrd(backup: &Path, target: &Path) -> std::io::Result<()> {
    let name = target.file_name().and_then(|n| n.to_str()).unwrap_or("initrd.img");
    let tmp = target.with_file_name(format!(".{name}.rynas-restore"));
    let r = (|| {
        std::fs::copy(backup, &tmp)?;
        std::fs::File::open(&tmp)?.sync_all()?;
        std::fs::rename(&tmp, target)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
        // no room for a second copy in /boot: overwrite in place
        std::fs::copy(backup, target)?;
        std::fs::File::open(target)?.sync_all()?;
    }
    Ok(())
}

/// Regenerates the initramfs of the newest kernel (it carries a copy of mdadm.conf) and
/// checks that the new one still has kernel modules; if it lost them, the copy taken before
/// is put back. Never fails the change: a failure or a rejected initramfs is logged and left
/// in [`INITRAMFS_ALERT`] for rynasd to tell the administrators.
fn refresh_initramfs(ctx: &ApplyCtx) {
    let outside = ctx.root == Path::new("/") && systemd_running();
    let boot = ctx.path(Path::new("/boot"));
    let Some(release) = newest_initrd(&boot) else {
        // no initramfs to protect (update-initramfs -u then has nothing to do either)
        if let Err(e) = ctx.system.run(&update_initramfs_cmd(&["-u"], outside)) {
            (ctx.log)(&format!(
                "storage.volumes: warning: update-initramfs failed ({e}); run `update-initramfs -u` by hand"
            ));
            raise_initramfs_alert(ctx, InitramfsAlertKind::Failed, "", &e);
        }
        return;
    };
    let target = boot.join(format!("initrd.img-{release}"));
    let backup_dir = ctx.path(Path::new(INITRAMFS_BACKUP_DIR));
    let backup = backup_dir.join(format!("initrd.img-{release}"));
    let copied = std::fs::create_dir_all(&backup_dir)
        .and_then(|_| std::fs::copy(&target, &backup))
        .and_then(|_| std::fs::File::open(&backup)?.sync_all());
    if let Err(e) = copied {
        let _ = std::fs::remove_file(&backup);
        (ctx.log)(&format!(
            "storage.volumes: warning: cannot copy {} to {} ({e}): the initramfs was not refreshed; run `update-initramfs -u -k {release}` by hand",
            target.display(),
            backup.display()
        ));
        raise_initramfs_alert(ctx, InitramfsAlertKind::Failed, &release, &format!("no room for a copy of the current one: {e}"));
        return;
    }
    let before = initrd_modules(ctx, &target);
    let run = ctx.system.run(&update_initramfs_cmd(&["-u", "-k", &release], outside));
    let mut failed = None;
    if let Err(e) = run {
        if e.contains("timed out") {
            // the unit keeps running (keep_running): checking or restoring now would race it
            let _ = std::fs::remove_file(&backup);
            (ctx.log)(&format!(
                "storage.volumes: warning: update-initramfs is still running after {} min ({e}); check /boot/initrd.img-{release} with lsinitramfs when it ends",
                INITRAMFS_TIMEOUT.as_secs() / 60
            ));
            return;
        }
        (ctx.log)(&format!(
            "storage.volumes: warning: update-initramfs failed ({e}); run `update-initramfs -u -k {release}` by hand"
        ));
        failed = Some(e);
    }
    let after = initrd_modules(ctx, &target);
    match check_initrd(before, after) {
        InitrdCheck::Fine | InitrdCheck::Unverified if failed.is_some() => {
            let _ = std::fs::remove_file(&backup);
            raise_initramfs_alert(ctx, InitramfsAlertKind::Failed, &release, failed.as_deref().unwrap_or(""));
        }
        InitrdCheck::Fine => {
            let _ = std::fs::remove_file(&backup);
        }
        InitrdCheck::Unverified => {
            let _ = std::fs::remove_file(&backup);
            (ctx.log)(&format!(
                "storage.volumes: warning: the new initramfs of {release} could not be checked (lsinitramfs missing or failing)"
            ));
        }
        InitrdCheck::Broken(why) => match restore_initrd(&backup, &target) {
            Ok(()) => {
                let _ = std::fs::remove_file(&backup);
                (ctx.log)(&format!(
                    "storage.volumes: ERROR: the regenerated initramfs of {release} was unusable ({why}): the previous one was put back; the initramfs copy of mdadm.conf is outdated, run `update-initramfs -u -k {release}` from a root shell and check it with lsinitramfs"
                ));
                raise_initramfs_alert(ctx, InitramfsAlertKind::Rejected, &release, &why);
            }
            Err(e) => {
                let msg = format!(
                    "{why}; putting the previous one back failed ({e}): copy {} to {} before restarting",
                    backup.display(),
                    target.display()
                );
                (ctx.log)(&format!(
                    "storage.volumes: ERROR: the regenerated initramfs of {release} is unusable: {msg}, or the machine may not boot"
                ));
                raise_initramfs_alert(ctx, InitramfsAlertKind::NotRestored, &release, &msg);
            }
        },
    }
}

/// File (below the root) telling rynasd that the last initramfs refresh went wrong: the
/// change that asked for it is kept (the volumes work; the initramfs copy of mdadm.conf
/// only matters for arrays assembled before the root), the administrators are told.
pub const INITRAMFS_ALERT: &str = "/var/lib/rynas/initramfs/alert.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InitramfsAlertKind {
    /// update-initramfs failed (or could not start): the previous initramfs stays.
    Failed,
    /// The new initramfs had lost its kernel modules: the previous one was put back.
    Rejected,
    /// The new initramfs is unusable and the previous one could not be put back.
    NotRestored,
}

/// What [`INITRAMFS_ALERT`] holds.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InitramfsAlert {
    pub kind: InitramfsAlertKind,
    /// Kernel release of the initramfs (empty when none was found).
    pub release: String,
    pub detail: String,
}

fn raise_initramfs_alert(ctx: &ApplyCtx, kind: InitramfsAlertKind, release: &str, detail: &str) {
    let a = InitramfsAlert { kind, release: release.to_string(), detail: detail.to_string() };
    let path = ctx.path(Path::new(INITRAMFS_ALERT));
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|_| std::fs::write(&path, serde_json::to_vec(&a).unwrap_or_default()));
    if let Err(e) = written {
        (ctx.log)(&format!("storage.volumes: cannot write {} ({e})", path.display()));
    }
}

/// Takes the alert of the last initramfs refresh below `root`, if any (read once).
pub fn take_initramfs_alert(root: &Path) -> Option<InitramfsAlert> {
    let path = root.join(INITRAMFS_ALERT.trim_start_matches('/'));
    let text = std::fs::read(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    serde_json::from_slice(&text).ok()
}

// ------------------------------------------------------------------------------- volumes

pub fn parse_volumes(v: Option<&Value>) -> Result<StorageVolumes, String> {
    match v {
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("storage.volumes: {e}")),
        None => Ok(StorageVolumes::default()),
    }
}

/// `/etc/fstab` line of a volume.
pub fn fstab_line(r: &VolumeRecord) -> String {
    format!(
        "UUID={} {} btrfs {},nofail{} 0 0",
        r.fs_uuid,
        r.mountpoint,
        r.options,
        if r.luks_uuid.is_some() { ",noauto" } else { "" }
    )
}

/// The md arrays of a record come in one of two forms: one array (`md_uuid`), or for a
/// flexible RAID one array per tier (`tiers`, numbered 1, 2, ... without gaps) and the
/// volume group that joins them.
fn check_arrays(r: &VolumeRecord) -> Result<(), rynas_config::Invalid> {
    if !r.level.is_flexible() {
        if !r.tiers.is_empty() || r.vg.is_some() {
            return Err(format!("volume {}: tiers belong to a flexible RAID", r.label).into());
        }
        if !is_uuid(&r.md_uuid) {
            return Err(format!("volume {}: invalid UUID", r.label).into());
        }
        return Ok(());
    }
    if !r.md_uuid.is_empty() {
        return Err(format!("volume {}: a flexible RAID lists its arrays in tiers", r.label).into());
    }
    if r.tiers.is_empty() {
        return Err(format!("volume {}: a flexible RAID needs at least one tier", r.label).into());
    }
    for (i, t) in r.tiers.iter().enumerate() {
        if t.index as usize != i + 1 {
            return Err(format!("volume {}: tiers must be numbered 1, 2, ... in order", r.label).into());
        }
        if !is_uuid(&t.md_uuid) {
            return Err(format!("volume {}: invalid UUID of tier {}", r.label, t.index).into());
        }
    }
    let vg_ok = r.vg.as_deref().is_some_and(|v| {
        !v.is_empty()
            && v.len() <= 64
            && !v.starts_with('-')
            && v.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-'))
    });
    if !vg_ok {
        return Err(format!("volume {}: invalid volume group name", r.label).into());
    }
    Ok(())
}

/// The `/etc/mdadm/mdadm.conf` lines of a volume: one array, or one per tier (with
/// `AUTO -all` only listed arrays are assembled, so every tier must be there).
fn mdadm_lines(r: &VolumeRecord) -> String {
    if r.tiers.is_empty() {
        return format!("ARRAY /dev/md/{} metadata=1.2 UUID={}\n", r.label, r.md_uuid);
    }
    r.tiers
        .iter()
        .map(|t| {
            format!(
                "ARRAY /dev/md/{}_t{} metadata=1.2 UUID={}\n",
                r.label, t.index, t.md_uuid
            )
        })
        .collect()
}

pub struct VolumesDomain;

impl Applier for VolumesDomain {
    fn key(&self) -> &str {
        VOLUMES_KEY
    }

    /// Before users (10) and shares (20): shares live on volumes.
    fn order(&self) -> i32 {
        5
    }

    fn validate(&self, value: Value) -> Result<Value, rynas_config::Invalid> {
        let mut c = parse_volumes(Some(&value))?;
        c.volumes.sort_by(|a, b| a.label.cmp(&b.label));
        for w in c.volumes.windows(2) {
            if w[0].label == w[1].label {
                return Err(format!("volume {} is listed twice", w[0].label).into());
            }
        }
        for r in &c.volumes {
            if !valid_label(&r.label) {
                return Err(format!("invalid volume label {:?}", r.label).into());
            }
            if r.mountpoint != format!("/{}", r.label) {
                return Err(
                    format!("volume {}: the mount point must be /{}", r.label, r.label).into(),
                );
            }
            if !is_uuid(&r.fs_uuid) {
                return Err(format!("volume {}: invalid UUID", r.label).into());
            }
            check_arrays(r)?;
            if r.luks_uuid.as_deref().is_some_and(|u| !is_uuid(u)) {
                return Err(format!("volume {}: invalid LUKS UUID", r.label).into());
            }
            if r.options.is_empty()
                || !r
                    .options
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b',' | b'=' | b':' | b'_'))
            {
                return Err(
                    format!("volume {}: invalid mount options {:?}", r.label, r.options).into(),
                );
            }
        }
        serde_json::to_value(c).map_err(|e| e.to_string().into())
    }

    fn render(&self, value: &Value, _ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        let c = parse_volumes(Some(value))?;
        let fstab: String = c.volumes.iter().map(|r| fstab_line(r) + "\n").collect();
        let crypttab: String = c
            .volumes
            .iter()
            .filter_map(|r| {
                r.luks_uuid
                    .as_ref()
                    .map(|u| format!("{} UUID={u} none luks,noauto\n", mapper_name(&r.label)))
            })
            .collect();
        let mdadm: String = c.volumes.iter().map(mdadm_lines).collect();
        Ok(vec![
            ManagedFile::block(FSTAB, "storage", fstab),
            ManagedFile::block(CRYPTTAB, "storage", crypttab),
            ManagedFile::block(MDADM_CONF, "storage", mdadm),
        ])
    }

    fn reconcile(
        &self,
        from: Option<&Value>,
        to: Option<&Value>,
        ctx: &ApplyCtx,
    ) -> Result<(), String> {
        self.reload(to, ctx)?;
        // The initramfs carries its own copy of mdadm.conf (with `AUTO -all` on synodeb only
        // listed arrays are assembled): refresh it when the set of arrays changes.
        if md_uuids(from) == md_uuids(to) {
            return Ok(());
        }
        if ctx.rolling_back() {
            // a rollback never waits minutes for mkinitramfs under the configuration lock;
            // the arrays put back are assembled from /etc/mdadm/mdadm.conf after the root
            (ctx.log)("storage.volumes: rolling back: the initramfs is not regenerated; run `update-initramfs -u` if an array must be assembled before the root");
            return Ok(());
        }
        if ctx.root == Path::new("/") && !Path::new(UPDATE_INITRAMFS).exists() {
            (ctx.log)("storage.volumes: update-initramfs not found, initramfs mdadm.conf not refreshed");
            return Ok(());
        }
        (ctx.log)("storage.volumes: arrays changed, refreshing the initramfs");
        // never fatal: data arrays are assembled after the root from /etc/mdadm/mdadm.conf;
        // the initramfs copy only matters for early assembly. A failure must not undo the
        // volume (nor stop a growth half way): an initramfs left without kernel modules (the
        // machine would not boot) is put back, and the administrators are told
        // ([`INITRAMFS_ALERT`]).
        refresh_initramfs(ctx);
        Ok(())
    }

    fn reload(&self, _value: Option<&Value>, ctx: &ApplyCtx) -> Result<(), String> {
        // systemd regenerates the mount units from fstab
        if systemd_running() {
            ctx.system
                .run(&Cmd::new("systemctl").arg("daemon-reload"))
                .map(|_| ())
        } else {
            (ctx.log)("storage.volumes: systemd is not running, no daemon-reload");
            Ok(())
        }
    }
}

// ------------------------------------------------------------------------------- tuning

pub fn parse_tuning(v: Option<&Value>) -> Result<StorageTuning, String> {
    match v {
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("storage.disks: {e}")),
        None => Ok(StorageTuning::default()),
    }
}

/// What the per-type rule sets (docs/research/disk-tuning.md §3-4): the scheduler only.
/// Read-ahead stays the kernel's (6.17+ sizes it from the device, btrfs and md read ahead on
/// their own device), APM and spin-down stay as the drive has them unless overridden: the
/// standby timer is opt-in.
pub fn type_defaults(kind: DiskKind) -> DiskSettings {
    let scheduler = match kind {
        DiskKind::Hdd | DiskKind::Ssd => IoScheduler::MqDeadline,
        DiskKind::Nvme => IoScheduler::None,
        DiskKind::Unknown => return DiskSettings::default(),
    };
    DiskSettings {
        scheduler: Some(scheduler),
        read_ahead_kb: None,
        apm: None,
        standby_s: None,
    }
}

/// Defaults with an override on top.
pub fn effective(kind: DiskKind, o: Option<&DiskSettings>) -> DiskSettings {
    let mut d = type_defaults(kind);
    if let Some(o) = o {
        d.scheduler = o.scheduler.or(d.scheduler);
        d.read_ahead_kb = o.read_ahead_kb.or(d.read_ahead_kb);
        d.apm = o.apm.or(d.apm);
        d.standby_s = o.standby_s.or(d.standby_s);
    }
    d
}

/// `hdparm -S` value for a spin-down delay: 1-240 are multiples of 5 s, 241-251 multiples
/// of 30 min; rounded up. 0 disables the timer.
pub fn standby_code(s: u32) -> u8 {
    match s {
        0 => 0,
        1..=1200 => s.div_ceil(5) as u8,
        _ => (240 + s.div_ceil(1800)).min(251) as u8,
    }
}

/// Serial numbers end up in a udev match: a conservative character set.
pub fn valid_serial(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// md `stripe_cache_size` accepted by the kernel.
pub const STRIPE_CACHE: std::ops::RangeInclusive<u32> = 17..=32768;

fn assignments(s: &DiskSettings) -> String {
    let mut out = String::new();
    if let Some(x) = s.scheduler {
        out.push_str(&format!(", ATTR{{queue/scheduler}}=\"{}\"", x.as_str()));
    }
    if let Some(x) = s.read_ahead_kb {
        out.push_str(&format!(", ATTR{{queue/read_ahead_kb}}=\"{x}\""));
    }
    let mut hdparm = String::new();
    if let Some(x) = s.apm {
        hdparm.push_str(&format!(" -B {x}"));
    }
    if let Some(x) = s.standby_s {
        hdparm.push_str(&format!(" -S {}", standby_code(x)));
    }
    if !hdparm.is_empty() {
        out.push_str(&format!(", RUN+=\"/usr/sbin/hdparm -q{hdparm} $devnode\""));
    }
    out
}

const DISK: &str = "ACTION==\"add|change\", SUBSYSTEM==\"block\", ENV{DEVTYPE}==\"disk\"";

/// The udev rules file.
pub fn render_rules(c: &StorageTuning) -> String {
    let mut r = String::from(
        "# rynas storage settings (D-SYSTUNE): per disk type, then per disk (by serial)\n",
    );
    let hdd = assignments(&type_defaults(DiskKind::Hdd));
    let ssd = assignments(&type_defaults(DiskKind::Ssd));
    let nvme = assignments(&type_defaults(DiskKind::Nvme));
    r.push_str(&format!(
        "{DISK}, KERNEL==\"sd*\", ATTR{{queue/rotational}}==\"1\"{hdd}\n"
    ));
    r.push_str(&format!(
        "{DISK}, KERNEL==\"sd*\", ATTR{{queue/rotational}}==\"0\"{ssd}\n"
    ));
    r.push_str(&format!("{DISK}, KERNEL==\"nvme[0-9]*n[0-9]*\"{nvme}\n"));
    for o in &c.disks {
        r.push_str(&format!(
            "{DISK}, ENV{{ID_SERIAL_SHORT}}==\"{}\"{}\n",
            o.serial,
            assignments(&o.settings)
        ));
    }
    for a in &c.arrays {
        r.push_str(&format!(
            "ACTION==\"add|change\", SUBSYSTEM==\"block\", KERNEL==\"md*\", ENV{{MD_UUID}}==\"{}\", ATTR{{md/stripe_cache_size}}=\"{}\"\n",
            a.md_uuid, a.stripe_cache_size
        ));
    }
    r
}

pub struct TuningDomain;

impl Applier for TuningDomain {
    fn key(&self) -> &str {
        TUNING_KEY
    }

    fn order(&self) -> i32 {
        6
    }

    fn validate(&self, value: Value) -> Result<Value, rynas_config::Invalid> {
        let mut c = parse_tuning(Some(&value))?;
        c.disks.retain(|o| !o.settings.is_empty());
        c.disks.sort_by(|a, b| a.serial.cmp(&b.serial));
        for w in c.disks.windows(2) {
            if w[0].serial == w[1].serial {
                return Err(format!("disk {} is listed twice", w[0].serial).into());
            }
        }
        for o in &c.disks {
            if !valid_serial(&o.serial) {
                return Err(format!(
                    "serial {:?}: only letters, digits, '.', '_' and '-' can be matched",
                    o.serial
                )
                .into());
            }
            let s = &o.settings;
            if s.read_ahead_kb.is_some_and(|r| r > 65536) {
                return Err(format!("{}: read-ahead above 65536 KiB", o.serial).into());
            }
            if s.apm == Some(0) {
                return Err(format!("{}: APM levels are 1-255", o.serial).into());
            }
            if s.standby_s.is_some_and(|x| x > 19800) {
                return Err(format!("{}: spin-down delay above 5.5 hours", o.serial).into());
            }
        }
        c.arrays.sort_by(|a, b| a.md_uuid.cmp(&b.md_uuid));
        for w in c.arrays.windows(2) {
            if w[0].md_uuid == w[1].md_uuid {
                return Err(format!("array {} is listed twice", w[0].md_uuid).into());
            }
        }
        for a in &c.arrays {
            if !is_uuid(&a.md_uuid) {
                return Err(format!("invalid md UUID {:?}", a.md_uuid).into());
            }
            if !STRIPE_CACHE.contains(&a.stripe_cache_size) {
                return Err(format!(
                    "stripe_cache_size must be between {} and {}",
                    STRIPE_CACHE.start(),
                    STRIPE_CACHE.end()
                )
                .into());
            }
        }
        serde_json::to_value(c).map_err(|e| e.to_string().into())
    }

    fn render(&self, value: &Value, _ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        let c = parse_tuning(Some(value))?;
        Ok(vec![ManagedFile::whole(UDEV_RULES, render_rules(&c), true)])
    }

    fn check_files(&self, files: &[Candidate], ctx: &ApplyCtx) -> Result<(), String> {
        for f in files {
            let path = f.staged.to_string_lossy();
            ctx.system
                .run(&Cmd::new("udevadm").args(["verify", "--resolve-names=never", &path]))
                .map_err(|e| format!("udevadm verify: {e}"))?;
        }
        Ok(())
    }

    fn reconcile(
        &self,
        from: Option<&Value>,
        to: Option<&Value>,
        ctx: &ApplyCtx,
    ) -> Result<(), String> {
        if from == to {
            return Ok(());
        }
        // no udev daemon (containers): the rules apply at the next boot
        if ctx.root == Path::new("/") && !Path::new("/run/udev/control").exists() {
            (ctx.log)("storage.disks: udev is not running, the rules apply at the next boot");
            return Ok(());
        }
        // new rules for the disks that are there now (hotplugged ones get them on add)
        ctx.system
            .run(&Cmd::new("udevadm").args(["control", "--reload"]))?;
        ctx.system.run(&Cmd::new("udevadm").args([
            "trigger",
            "--action=change",
            "--subsystem-match=block",
            "--property-match=DEVTYPE=disk",
        ]))?;
        // wait for the rules' hdparm runs, so the values read right after the apply (APM,
        // write cache) are the new ones; a slow disk is not an apply failure
        if let Err(e) = ctx
            .system
            .run(&Cmd::new("udevadm").args(["settle", "--timeout=30"]))
        {
            (ctx.log)(&format!("storage.disks: udevadm settle: {e}"));
        }
        Ok(())
    }
}

/// md `stripe_cache_size` proposal for a RAID5/6 array: the largest power of two up to 8192
/// whose memory (entries × 4 KiB × members) stays within 2% of the RAM; never below the
/// kernel default of 256.
pub fn propose_stripe_cache(level: &str, members: u32, ram_bytes: u64) -> Option<(u32, u64)> {
    if !matches!(level, "raid4" | "raid5" | "raid6") || members == 0 {
        return None;
    }
    let per_entry = 4096 * u64::from(members);
    let budget = ram_bytes / 50;
    let mut n: u32 = 8192;
    while n > 256 && u64::from(n) * per_entry > budget {
        n /= 2;
    }
    Some((n, u64::from(n) * per_entry))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::storage::{ArrayTuning, DiskOverride, RaidLevel, TierRecord};
    use rynas_config::{Config, ConfigOptions, MemorySystem};
    use std::sync::Arc;

    fn record(label: &str, luks: bool) -> VolumeRecord {
        VolumeRecord {
            label: label.into(),
            md_uuid: "0a1b2c3d:4e5f6071:8293a4b5:c6d7e8f9".into(),
            level: RaidLevel::Raid1,
            luks_uuid: luks.then(|| "11111111-2222-3333-4444-555555555555".into()),
            fs_uuid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
            mountpoint: format!("/{label}"),
            options: "noatime,compress=zstd:1".into(),
            created_ms: 1,
            ..Default::default()
        }
    }

    #[test]
    fn volumes_render_blocks_and_validate() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::write(
            root.join("etc/fstab"),
            "UUID=root-uuid / ext4 errors=remount-ro 0 1\n",
        )
        .unwrap();
        let sys = Arc::new(MemorySystem::new("nas"));
        let cfg = Config::open(
            ConfigOptions::new(dir.path().join("state"), &root),
            sys,
            vec![Arc::new(VolumesDomain), Arc::new(TuningDomain)],
        )
        .unwrap();
        let doc = StorageVolumes {
            volumes: vec![record("vol2", true), record("photos", false)],
        };
        cfg.apply_value(
            "alice",
            VOLUMES_KEY,
            Some(serde_json::to_value(&doc).unwrap()),
            "volume created",
            &|_| {},
        )
        .unwrap();
        let fstab = std::fs::read_to_string(root.join("etc/fstab")).unwrap();
        assert!(fstab.starts_with("UUID=root-uuid / ext4"), "{fstab}");
        assert!(fstab.contains(
            "UUID=aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee /photos btrfs noatime,compress=zstd:1,nofail 0 0\n"
        ));
        assert!(fstab.contains("/vol2 btrfs noatime,compress=zstd:1,nofail,noauto 0 0\n"));
        let crypttab = std::fs::read_to_string(root.join("etc/crypttab")).unwrap();
        assert!(
            crypttab.contains(
                "rynas-vol2 UUID=11111111-2222-3333-4444-555555555555 none luks,noauto\n"
            )
        );
        assert!(!crypttab.contains("photos"));
        let mdadm = std::fs::read_to_string(root.join("etc/mdadm/mdadm.conf")).unwrap();
        assert!(mdadm.contains(
            "ARRAY /dev/md/photos metadata=1.2 UUID=0a1b2c3d:4e5f6071:8293a4b5:c6d7e8f9\n"
        ));
        // drift: the blocks are tracked
        assert!(cfg.drift().unwrap().entries.is_empty());
        // removing a volume removes its lines, other lines stay
        let doc = StorageVolumes {
            volumes: vec![record("photos", false)],
        };
        cfg.apply_value(
            "alice",
            VOLUMES_KEY,
            Some(serde_json::to_value(&doc).unwrap()),
            "",
            &|_| {},
        )
        .unwrap();
        let fstab = std::fs::read_to_string(root.join("etc/fstab")).unwrap();
        assert!(!fstab.contains("/vol2") && fstab.contains("/photos"));
        assert!(fstab.contains("UUID=root-uuid / ext4"));
        // validation
        let mut bad = record("vol3", false);
        bad.mountpoint = "/etc".into();
        let v = serde_json::to_value(StorageVolumes { volumes: vec![bad] }).unwrap();
        assert!(VolumesDomain.validate(v).is_err());
        let mut bad = record("vol3", false);
        bad.options = "noatime ; rm".into();
        let v = serde_json::to_value(StorageVolumes { volumes: vec![bad] }).unwrap();
        assert!(VolumesDomain.validate(v).is_err());
    }

    #[test]
    fn udev_rules_defaults_overrides_and_stripe_cache() {
        let c = StorageTuning {
            disks: vec![DiskOverride {
                serial: "WD-WX12345".into(),
                settings: DiskSettings {
                    scheduler: Some(IoScheduler::Bfq),
                    read_ahead_kb: None,
                    apm: Some(127),
                    standby_s: Some(1200),
                },
            }],
            arrays: vec![ArrayTuning {
                md_uuid: "0a1b2c3d:4e5f6071:8293a4b5:c6d7e8f9".into(),
                stripe_cache_size: 4096,
            }],
        };
        let r = render_rules(&c);
        assert!(r.contains("KERNEL==\"sd*\", ATTR{queue/rotational}==\"1\", ATTR{queue/scheduler}=\"mq-deadline\"\n"), "{r}");
        assert!(r.contains("KERNEL==\"nvme[0-9]*n[0-9]*\", ATTR{queue/scheduler}=\"none\"\n"));
        assert!(!r.contains("read_ahead_kb"), "{r}");
        assert!(r.contains("ENV{ID_SERIAL_SHORT}==\"WD-WX12345\", ATTR{queue/scheduler}=\"bfq\", RUN+=\"/usr/sbin/hdparm -q -B 127 -S 240 $devnode\"\n"), "{r}");
        assert!(r.contains("ENV{MD_UUID}==\"0a1b2c3d:4e5f6071:8293a4b5:c6d7e8f9\", ATTR{md/stripe_cache_size}=\"4096\"\n"));
        assert_eq!(standby_code(0), 0);
        assert_eq!(standby_code(5), 1);
        assert_eq!(standby_code(6), 2);
        assert_eq!(standby_code(1201), 241);
        assert_eq!(standby_code(3600), 242);
        assert_eq!(standby_code(19800), 251);
        // validation: udev-unsafe serials and empty overrides
        let bad = StorageTuning {
            disks: vec![DiskOverride {
                serial: "X\", RUN+=\"/bin/sh".into(),
                settings: DiskSettings {
                    apm: Some(1),
                    ..Default::default()
                },
            }],
            arrays: vec![],
        };
        assert!(
            TuningDomain
                .validate(serde_json::to_value(bad).unwrap())
                .is_err()
        );
        let empty = StorageTuning {
            disks: vec![DiskOverride {
                serial: "S1".into(),
                settings: DiskSettings::default(),
            }],
            arrays: vec![],
        };
        let v = TuningDomain
            .validate(serde_json::to_value(empty).unwrap())
            .unwrap();
        assert_eq!(parse_tuning(Some(&v)).unwrap().disks.len(), 0);
        let mut big = c.clone();
        big.arrays[0].stripe_cache_size = 65536;
        assert!(
            TuningDomain
                .validate(serde_json::to_value(big).unwrap())
                .is_err()
        );
    }

    #[test]
    fn tuning_apply_verifies_and_triggers_udev() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let sys = Arc::new(MemorySystem::new("nas"));
        let cfg = Config::open(
            ConfigOptions::new(dir.path().join("state"), &root),
            sys.clone(),
            vec![Arc::new(TuningDomain)],
        )
        .unwrap();
        let v = serde_json::to_value(StorageTuning::default()).unwrap();
        cfg.stage("alice", TUNING_KEY, Some(v)).unwrap();
        cfg.apply("alice", "alice", "", &|_| {}, &|| false).unwrap();
        let rules = std::fs::read_to_string(root.join(UDEV_RULES.trim_start_matches('/'))).unwrap();
        assert!(rules.starts_with("# Managed by rynas"));
        let calls = sys.state().calls.join("\n");
        assert!(
            calls.contains("run udevadm verify --resolve-names=never"),
            "{calls}"
        );
        assert!(calls.contains("run udevadm control --reload"), "{calls}");
        assert!(calls.contains("run udevadm trigger --action=change --subsystem-match=block --property-match=DEVTYPE=disk"));
    }

    #[test]
    fn volumes_refresh_initramfs_only_when_arrays_change() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("etc/mdadm")).unwrap();
        std::fs::write(root.join("etc/fstab"), "").unwrap();
        let sys = Arc::new(MemorySystem::new("nas"));
        let cfg = Config::open(
            ConfigOptions::new(dir.path().join("state"), &root),
            sys.clone(),
            vec![Arc::new(VolumesDomain)],
        )
        .unwrap();
        let count = || sys.state().calls.iter().filter(|c| c.contains("update-initramfs -u")).count();
        let mut c = StorageVolumes::default();
        c.volumes.push(record("photos", false));
        cfg.stage("alice", VOLUMES_KEY, Some(serde_json::to_value(&c).unwrap())).unwrap();
        cfg.apply("alice", "alice", "", &|_| {}, &|| false).unwrap();
        assert_eq!(count(), 1, "new array");
        // same arrays, other mount options: no initramfs rebuild
        c.volumes[0].options = "noatime".into();
        cfg.stage("alice", VOLUMES_KEY, Some(serde_json::to_value(&c).unwrap())).unwrap();
        cfg.apply("alice", "alice", "", &|_| {}, &|| false).unwrap();
        assert_eq!(count(), 1, "unchanged arrays");
    }

    #[test]
    fn kernel_release_order_and_newest_initrd() {
        use std::cmp::Ordering::*;
        assert_eq!(cmp_release("6.12.9-amd64", "6.12.10-amd64"), Less);
        assert_eq!(cmp_release("7.3.4-11-synodeb", "7.3.3-10-synodeb"), Greater);
        assert_eq!(cmp_release("7.0~rc1", "7.0"), Less);
        assert_eq!(cmp_release("6.1.0-01", "6.1.0-1"), Equal);
        assert_eq!(cmp_release("6.1.0-1", "6.1.0-1-rt"), Less);
        let d = tempfile::tempdir().unwrap();
        for f in ["initrd.img-6.12.9-amd64", "initrd.img-6.12.10-amd64", "config-7.0.0"] {
            std::fs::write(d.path().join(f), "x").unwrap();
        }
        std::os::unix::fs::symlink("initrd.img-6.12.9-amd64", d.path().join("initrd.img-9.9.9")).unwrap();
        std::os::unix::fs::symlink("initrd.img-6.12.9-amd64", d.path().join("initrd.img")).unwrap();
        assert_eq!(newest_initrd(d.path()).as_deref(), Some("6.12.10-amd64"));
        assert_eq!(newest_initrd(&d.path().join("missing")), None);
    }

    #[test]
    fn initrd_check_decisions() {
        let list = "usr/lib/modules/7.3.4/kernel/drivers/ata/ahci.ko.xz\nusr/lib/modules/7.3.4/kernel/fs/btrfs/btrfs.ko\nusr/lib/modules/7.3.4/modules.dep\netc/mdadm/mdadm.conf\n";
        assert_eq!(count_modules(list), 2);
        assert_eq!(count_modules("etc/mdadm/mdadm.conf\n"), 0);
        assert_eq!(check_initrd(Some(55), Some(55)), InitrdCheck::Fine);
        assert_eq!(check_initrd(Some(55), Some(40)), InitrdCheck::Fine);
        assert_eq!(check_initrd(None, Some(3)), InitrdCheck::Fine);
        assert_eq!(check_initrd(Some(0), Some(0)), InitrdCheck::Fine, "everything built in");
        assert!(matches!(check_initrd(Some(55), Some(0)), InitrdCheck::Broken(_)));
        assert!(matches!(check_initrd(Some(55), None), InitrdCheck::Broken(_)));
        assert_eq!(check_initrd(None, None), InitrdCheck::Unverified);
        assert_eq!(check_initrd(None, Some(0)), InitrdCheck::Unverified);
    }

    /// update-initramfs writes `produce` into the initramfs; lsinitramfs lists two modules for
    /// "GOOD" and none for anything else.
    struct InitrdSystem {
        root: std::path::PathBuf,
        produce: &'static str,
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl rynas_config::System for InitrdSystem {
        fn hostname(&self) -> Result<String, String> {
            Ok("nas".into())
        }
        fn set_hostname(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn reload_or_restart_unit(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn unit_active(&self, _: &str) -> Result<bool, String> {
            Ok(true)
        }
        fn run(&self, cmd: &Cmd) -> Result<rynas_tasks::CmdOutput, String> {
            let shown = cmd.display();
            self.calls.lock().unwrap().push(shown.clone());
            let mut stdout = String::new();
            if shown.contains("lsinitramfs") {
                let path = shown.split_whitespace().last().unwrap();
                if std::fs::read_to_string(path).unwrap_or_default() == "GOOD" {
                    stdout = "lib/modules/k/ahci.ko.zst\nlib/modules/k/md-mod.ko.zst\n".into();
                }
            } else if let Some(k) = shown.split(" -k ").nth(1) {
                let k = k.split_whitespace().next().unwrap();
                std::fs::write(self.root.join(format!("boot/initrd.img-{k}")), self.produce).unwrap();
            }
            Ok(rynas_tasks::CmdOutput { code: Some(0), stdout, stderr: String::new() })
        }
    }

    fn initrd_config(produce: &'static str) -> (tempfile::TempDir, Arc<InitrdSystem>, Config) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("etc/mdadm")).unwrap();
        std::fs::create_dir_all(root.join("boot")).unwrap();
        std::fs::write(root.join("etc/fstab"), "").unwrap();
        std::fs::write(root.join("boot/initrd.img-6.12.9-amd64"), "GOOD").unwrap();
        std::fs::write(root.join("boot/initrd.img-6.12.10-amd64"), "GOOD").unwrap();
        let sys = Arc::new(InitrdSystem { root: root.clone(), produce, calls: Default::default() });
        let cfg = Config::open(
            ConfigOptions::new(dir.path().join("state"), &root),
            sys.clone(),
            vec![Arc::new(VolumesDomain)],
        )
        .unwrap();
        (dir, sys, cfg)
    }

    #[test]
    fn initramfs_refresh_keeps_a_good_initrd() {
        let (_dir, sys, cfg) = initrd_config("GOOD");
        let mut c = StorageVolumes::default();
        c.volumes.push(record("photos", false));
        cfg.stage("alice", VOLUMES_KEY, Some(serde_json::to_value(&c).unwrap())).unwrap();
        cfg.apply("alice", "alice", "", &|_| {}, &|| false).unwrap();
        let calls = sys.calls.lock().unwrap().join("\n");
        assert!(calls.contains("/usr/sbin/update-initramfs -u -k 6.12.10-amd64"), "{calls}");
        assert!(!calls.contains("systemd-run"), "tests never leave the root: {calls}");
        let backup = sys.root.join("var/lib/rynas/initramfs/initrd.img-6.12.10-amd64");
        assert!(!backup.exists(), "the copy is removed once the new initramfs is checked");
    }

    #[test]
    fn initramfs_without_modules_is_put_back_and_fails() {
        let (_dir, sys, cfg) = initrd_config("EMPTY");
        let mut c = StorageVolumes::default();
        c.volumes.push(record("photos", false));
        cfg.stage("alice", VOLUMES_KEY, Some(serde_json::to_value(&c).unwrap())).unwrap();
        // the volume is kept: the initramfs is put back and the administrators are told
        cfg.apply("alice", "alice", "", &|_| {}, &|| false).unwrap();
        assert!(cfg.applied(VOLUMES_KEY).unwrap().is_some());
        let target = sys.root.join("boot/initrd.img-6.12.10-amd64");
        assert_eq!(std::fs::read_to_string(target).unwrap(), "GOOD");
        assert_eq!(std::fs::read_to_string(sys.root.join("boot/initrd.img-6.12.9-amd64")).unwrap(), "GOOD");
        assert!(!sys.root.join("var/lib/rynas/initramfs/initrd.img-6.12.10-amd64").exists());
        let a = take_initramfs_alert(&sys.root).expect("alert");
        assert_eq!(a.kind, InitramfsAlertKind::Rejected);
        assert_eq!(a.release, "6.12.10-amd64");
        assert!(take_initramfs_alert(&sys.root).is_none(), "read once");
        // update-initramfs ran once: no second run for a rollback
        let runs = sys.calls.lock().unwrap().iter().filter(|c| c.contains("update-initramfs")).count();
        assert_eq!(runs, 1);
    }

    /// A rollback of a change of the arrays never regenerates the initramfs (minutes under
    /// the configuration lock): only the apply does.
    #[test]
    fn a_rollback_does_not_regenerate_the_initramfs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("etc/mdadm")).unwrap();
        std::fs::write(root.join("etc/fstab"), "").unwrap();
        let sys = Arc::new(MemorySystem::new("nas"));
        let cfg = Config::open(
            ConfigOptions::new(dir.path().join("state"), &root),
            sys.clone(),
            vec![Arc::new(VolumesDomain), Arc::new(FailsHealth)],
        )
        .unwrap();
        let count = || sys.state().calls.iter().filter(|c| c.contains("update-initramfs")).count();
        let mut c = StorageVolumes::default();
        c.volumes.push(record("photos", false));
        cfg.stage("alice", VOLUMES_KEY, Some(serde_json::to_value(&c).unwrap())).unwrap();
        cfg.stage("alice", "test.fails", Some(serde_json::json!(1))).unwrap();
        assert!(cfg.apply("alice", "alice", "", &|_| {}, &|| false).is_err());
        assert_eq!(count(), 1, "the apply regenerates it, the rollback does not");
        assert!(cfg.applied(VOLUMES_KEY).unwrap().is_none());
    }

    /// A later domain whose health check always fails (its apply is rolled back).
    struct FailsHealth;

    impl rynas_config::Applier for FailsHealth {
        fn key(&self) -> &str {
            "test.fails"
        }
        fn order(&self) -> i32 {
            50
        }
        fn validate(&self, value: Value) -> Result<Value, rynas_config::Invalid> {
            Ok(value)
        }
        fn render(&self, _: &Value, _: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
            Ok(Vec::new())
        }
        fn health(&self, _: Option<&Value>, _: &ApplyCtx) -> Result<(), String> {
            Err("always".into())
        }
    }

    #[test]
    fn update_initramfs_runs_outside_the_sandbox() {
        let c = update_initramfs_cmd(&["-u", "-k", "7.3.4-11-synodeb"], true);
        let shown = c.display();
        assert!(shown.starts_with("systemd-run --wait --pipe"), "{shown}");
        assert!(shown.contains("--unit=rynas-initramfs-"), "{shown}");
        assert!(!shown.contains("RuntimeMaxSec"), "keep running: {shown}");
        assert!(shown.ends_with("-- /usr/sbin/update-initramfs -u -k 7.3.4-11-synodeb"), "{shown}");
        assert!(rynas_tasks::transient_unit(&c, INITRAMFS_UNIT).is_some());
        let d = update_initramfs_cmd(&["-u"], false);
        assert_eq!(d.display(), "/usr/sbin/update-initramfs -u");
    }

    fn flex_record(label: &str) -> VolumeRecord {
        VolumeRecord {
            md_uuid: String::new(),
            level: RaidLevel::Flex1,
            tiers: vec![
                TierRecord { index: 1, md_uuid: "11111111:22222222:33333333:44444444".into() },
                TierRecord { index: 2, md_uuid: "55555555:66666666:77777777:88888888".into() },
            ],
            vg: Some(format!("rynas_{label}")),
            ..record(label, false)
        }
    }

    #[test]
    fn flexible_volume_records() {
        let ok = |r: VolumeRecord| {
            VolumesDomain
                .validate(serde_json::to_value(StorageVolumes { volumes: vec![r] }).unwrap())
                .is_ok()
        };
        assert!(ok(flex_record("vol2")));
        assert!(ok(record("vol2", false)));
        // one form or the other, never both, never neither
        let mut r = flex_record("vol2");
        r.md_uuid = "0a1b2c3d:4e5f6071:8293a4b5:c6d7e8f9".into();
        assert!(!ok(r));
        let mut r = flex_record("vol2");
        r.tiers.clear();
        assert!(!ok(r));
        let mut r = record("vol2", false);
        r.tiers = flex_record("vol2").tiers;
        assert!(!ok(r));
        let mut r = flex_record("vol2");
        r.tiers[1].index = 3;
        assert!(!ok(r), "tiers numbered without gaps");
        let mut r = flex_record("vol2");
        r.vg = Some("bad name".into());
        assert!(!ok(r));
        let mut r = flex_record("vol2");
        r.vg = None;
        assert!(!ok(r));

        // one ARRAY line per tier, and the tiers count as arrays for the initramfs
        assert_eq!(
            mdadm_lines(&flex_record("vol2")),
            "ARRAY /dev/md/vol2_t1 metadata=1.2 UUID=11111111:22222222:33333333:44444444\n\
             ARRAY /dev/md/vol2_t2 metadata=1.2 UUID=55555555:66666666:77777777:88888888\n"
        );
        let v = serde_json::to_value(StorageVolumes {
            volumes: vec![flex_record("vol2"), record("photos", false)],
        })
        .unwrap();
        assert_eq!(md_uuids(Some(&v)).len(), 3);
        let mut grown = flex_record("vol2");
        grown.tiers.push(TierRecord { index: 3, md_uuid: "99999999:aaaaaaaa:bbbbbbbb:cccccccc".into() });
        let w = serde_json::to_value(StorageVolumes { volumes: vec![grown, record("photos", false)] })
            .unwrap();
        assert_ne!(md_uuids(Some(&v)), md_uuids(Some(&w)), "a new tier refreshes the initramfs");
    }

    #[test]
    fn stripe_cache_proposal() {
        // 4 GB NAS, 4-disk RAID5: 2% = 80 MiB → 4096 entries × 16 KiB = 64 MiB
        let (n, mem) = propose_stripe_cache("raid5", 4, 4 << 30).unwrap();
        assert_eq!((n, mem), (4096, 4096 * 4096 * 4));
        assert_eq!(propose_stripe_cache("raid6", 8, 1 << 30).unwrap().0, 512);
        assert_eq!(propose_stripe_cache("raid1", 2, 4 << 30), None);
        assert_eq!(propose_stripe_cache("raid5", 3, 64 << 30).unwrap().0, 8192);
    }
}
