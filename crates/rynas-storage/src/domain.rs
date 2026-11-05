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
        .map(|c| c.volumes.into_iter().map(|r| r.md_uuid).collect())
        .unwrap_or_default()
}

fn systemd_running() -> bool {
    Path::new("/run/systemd/system").exists()
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
            if !is_uuid(&r.md_uuid) || !is_uuid(&r.fs_uuid) {
                return Err(format!("volume {}: invalid UUID", r.label).into());
            }
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
        let mdadm: String = c
            .volumes
            .iter()
            .map(|r| {
                format!(
                    "ARRAY /dev/md/{} metadata=1.2 UUID={}\n",
                    r.label, r.md_uuid
                )
            })
            .collect();
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
        if ctx.root == Path::new("/") && !Path::new(UPDATE_INITRAMFS).exists() {
            (ctx.log)("storage.volumes: update-initramfs not found, initramfs mdadm.conf not refreshed");
            return Ok(());
        }
        (ctx.log)("storage.volumes: arrays changed, refreshing the initramfs");
        ctx.system
            .run(&Cmd::new("update-initramfs").arg("-u"))
            .map(|_| ())
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

/// What the per-type rule sets (docs/research/system-reliability-tweaks.md §6).
pub fn type_defaults(kind: DiskKind) -> DiskSettings {
    let (scheduler, ra) = match kind {
        DiskKind::Hdd => (IoScheduler::MqDeadline, 2048),
        DiskKind::Ssd => (IoScheduler::MqDeadline, 256),
        DiskKind::Nvme => (IoScheduler::None, 128),
        DiskKind::Unknown => return DiskSettings::default(),
    };
    DiskSettings {
        scheduler: Some(scheduler),
        read_ahead_kb: Some(ra),
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
    use rynas_api_types::storage::{ArrayTuning, DiskOverride, RaidLevel};
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
        assert!(r.contains("KERNEL==\"sd*\", ATTR{queue/rotational}==\"1\", ATTR{queue/scheduler}=\"mq-deadline\", ATTR{queue/read_ahead_kb}=\"2048\"\n"), "{r}");
        assert!(r.contains("KERNEL==\"nvme[0-9]*n[0-9]*\", ATTR{queue/scheduler}=\"none\", ATTR{queue/read_ahead_kb}=\"128\"\n"));
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
    fn stripe_cache_proposal() {
        // 4 GB NAS, 4-disk RAID5: 2% = 80 MiB → 4096 entries × 16 KiB = 64 MiB
        let (n, mem) = propose_stripe_cache("raid5", 4, 4 << 30).unwrap();
        assert_eq!((n, mem), (4096, 4096 * 4096 * 4));
        assert_eq!(propose_stripe_cache("raid6", 8, 1 << 30).unwrap().0, 512);
        assert_eq!(propose_stripe_cache("raid1", 2, 4 << 30), None);
        assert_eq!(propose_stripe_cache("raid5", 3, 64 << 30).unwrap().0, 8192);
    }
}
