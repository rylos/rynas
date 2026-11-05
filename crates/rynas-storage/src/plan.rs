//! The planner: validates a storage change against the current inventory and returns the
//! exact ordered list of steps that would make it, with capacity and fault tolerance. It
//! never executes anything; the task engine will (D-TASKS).
//!
//! Volume layout (D-STO, D-CRYPT, D-COMPRESS, D-SNAP): one GPT partition per disk →
//! md (metadata 1.2, internal bitmap) → optional LUKS2 → btrfs (`-d single -m dup`, the
//! redundancy comes from md; DUP metadata lets btrfs repair metadata checksum errors that md
//! cannot arbitrate) mounted at `/<label>` with `noatime,compress=zstd:1`, with the
//! `.snapshots` and `@rynas-cache` subvolumes.

use rynas_api_types::storage::{
    ArrayHealth, DiskKind, DiskSummary, GrowPlanRequest, HealthVerdict, MdArray, MemberRole,
    PlanAction, PlanIssue, PlanIssueCode, PlanStep, RaidLevel, ReplacePlanRequest,
    ScrubPlanRequest, StoragePlan, VolumePlanRequest,
};

use crate::Inventory;

const MIB: u64 = 1 << 20;
/// Partition starts at 1 MiB (sgdisk default alignment).
const PART_START: u64 = MIB;
/// Left free at the end of each disk so that a replacement "of the same size" from another
/// vendor (a few MB smaller) still fits.
pub const END_RESERVE: u64 = 128 * MIB;
/// md 1.2 superblock, bitmap and data offset (mdadm reserves room for reshapes). Estimate.
const MD_OVERHEAD: u64 = 128 * MIB;
/// LUKS2 header with the default 16 MiB metadata area.
const LUKS_HEADER: u64 = 16 * MIB;
/// Argon2id memory cap (KiB) so a 4 GB NAS does not run out of memory while unlocking.
pub const PBKDF_MEMORY_KIB: &str = "262144";
pub const COMPRESSION: &str = "zstd:1";

fn cmd(description: &str, argv: &[&str]) -> PlanStep {
    PlanStep {
        description: description.to_string(),
        action: PlanAction::Command {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            stdin: None,
        },
        destroys: Vec::new(),
    }
}

fn cmd_secret(description: &str, argv: &[&str], secret: &str) -> PlanStep {
    let mut s = cmd(description, argv);
    if let PlanAction::Command { stdin, .. } = &mut s.action {
        *stdin = Some(secret.to_string());
    }
    s
}

fn destroying(mut s: PlanStep, devs: &[&str]) -> PlanStep {
    s.destroys = devs.iter().map(|d| d.to_string()).collect();
    s
}

fn config(description: &str, file: &str, entry: &str) -> PlanStep {
    PlanStep {
        description: description.to_string(),
        action: PlanAction::Config {
            file: file.into(),
            entry: entry.into(),
        },
        destroys: Vec::new(),
    }
}

fn wait(description: &str, condition: &str) -> PlanStep {
    PlanStep {
        description: description.to_string(),
        action: PlanAction::Wait {
            condition: condition.into(),
        },
        destroys: Vec::new(),
    }
}

fn sysfs(description: &str, path: &str, value: &str) -> PlanStep {
    PlanStep {
        description: description.to_string(),
        action: PlanAction::WriteSysfs {
            path: path.into(),
            value: value.into(),
        },
        destroys: Vec::new(),
    }
}

/// First partition of a disk: "sdb" → "sdb1", "nvme0n1" → "nvme0n1p1".
pub fn part_name(disk: &str, n: u32) -> String {
    if disk.ends_with(|c: char| c.is_ascii_digit()) {
        format!("{disk}p{n}")
    } else {
        format!("{disk}{n}")
    }
}

/// systemd unit of a mount point, as `systemd-escape --path --suffix=mount` names it:
/// "/vol1" → "vol1.mount", "/media-01" → "media\x2d01.mount".
pub fn mount_unit(mountpoint: &str) -> String {
    let p = mountpoint.trim_matches('/');
    if p.is_empty() {
        return "-.mount".into();
    }
    let mut out = String::new();
    for (i, c) in p.chars().enumerate() {
        match c {
            '/' => out.push('-'),
            c if c.is_ascii_alphanumeric() || c == '_' || (c == '.' && i > 0) => out.push(c),
            c => {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("\\x{b:02x}"));
                }
            }
        }
    }
    out.push_str(".mount");
    out
}

/// Volume labels double as mount names ([`rynas_api_types::validate::volume_label`]).
pub use rynas_api_types::validate::volume_label as valid_label;

pub fn level_name(l: RaidLevel) -> &'static str {
    match l {
        RaidLevel::Single => "single",
        RaidLevel::Raid0 => "raid0",
        RaidLevel::Raid1 => "raid1",
        RaidLevel::Raid5 => "raid5",
        RaidLevel::Raid6 => "raid6",
        RaidLevel::Raid10 => "raid10",
    }
}

/// md level number for `mdadm --level`.
fn md_level(l: RaidLevel) -> &'static str {
    match l {
        RaidLevel::Single | RaidLevel::Raid1 => "1",
        RaidLevel::Raid0 => "0",
        RaidLevel::Raid5 => "5",
        RaidLevel::Raid6 => "6",
        RaidLevel::Raid10 => "10",
    }
}

/// (minimum disks, accepted count) for a level.
fn count_ok(l: RaidLevel, n: usize) -> Result<(), String> {
    let (min, extra) = match l {
        RaidLevel::Single => (1, n == 1),
        RaidLevel::Raid0 | RaidLevel::Raid1 => (2, true),
        RaidLevel::Raid5 => (3, true),
        RaidLevel::Raid6 => (4, true),
        RaidLevel::Raid10 => (4, n.is_multiple_of(2)),
    };
    if n < min {
        return Err(format!(
            "{} needs at least {min} disks, {n} selected",
            level_name(l)
        ));
    }
    if !extra {
        return Err(match l {
            RaidLevel::Single => "single takes exactly one disk".to_string(),
            _ => format!("{} needs an even number of disks", level_name(l)),
        });
    }
    Ok(())
}

/// Data-bearing members out of `n` and guaranteed fault tolerance.
pub fn capacity_factor(l: RaidLevel, n: u64) -> (u64, u32) {
    match l {
        RaidLevel::Single => (1, 0),
        RaidLevel::Raid0 => (n, 0),
        RaidLevel::Raid1 => (1, (n - 1) as u32),
        RaidLevel::Raid5 => (n - 1, 1),
        RaidLevel::Raid6 => (n - 2, 2),
        RaidLevel::Raid10 => (n / 2, 1),
    }
}

fn level_from_md(level: &str, raid_disks: u32) -> Option<RaidLevel> {
    Some(match level {
        "raid1" if raid_disks == 1 => RaidLevel::Single,
        "raid1" => RaidLevel::Raid1,
        "raid0" => RaidLevel::Raid0,
        "raid5" => RaidLevel::Raid5,
        "raid6" => RaidLevel::Raid6,
        "raid10" => RaidLevel::Raid10,
        _ => return None,
    })
}

fn usable_partition(disk_size: u64) -> u64 {
    disk_size.saturating_sub(PART_START + END_RESERVE) / MIB * MIB
}

/// Records an error of the plan: its English text and its issue (code, request field and
/// the object it names).
fn fail(
    p: &mut StoragePlan,
    code: PlanIssueCode,
    field: &str,
    subject: Option<&str>,
    message: String,
) {
    p.errors.push(message);
    p.issues.push(PlanIssue {
        code,
        field: Some(field.to_string()),
        subject: subject.map(str::to_string),
    });
}

/// Problems with using `name` (from the request field `field`) as a fresh disk: errors stop
/// the plan, warnings are shown.
fn check_new_disk(
    inv: &Inventory,
    name: &str,
    field: &str,
    p: &mut StoragePlan,
) -> Option<DiskSummary> {
    use PlanIssueCode as C;
    let Some(d) = inv.disks.iter().find(|d| d.name == name) else {
        fail(
            p,
            C::NoSuchDisk,
            field,
            Some(name),
            format!("{name}: no such disk"),
        );
        return None;
    };
    let usage = crate::inventory::disk_usage(&inv.graph, name);
    if usage.system {
        let m = format!("{name}: holds the operating system");
        fail(p, C::SystemDisk, field, Some(name), m);
    } else if usage.in_use {
        let m = format!("{name}: in use ({})", usage.used_by.join(", "));
        fail(p, C::DiskInUse, field, Some(name), m);
    } else if !usage.used_by.is_empty() {
        p.warnings.push(format!(
            "{name}: existing data will be erased ({})",
            usage.used_by.join(", ")
        ));
    }
    match d.health {
        HealthVerdict::Failing => {
            let m = format!(
                "{name}: SMART says the disk is failing ({})",
                d.health_reasons
                    .iter()
                    .map(|r| r.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            );
            fail(p, C::DiskFailing, field, Some(name), m)
        }
        HealthVerdict::Warning => p.warnings.push(format!(
            "{name}: SMART warnings ({})",
            d.health_reasons
                .iter()
                .filter(|r| r.severity >= rynas_api_types::storage::Severity::Warning)
                .map(|r| r.message.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        )),
        HealthVerdict::Unknown => p.warnings.push(format!("{name}: no SMART data yet")),
        HealthVerdict::Healthy => {}
    }
    if d.removable || d.transport == "usb" {
        p.warnings.push(format!(
            "{name}: USB/removable disk: unplugging it breaks the volume"
        ));
    }
    if d.size < 2 * (PART_START + END_RESERVE + MD_OVERHEAD) {
        fail(
            p,
            C::DiskTooSmall,
            field,
            Some(name),
            format!("{name}: too small"),
        );
    }
    Some(d.clone())
}

fn finish(mut p: StoragePlan) -> StoragePlan {
    p.valid = p.errors.is_empty();
    if !p.valid {
        p.steps.clear();
    }
    p
}

fn partition_steps(steps: &mut Vec<PlanStep>, disk: &str, label: &str) {
    let dev = format!("/dev/{disk}");
    steps.push(destroying(
        cmd(
            &format!("Erase signatures on {disk}"),
            &["wipefs", "--all", &dev],
        ),
        &[disk],
    ));
    steps.push(destroying(
        cmd(
            &format!("Clear the partition table of {disk}"),
            &["sgdisk", "--zap-all", &dev],
        ),
        &[disk],
    ));
    steps.push(cmd(
        &format!("Create one Linux RAID partition on {disk}, 128 MiB left free at the end"),
        &[
            "sgdisk",
            "--new=1:0:-128M",
            "--typecode=1:FD00",
            &format!("--change-name=1:rynas-{label}"),
            &dev,
        ],
    ));
}

/// `POST /api/v1/storage/plan/volume`
pub fn plan_volume(inv: &Inventory, req: &VolumePlanRequest) -> StoragePlan {
    let mut p = StoragePlan {
        operation: "create_volume".into(),
        ..Default::default()
    };
    let label = req.label.as_str();
    if !valid_label(label) {
        fail(
            &mut p,
            PlanIssueCode::InvalidLabel,
            "label",
            None,
            "label must be 1-16 characters: lowercase letters, digits and '-', starting with a letter (not a system directory name)"
                .into(),
        );
    } else {
        let taken = inv
            .filesystems
            .iter()
            .any(|f| f.label.as_deref() == Some(label))
            || inv.arrays.iter().any(|a| {
                a.md_name
                    .as_deref()
                    .is_some_and(|n| n == label || n.ends_with(&format!(":{label}")))
            })
            || inv
                .mounts
                .iter()
                .any(|m| m.mountpoint == format!("/{label}"));
        if taken {
            fail(
                &mut p,
                PlanIssueCode::LabelTaken,
                "label",
                None,
                format!("a volume, array or mount named {label} already exists"),
            );
        }
    }
    let mut seen = std::collections::HashSet::new();
    for d in &req.disks {
        if !seen.insert(d) {
            let m = format!("{d}: selected twice");
            fail(
                &mut p,
                PlanIssueCode::DiskSelectedTwice,
                "disks",
                Some(d),
                m,
            );
        }
    }
    if let Err(e) = count_ok(req.level, req.disks.len()) {
        let code = if req.disks.is_empty() {
            PlanIssueCode::NoDisks
        } else {
            PlanIssueCode::DiskCount
        };
        fail(&mut p, code, "disks", None, e);
    }
    let mut chosen: Vec<DiskSummary> = Vec::new();
    for name in &req.disks {
        if let Some(d) = check_new_disk(inv, name, "disks", &mut p) {
            chosen.push(d);
        }
    }
    if chosen.is_empty() {
        return finish(p);
    }

    // capacity
    let n = chosen.len() as u64;
    let min = chosen.iter().map(|d| d.size).min().unwrap_or(0);
    p.raw_bytes = chosen.iter().map(|d| d.size).sum();
    p.wasted_bytes = chosen.iter().map(|d| d.size - min).sum();
    let member = usable_partition(min).saturating_sub(MD_OVERHEAD);
    let (factor, tolerance) = if count_ok(req.level, chosen.len()).is_ok() {
        capacity_factor(req.level, n)
    } else {
        (0, 0)
    };
    p.usable_bytes = (factor * member).saturating_sub(if req.encrypt { LUKS_HEADER } else { 0 });
    p.fault_tolerance = tolerance;

    // warnings about the mix
    if p.wasted_bytes > 0 && n > 1 {
        p.warnings.push(format!(
            "disks of different sizes: every member is used as large as the smallest, {:.1} GB unused",
            p.wasted_bytes as f64 / 1e9
        ));
    }
    let kinds: std::collections::HashSet<DiskKind> = chosen.iter().map(|d| d.kind).collect();
    if kinds.len() > 1 {
        p.warnings.push(
            "mixed disk types (HDD/SSD/NVMe): the array runs at the speed of the slowest".into(),
        );
    }
    let smr: Vec<&str> = chosen
        .iter()
        .filter(|d| d.smr)
        .map(|d| d.name.as_str())
        .collect();
    if !smr.is_empty() {
        let raid = !matches!(req.level, RaidLevel::Single | RaidLevel::Raid0);
        p.warnings.push(format!(
            "SMR (shingled) disks: {}{}",
            smr.join(", "),
            if raid {
                ": rebuilds can take days or time out; prefer CMR disks for RAID"
            } else {
                ": slow sustained writes"
            }
        ));
    }
    match req.level {
        RaidLevel::Single => p.warnings.push(
            "no redundancy: one disk failure loses the volume (a mirror disk can be added later)"
                .into(),
        ),
        RaidLevel::Raid0 => p.warnings.push(
            "RAID0 has no redundancy: any disk failure loses the whole volume".into(),
        ),
        RaidLevel::Raid5
            if chosen.iter().all(|d| d.kind == DiskKind::Hdd) && (n >= 6 || min >= 10_000_000_000_000) =>
        {
            p.warnings.push(
                "RAID5 with large or many HDDs: a second failure during the long rebuild loses the volume; consider RAID6"
                    .into(),
            )
        }
        _ => {}
    }
    if req.encrypt {
        p.warnings.push(
            "encrypted: the volume stays locked after every boot until unlocked from rynas; keep the recovery key and the header backup off the NAS"
                .into(),
        );
    }
    if !req.compress {
        p.warnings
            .push("compression off: zstd:1 costs little CPU and usually saves space".into());
    }
    if !p.errors.is_empty() {
        return finish(p);
    }

    // steps
    let md = format!("/dev/md/{label}");
    let mapper_name = format!("rynas-{label}");
    let mapper = format!("/dev/mapper/{mapper_name}");
    let mnt = format!("/{label}");
    let fs_dev = if req.encrypt {
        mapper.clone()
    } else {
        md.clone()
    };
    let mut steps = Vec::new();
    for d in &chosen {
        partition_steps(&mut steps, &d.name, label);
    }
    steps.push(cmd(
        "Wait for udev to create the partition nodes",
        &["udevadm", "settle"],
    ));
    let parts: Vec<String> = chosen
        .iter()
        .map(|d| format!("/dev/{}", part_name(&d.name, 1)))
        .collect();
    let mut argv: Vec<String> = vec![
        "mdadm".into(),
        "--create".into(),
        md.clone(),
        "--run".into(),
        format!("--level={}", md_level(req.level)),
        format!("--raid-devices={}", n),
        "--metadata=1.2".into(),
        format!("--name={label}"),
    ];
    if req.level != RaidLevel::Raid0 {
        argv.push("--bitmap=internal".into());
    }
    if req.level == RaidLevel::Single {
        argv.push("--force".into()); // a one-disk RAID1 needs --force
    }
    argv.extend(parts.iter().cloned());
    let argv_ref: Vec<&str> = argv.iter().map(String::as_str).collect();
    let part_names: Vec<&str> = parts
        .iter()
        .map(|p| p.trim_start_matches("/dev/"))
        .collect();
    steps.push(destroying(
        cmd(
            &format!(
                "Create the md array ({}, {n} disk{})",
                level_name(req.level),
                if n == 1 { "" } else { "s" }
            ),
            &argv_ref,
        ),
        &part_names,
    ));
    steps.push(config(
        "Record the array so it is assembled at boot",
        "/etc/mdadm/mdadm.conf",
        &format!("ARRAY {md} metadata=1.2 name=<host>:{label} UUID=<from mdadm --detail --brief>"),
    ));
    if req.encrypt {
        steps.push(cmd_secret(
            "Format the array as LUKS2 (Argon2id, 256 MiB memory, 4 KiB sectors)",
            &[
                "cryptsetup",
                "luksFormat",
                "--batch-mode",
                "--type",
                "luks2",
                "--sector-size",
                "4096",
                "--pbkdf",
                "argon2id",
                "--pbkdf-memory",
                PBKDF_MEMORY_KIB,
                "--label",
                &mapper_name,
                "--key-file",
                "-",
                &md,
            ],
            "passphrase",
        ));
        steps.push(cmd_secret(
            "Add a recovery key (shown once: it must be saved before going on)",
            &[
                "systemd-cryptenroll",
                "--recovery-key",
                "--unlock-key-file=/dev/stdin",
                &md,
            ],
            "passphrase",
        ));
        steps.push(cmd(
            "Back up the LUKS header (offered for download, then removed from the NAS)",
            &[
                "cryptsetup",
                "luksHeaderBackup",
                &md,
                "--header-backup-file",
                &format!("/run/rynas/luks-header-{label}.img"),
            ],
        ));
        steps.push(cmd_secret(
            "Unlock the new volume",
            &["cryptsetup", "open", "--key-file", "-", &md, &mapper_name],
            "passphrase",
        ));
        steps.push(config(
            "Declare the encrypted volume, locked at boot (noauto)",
            "/etc/crypttab",
            &format!("{mapper_name} UUID=<luks uuid> none luks,noauto"),
        ));
        if chosen.iter().any(|d| d.kind != DiskKind::Hdd) {
            p.warnings.push(
                "SSD under LUKS: TRIM is off by default because it reveals which blocks are free; enable 'discard' in crypttab if that is acceptable"
                    .into(),
            );
        }
    }
    steps.push(cmd(
        "Create the btrfs filesystem (data single, metadata DUP)",
        &[
            "mkfs.btrfs",
            "--label",
            label,
            "--data",
            "single",
            "--metadata",
            "dup",
            &fs_dev,
        ],
    ));
    let mut opts = "noatime".to_string();
    if req.compress {
        opts.push_str(&format!(",compress={COMPRESSION}"));
    }
    // fstab first: systemd mounts the volume from it (a mount made inside rynasd's own
    // mount namespace would stay invisible to the rest of the system)
    steps.push(config(
        "Mount the volume at boot",
        "/etc/fstab",
        &format!(
            "UUID=<btrfs uuid> {mnt} btrfs {opts},nofail{} 0 0",
            if req.encrypt { ",noauto" } else { "" }
        ),
    ));
    steps.push(cmd(
        &format!("Create the mount point {mnt}"),
        &["mkdir", "-p", &mnt],
    ));
    steps.push(cmd(
        &format!("Mount the volume at {mnt} (systemd mount unit from /etc/fstab)"),
        &["systemctl", "start", &mount_unit(&mnt)],
    ));
    steps.push(cmd(
        "Create the snapshot subvolume (btrbk snapshot_dir, hidden from shares)",
        &["btrfs", "subvolume", "create", &format!("{mnt}/.snapshots")],
    ));
    steps.push(cmd(
        "Create the cache subvolume (dedup hashfiles, excluded from snapshots)",
        &[
            "btrfs",
            "subvolume",
            "create",
            &format!("{mnt}/@rynas-cache"),
        ],
    ));
    steps.push(cmd(
        "Disable copy-on-write on the cache subvolume (SQLite files)",
        &["chattr", "+C", &format!("{mnt}/@rynas-cache")],
    ));
    p.summary = vec![
        ("level".into(), level_name(req.level).into()),
        ("md".into(), md),
        ("filesystem_device".into(), fs_dev),
        ("mountpoint".into(), mnt),
    ];
    if req.encrypt {
        p.summary.push(("mapper".into(), mapper_name));
    }
    p.steps = steps;
    finish(p)
}

fn find_array<'a>(inv: &'a Inventory, name: &str) -> Option<&'a MdArray> {
    let n = name.trim_start_matches("/dev/");
    inv.arrays.iter().find(|a| {
        a.name == n
            || a.md_name.as_deref().is_some_and(|m| {
                m == n.trim_start_matches("md/")
                    || m.ends_with(&format!(":{}", n.trim_start_matches("md/")))
            })
    })
}

/// What sits on top of an array: (LUKS mapper name, btrfs mountpoint).
fn stack_above(inv: &Inventory, md: &str) -> (Option<String>, Option<String>) {
    let mapper = inv
        .graph
        .above(md)
        .into_iter()
        .find(|d| d.kind == "crypt")
        .map(|d| d.name.clone());
    let mnt = inv
        .graph
        .above(md)
        .into_iter()
        .chain(inv.graph.get(md))
        .find_map(|d| {
            (d.fstype.as_deref() == Some("btrfs"))
                .then(|| d.uuid.clone())
                .flatten()
        })
        .and_then(|uuid| inv.filesystems.iter().find(|f| f.uuid == uuid))
        .and_then(|f| f.mountpoint.clone());
    (mapper, mnt)
}

fn array_busy(a: &MdArray, p: &mut StoragePlan) {
    if let Some(s) = &a.sync {
        let m = format!(
            "{} is busy ({}{}); wait for it to finish",
            a.name,
            s.action,
            s.pct.map(|x| format!(" {x:.1}%")).unwrap_or_default()
        );
        fail(p, PlanIssueCode::ArrayBusy, "array", Some(&a.name), m);
    }
}

/// Size of the md component partition of an existing member.
fn member_part_size(inv: &Inventory, a: &MdArray) -> u64 {
    a.members
        .iter()
        .filter(|m| m.role == MemberRole::Active)
        .filter_map(|m| inv.graph.get(&m.device))
        .map(|d| d.size)
        .max()
        .unwrap_or(0)
}

/// `POST /api/v1/storage/plan/grow`
pub fn plan_grow(inv: &Inventory, req: &GrowPlanRequest) -> StoragePlan {
    let mut p = StoragePlan {
        operation: "grow_array".into(),
        ..Default::default()
    };
    let Some(a) = find_array(inv, &req.array) else {
        let m = format!("{}: no such array", req.array);
        fail(
            &mut p,
            PlanIssueCode::NoSuchArray,
            "array",
            Some(&req.array),
            m,
        );
        return finish(p);
    };
    if a.health != ArrayHealth::Healthy {
        let m = format!(
            "{} is not healthy ({}): repair it before growing",
            a.name, a.state
        );
        fail(
            &mut p,
            PlanIssueCode::ArrayNotHealthy,
            "array",
            Some(&a.name),
            m,
        );
    }
    array_busy(a, &mut p);
    if req.disks.is_empty() {
        let m = "select at least one disk to add".into();
        fail(&mut p, PlanIssueCode::NoDisks, "disks", None, m);
    }
    let Some(from) = level_from_md(&a.level, a.raid_disks) else {
        let m = format!("{}: level {} cannot be grown here", a.name, a.level);
        fail(
            &mut p,
            PlanIssueCode::LevelUnsupported,
            "array",
            Some(&a.name),
            m,
        );
        return finish(p);
    };
    let to = req.level.unwrap_or(match from {
        RaidLevel::Single => RaidLevel::Raid1,
        l => l,
    });
    let old_n = a.raid_disks as u64;
    let new_n = old_n + req.disks.len() as u64;
    let supported =
        matches!(
            (from, to),
            (RaidLevel::Single | RaidLevel::Raid1, RaidLevel::Raid1)
                | (RaidLevel::Raid5, RaidLevel::Raid5)
                | (RaidLevel::Raid6, RaidLevel::Raid6)
                | (RaidLevel::Raid5, RaidLevel::Raid6)
        ) || (matches!(from, RaidLevel::Raid1) && a.raid_disks == 2 && to == RaidLevel::Raid5);
    if !supported {
        let m = format!(
            "{} → {} is not supported (supported: add mirrors, expand RAID5/6, RAID1 (2 disks) → RAID5, RAID5 → RAID6)",
            level_name(from),
            level_name(to)
        );
        fail(
            &mut p,
            PlanIssueCode::LevelChangeUnsupported,
            "level",
            None,
            m,
        );
    }
    if count_ok(to, new_n as usize).is_err() {
        let m = format!("{} with {new_n} disks is not possible", level_name(to));
        fail(&mut p, PlanIssueCode::DiskCount, "disks", None, m);
    }
    let part_size = member_part_size(inv, a);
    let label = a
        .md_name
        .as_deref()
        .map(|n| n.rsplit(':').next().unwrap_or(n).to_string())
        .unwrap_or_else(|| a.name.clone());
    let mut new: Vec<DiskSummary> = Vec::new();
    for name in &req.disks {
        if let Some(d) = check_new_disk(inv, name, "disks", &mut p) {
            if usable_partition(d.size) < part_size {
                let m = format!(
                    "{name}: too small ({:.1} GB, members use {:.1} GB)",
                    d.size as f64 / 1e9,
                    part_size as f64 / 1e9
                );
                fail(&mut p, PlanIssueCode::DiskTooSmall, "disks", Some(name), m);
            } else if usable_partition(d.size) > part_size + 10 * END_RESERVE {
                p.warnings.push(format!(
                    "{name} is larger than the members: the extra space stays unused until every member is replaced"
                ));
            }
            new.push(d);
        }
    }
    let member = part_size.saturating_sub(MD_OVERHEAD);
    let (_, tol) = capacity_factor(to, new_n.max(1));
    let (factor, _) = if count_ok(to, new_n as usize).is_ok() {
        capacity_factor(to, new_n)
    } else {
        (0, 0)
    };
    p.usable_bytes = factor * member;
    p.fault_tolerance = tol;
    p.raw_bytes = new.iter().map(|d| d.size).sum();
    if !p.errors.is_empty() {
        return finish(p);
    }

    let md = a.path.clone();
    let mut steps = Vec::new();
    for d in &new {
        partition_steps(&mut steps, &d.name, &label);
    }
    steps.push(cmd(
        "Wait for udev to create the partition nodes",
        &["udevadm", "settle"],
    ));
    let mut add: Vec<String> = vec!["mdadm".into(), md.clone(), "--add".into()];
    add.extend(
        new.iter()
            .map(|d| format!("/dev/{}", part_name(&d.name, 1))),
    );
    let backup = format!("--backup-file=/var/lib/rynas/md-reshape-{}.bak", a.name);
    let raid_devices = format!("--raid-devices={new_n}");
    if from == RaidLevel::Raid1 && to == RaidLevel::Raid5 {
        steps.push(cmd(
            "Convert the 2-disk mirror to RAID5 (instant, same data layout)",
            &["mdadm", "--grow", &md, "--level=5"],
        ));
    }
    steps.push(cmd(
        "Add the new partitions as spares",
        &add.iter().map(String::as_str).collect::<Vec<_>>(),
    ));
    let mut grow = vec!["mdadm", "--grow", &md, &raid_devices];
    let level6 = "--level=6";
    if from == RaidLevel::Raid5 && to == RaidLevel::Raid6 {
        grow.insert(3, level6);
        grow.push(&backup);
    }
    let reshape = to != RaidLevel::Raid1;
    steps.push(cmd(
        if reshape {
            "Reshape the array onto the new disks (hours to days; the volume stays online)"
        } else {
            "Turn the spares into active mirrors (resync)"
        },
        &grow,
    ));
    steps.push(wait(
        if reshape {
            "Wait for the reshape to finish"
        } else {
            "Wait for the resync to finish"
        },
        &format!("/sys/block/{}/md/sync_action == idle", a.name),
    ));
    if reshape {
        let (mapper, mnt) = stack_above(inv, &a.name);
        if let Some(m) = &mapper {
            steps.push(cmd_secret(
                "Grow the LUKS mapping to the new array size",
                &["cryptsetup", "resize", "--key-file", "-", m],
                "passphrase",
            ));
        }
        match &mnt {
            Some(m) => steps.push(cmd(
                "Grow the filesystem to the new size",
                &["btrfs", "filesystem", "resize", "max", m],
            )),
            None => p
                .warnings
                .push("no mounted btrfs on this array: grow the filesystem by hand".into()),
        }
    }
    p.summary = vec![
        ("array".into(), md),
        ("from".into(), level_name(from).into()),
        ("to".into(), level_name(to).into()),
        ("raid_devices".into(), new_n.to_string()),
    ];
    p.steps = steps;
    finish(p)
}

/// `POST /api/v1/storage/plan/replace`
pub fn plan_replace(inv: &Inventory, req: &ReplacePlanRequest) -> StoragePlan {
    let mut p = StoragePlan {
        operation: "replace_disk".into(),
        ..Default::default()
    };
    let Some(a) = find_array(inv, &req.array) else {
        let m = format!("{}: no such array", req.array);
        fail(
            &mut p,
            PlanIssueCode::NoSuchArray,
            "array",
            Some(&req.array),
            m,
        );
        return finish(p);
    };
    if a.level == "raid0" || a.level == "linear" {
        let m = format!(
            "{}: {} has no redundancy, a member cannot be replaced",
            a.name, a.level
        );
        fail(
            &mut p,
            PlanIssueCode::NoRedundancy,
            "array",
            Some(&a.name),
            m,
        );
    }
    let old = req.old.trim_start_matches("/dev/");
    let member = a
        .members
        .iter()
        .find(|m| m.device == old || m.disk.as_deref() == Some(old));
    let Some(member) = member else {
        let m = format!("{old} is not a member of {}", a.name);
        fail(&mut p, PlanIssueCode::NotAMember, "old", Some(old), m);
        return finish(p);
    };
    if member.role == MemberRole::Spare {
        p.warnings.push(format!(
            "{} is a spare: it can simply be removed",
            member.device
        ));
    }
    // a healthy member to copy the partition table from
    let source = a
        .members
        .iter()
        .filter(|m| m.role == MemberRole::Active && m.device != member.device)
        .find_map(|m| m.disk.clone());
    let Some(source) = source else {
        let m = format!("{}: no healthy member left to rebuild from", a.name);
        fail(
            &mut p,
            PlanIssueCode::NoHealthyMember,
            "array",
            Some(&a.name),
            m,
        );
        return finish(p);
    };
    let src_size = inv
        .disks
        .iter()
        .find(|d| d.name == source)
        .map(|d| d.size)
        .unwrap_or(0);
    let new = check_new_disk(inv, &req.new_disk, "new_disk", &mut p);
    if let Some(d) = &new {
        if d.size < src_size {
            let m = format!(
                "{}: smaller than the other members ({:.1} GB < {:.1} GB)",
                d.name,
                d.size as f64 / 1e9,
                src_size as f64 / 1e9
            );
            fail(
                &mut p,
                PlanIssueCode::DiskTooSmall,
                "new_disk",
                Some(&d.name),
                m,
            );
        }
        if d.smr && !matches!(a.level.as_str(), "raid1") {
            p.warnings.push(format!(
                "{}: SMR disk, the rebuild may be very slow",
                d.name
            ));
        }
    }
    if let Some(s) = &a.sync
        && s.action != "recovery"
    {
        let m = format!("{} is busy ({}); wait for it to finish", a.name, s.action);
        fail(&mut p, PlanIssueCode::ArrayBusy, "array", Some(&a.name), m);
    }
    // other partitions on the source disk (EFI, swap) are not copied by md
    let src_parts = inv
        .graph
        .get(&source)
        .map(|d| d.children.len())
        .unwrap_or(0);
    if src_parts > 1 {
        p.warnings.push(format!(
            "{source} has {src_parts} partitions: only the md members are rebuilt, copy the others (EFI, swap) separately"
        ));
    }
    let tol = crate::md::tolerance(&a.level, a.raid_disks);
    p.fault_tolerance = tol;
    p.usable_bytes = a.size;
    if !p.errors.is_empty() {
        return finish(p);
    }
    let new = new.expect("checked above");
    let new_dev = format!("/dev/{}", new.name);
    // the component index of the member: keep the same partition number
    let part_no = member
        .device
        .strip_prefix(member.disk.as_deref().unwrap_or(""))
        .map(|s| s.trim_start_matches('p'))
        .and_then(|n| n.parse::<u32>().ok())
        .unwrap_or(1);
    let new_part = format!("/dev/{}", part_name(&new.name, part_no));
    let old_dev = format!("/dev/{}", member.device);
    let md = a.path.clone();
    let mut steps = vec![
        destroying(
            cmd(
                &format!("Erase signatures on {}", new.name),
                &["wipefs", "--all", &new_dev],
            ),
            &[&new.name],
        ),
        destroying(
            cmd(
                &format!("Copy the partition table of {source} to {}", new.name),
                &[
                    "sgdisk",
                    &format!("--replicate={new_dev}"),
                    &format!("/dev/{source}"),
                ],
            ),
            &[&new.name],
        ),
        cmd(
            &format!("Give {} its own partition GUIDs", new.name),
            &["sgdisk", "--randomize-guids", &new_dev],
        ),
        cmd(
            "Wait for udev to create the partition nodes",
            &["udevadm", "settle"],
        ),
    ];
    let hot = member.role == MemberRole::Active && a.health == ArrayHealth::Healthy;
    if hot {
        // keeps full redundancy during the copy
        steps.push(cmd(
            "Add the new partition as a spare",
            &["mdadm", &md, "--add-spare", &new_part],
        ));
        steps.push(cmd(
            &format!(
                "Copy {} onto the new disk while it stays in the array",
                member.device
            ),
            &["mdadm", &md, "--replace", &old_dev, "--with", &new_part],
        ));
        steps.push(wait(
            "Wait for the copy to finish",
            &format!("/sys/block/{}/md/sync_action == idle", a.name),
        ));
        steps.push(cmd(
            &format!("Remove {} (marked faulty by the replace)", member.device),
            &["mdadm", &md, "--remove", &old_dev],
        ));
    } else {
        if member.role != MemberRole::Faulty {
            steps.push(cmd(
                &format!("Mark {} as failed", member.device),
                &["mdadm", &md, "--fail", &old_dev],
            ));
        }
        steps.push(cmd(
            &format!("Remove {} from the array", member.device),
            &["mdadm", &md, "--remove", &old_dev],
        ));
        steps.push(cmd(
            "Add the new partition: the rebuild starts",
            &["mdadm", &md, "--add", &new_part],
        ));
        steps.push(wait(
            "Wait for the rebuild to finish",
            &format!("/sys/block/{}/md/sync_action == idle", a.name),
        ));
    }
    p.summary = vec![
        ("array".into(), md),
        ("old".into(), member.device.clone()),
        (
            "new".into(),
            new_part.trim_start_matches("/dev/").to_string(),
        ),
        (
            "mode".into(),
            if hot { "hot replace" } else { "rebuild" }.into(),
        ),
    ];
    p.steps = steps;
    finish(p)
}

/// `POST /api/v1/storage/plan/scrub`
pub fn plan_scrub(inv: &Inventory, req: &ScrubPlanRequest) -> StoragePlan {
    let mut p = StoragePlan {
        operation: "scrub".into(),
        ..Default::default()
    };
    let key = req.filesystem.as_str();
    let Some(fs) = inv.filesystems.iter().find(|f| {
        f.uuid == key
            || f.mountpoint.as_deref() == Some(key)
            || f.mountpoints.iter().any(|m| m == key)
    }) else {
        let m = format!("{key}: no such btrfs filesystem");
        fail(
            &mut p,
            PlanIssueCode::NoSuchFilesystem,
            "filesystem",
            Some(key),
            m,
        );
        return finish(p);
    };
    let Some(mnt) = fs.mountpoint.clone() else {
        let m = format!("{key}: not mounted, cannot scrub");
        fail(
            &mut p,
            PlanIssueCode::NotMounted,
            "filesystem",
            Some(key),
            m,
        );
        return finish(p);
    };
    if fs
        .scrub
        .as_ref()
        .is_some_and(|s| s.status == rynas_api_types::storage::ScrubState::Running)
    {
        let m = "a scrub is already running on this filesystem".into();
        fail(
            &mut p,
            PlanIssueCode::ScrubRunning,
            "filesystem",
            Some(key),
            m,
        );
    }
    // md arrays under the filesystem devices
    let mut arrays: Vec<&MdArray> = Vec::new();
    for dev in &fs.devices {
        if let Some(d) = inv.graph.find(&dev.path) {
            let mut stack = vec![d.kname.clone()];
            while let Some(k) = stack.pop() {
                if let Some(a) = inv.arrays.iter().find(|a| a.name == k)
                    && !arrays.iter().any(|x| x.name == a.name)
                {
                    arrays.push(a);
                }
                if let Some(x) = inv.graph.get(&k) {
                    stack.extend(x.parents.iter().cloned());
                }
            }
        }
    }
    for a in &arrays {
        if a.health != ArrayHealth::Healthy {
            p.warnings.push(format!(
                "{} is not healthy: a scrub can find but not repair errors without redundancy",
                a.name
            ));
        }
        if req.md_check {
            array_busy(a, &mut p);
        }
    }
    p.usable_bytes = fs.device_size;
    p.warnings.push(
        "scrub reads every allocated block: it takes hours on large HDD volumes and wakes sleeping disks; it never runs together with replication, balance or dedup on the same filesystem"
            .into(),
    );
    if !p.errors.is_empty() {
        return finish(p);
    }
    let mut steps = vec![cmd(
        "Verify every checksum and repair from a good copy (metadata DUP, md mirrors)",
        &["btrfs", "scrub", "start", "-B", "-d", &mnt],
    )];
    if req.md_check {
        for a in &arrays {
            if a.level == "raid0" || a.level == "linear" {
                continue;
            }
            steps.push(sysfs(
                &format!(
                    "Check the parity/mirrors of {} (read-only, counts mismatches)",
                    a.name
                ),
                &format!("/sys/block/{}/md/sync_action", a.name),
                "check",
            ));
            steps.push(wait(
                &format!("Wait for the {} check, then read mismatch_cnt", a.name),
                &format!("/sys/block/{}/md/sync_action == idle", a.name),
            ));
        }
    }
    p.summary = vec![
        ("mountpoint".into(), mnt),
        ("filesystem".into(), fs.uuid.clone()),
    ];
    p.steps = steps;
    finish(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::nas_inventory;

    fn argvs(p: &StoragePlan) -> Vec<String> {
        p.steps
            .iter()
            .filter_map(|s| match &s.action {
                PlanAction::Command { argv, .. } => Some(argv.join(" ")),
                PlanAction::WriteSysfs { path, value } => Some(format!("echo {value} > {path}")),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn labels() {
        assert!(valid_label("vol2"));
        assert!(valid_label("media-01"));
        assert!(!valid_label("Vol2"));
        assert!(!valid_label("2vol"));
        assert!(!valid_label("home"));
        assert!(!valid_label("a/b"));
        assert!(!valid_label(""));
        assert_eq!(part_name("sdb", 1), "sdb1");
        assert_eq!(part_name("nvme0n1", 1), "nvme0n1p1");
        assert_eq!(mount_unit("/vol1"), "vol1.mount");
        assert_eq!(mount_unit("/media-01"), "media\\x2d01.mount");
        assert_eq!(mount_unit("/srv/a b"), "srv-a\\x20b.mount");
    }

    #[test]
    fn capacity() {
        assert_eq!(capacity_factor(RaidLevel::Raid5, 4), (3, 1));
        assert_eq!(capacity_factor(RaidLevel::Raid6, 6), (4, 2));
        assert_eq!(capacity_factor(RaidLevel::Raid10, 4), (2, 1));
        assert_eq!(capacity_factor(RaidLevel::Raid1, 3), (1, 2));
    }

    #[test]
    fn single_disk_encrypted() {
        let inv = nas_inventory();
        let p = plan_volume(
            &inv,
            &VolumePlanRequest {
                disks: vec!["sdd".into()],
                level: RaidLevel::Single,
                encrypt: true,
                compress: true,
                label: "vol2".into(),
            },
        );
        assert!(p.valid, "{:?}", p.errors);
        assert_eq!(p.fault_tolerance, 0);
        let a = argvs(&p);
        assert_eq!(
            a,
            [
                "wipefs --all /dev/sdd",
                "sgdisk --zap-all /dev/sdd",
                "sgdisk --new=1:0:-128M --typecode=1:FD00 --change-name=1:rynas-vol2 /dev/sdd",
                "udevadm settle",
                "mdadm --create /dev/md/vol2 --run --level=1 --raid-devices=1 --metadata=1.2 --name=vol2 --bitmap=internal --force /dev/sdd1",
                "cryptsetup luksFormat --batch-mode --type luks2 --sector-size 4096 --pbkdf argon2id --pbkdf-memory 262144 --label rynas-vol2 --key-file - /dev/md/vol2",
                "systemd-cryptenroll --recovery-key --unlock-key-file=/dev/stdin /dev/md/vol2",
                "cryptsetup luksHeaderBackup /dev/md/vol2 --header-backup-file /run/rynas/luks-header-vol2.img",
                "cryptsetup open --key-file - /dev/md/vol2 rynas-vol2",
                "mkfs.btrfs --label vol2 --data single --metadata dup /dev/mapper/rynas-vol2",
                "mkdir -p /vol2",
                "systemctl start vol2.mount",
                "btrfs subvolume create /vol2/.snapshots",
                "btrfs subvolume create /vol2/@rynas-cache",
                "chattr +C /vol2/@rynas-cache",
            ]
        );
        // secrets travel on stdin, never in argv
        let secret_steps = p
            .steps
            .iter()
            .filter(|s| matches!(&s.action, PlanAction::Command { stdin: Some(x), .. } if x == "passphrase"))
            .count();
        assert_eq!(secret_steps, 3);
        assert!(p.warnings.iter().any(|w| w.contains("SMR")));
        assert!(p.warnings.iter().any(|w| w.contains("no redundancy")));
        let usable = p.usable_bytes as f64;
        assert!(usable > 3.99e12 && usable < 4.0008e12, "{usable}");
        // config entries for fstab / crypttab / mdadm.conf
        let files: Vec<_> = p
            .steps
            .iter()
            .filter_map(|s| match &s.action {
                PlanAction::Config { file, entry } => Some((file.as_str(), entry.as_str())),
                _ => None,
            })
            .collect();
        assert!(
            files
                .iter()
                .any(|(f, e)| *f == "/etc/crypttab" && e.contains("noauto"))
        );
        assert!(
            files
                .iter()
                .any(|(f, e)| *f == "/etc/fstab" && e.contains("noauto"))
        );
    }

    #[test]
    fn refuses_disks_in_use() {
        let inv = nas_inventory();
        let p = plan_volume(
            &inv,
            &VolumePlanRequest {
                disks: vec![
                    "sda".into(),
                    "sdd".into(),
                    "nvme0n1".into(),
                    "sdz".into(),
                    "sdd".into(),
                ],
                level: RaidLevel::Raid5,
                encrypt: false,
                compress: true,
                label: "vol1".into(),
            },
        );
        assert!(!p.valid);
        assert!(p.steps.is_empty());
        let e = p.errors.join("\n");
        assert!(e.contains("sda: in use"), "{e}");
        assert!(e.contains("nvme0n1: holds the operating system"), "{e}");
        assert!(e.contains("sdz: no such disk"), "{e}");
        assert!(e.contains("sdd: selected twice"), "{e}");
        assert!(e.contains("vol1 already exists"), "{e}");
        // one issue per error, with its code, the request field and the disk
        assert_eq!(p.issues.len(), p.errors.len());
        let has = |code: PlanIssueCode, field: &str, subject: Option<&str>| {
            p.issues.iter().any(|i| {
                i.code == code
                    && i.field.as_deref() == Some(field)
                    && i.subject.as_deref() == subject
            })
        };
        assert!(
            has(PlanIssueCode::LabelTaken, "label", None),
            "{:?}",
            p.issues
        );
        assert!(has(PlanIssueCode::DiskInUse, "disks", Some("sda")));
        assert!(has(PlanIssueCode::SystemDisk, "disks", Some("nvme0n1")));
        assert!(has(PlanIssueCode::NoSuchDisk, "disks", Some("sdz")));
        assert!(has(PlanIssueCode::DiskSelectedTwice, "disks", Some("sdd")));
    }

    #[test]
    fn raid1_mixed_sizes_and_locked_luks() {
        let inv = nas_inventory();
        let p = plan_volume(
            &inv,
            &VolumePlanRequest {
                disks: vec!["sdd".into(), "sde".into()],
                level: RaidLevel::Raid1,
                encrypt: false,
                compress: false,
                label: "backup".into(),
            },
        );
        assert!(p.valid, "{:?}", p.errors);
        assert_eq!(p.fault_tolerance, 1);
        assert_eq!(p.wasted_bytes, 4_000_787_030_016 - 2_000_398_934_016);
        let w = p.warnings.join("\n");
        assert!(w.contains("different sizes"), "{w}");
        assert!(w.contains("sde: existing data will be erased"), "{w}");
        assert!(w.contains("compression off"), "{w}");
        let a = argvs(&p);
        assert!(a.iter().any(|l| l == "systemctl start backup.mount"));
        let fstab = p
            .steps
            .iter()
            .find_map(|s| match &s.action {
                PlanAction::Config { file, entry } if file == "/etc/fstab" => Some(entry.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(fstab, "UUID=<btrfs uuid> /backup btrfs noatime,nofail 0 0");
        assert!(
            a.iter()
                .any(|l| l.ends_with("--bitmap=internal /dev/sdd1 /dev/sde1"))
        );
    }

    #[test]
    fn level_counts() {
        let inv = nas_inventory();
        for (level, disks, msg) in [
            (RaidLevel::Raid5, vec!["sdd", "sde"], "at least 3"),
            (RaidLevel::Raid10, vec!["sdd", "sde"], "at least 4"),
            (RaidLevel::Single, vec!["sdd", "sde"], "exactly one"),
        ] {
            let p = plan_volume(
                &inv,
                &VolumePlanRequest {
                    disks: disks.into_iter().map(String::from).collect(),
                    level,
                    encrypt: false,
                    compress: true,
                    label: "x".into(),
                },
            );
            assert!(p.errors.iter().any(|e| e.contains(msg)), "{:?}", p.errors);
        }
    }

    #[test]
    fn grow_refuses_busy_array_and_plans_raid5_expand() {
        let mut inv = nas_inventory();
        // fixture md2 is recovering
        let p = plan_grow(
            &inv,
            &GrowPlanRequest {
                array: "md2".into(),
                disks: vec!["sdd".into()],
                level: None,
            },
        );
        assert!(!p.valid);
        // make it healthy and grow with a large enough disk
        let a = inv.arrays.iter_mut().find(|a| a.name == "md2").unwrap();
        a.sync = None;
        a.health = ArrayHealth::Healthy;
        a.degraded = 0;
        for m in &mut a.members {
            m.role = MemberRole::Active;
        }
        let big = inv.disks.iter().find(|d| d.name == "sda").unwrap().clone();
        let mut new = big.clone();
        new.name = "sdf".into();
        new.path = "/dev/sdf".into();
        inv.disks.push(new);
        let p = plan_grow(
            &inv,
            &GrowPlanRequest {
                array: "md2".into(),
                disks: vec!["sdf".into()],
                level: None,
            },
        );
        assert!(p.valid, "{:?}", p.errors);
        let a = argvs(&p);
        assert!(
            a.contains(&"mdadm /dev/md2 --add /dev/sdf1".to_string()),
            "{a:?}"
        );
        assert!(
            a.contains(&"mdadm --grow /dev/md2 --raid-devices=4".to_string()),
            "{a:?}"
        );
        assert!(
            a.contains(&"cryptsetup resize --key-file - rynas-vol1".to_string()),
            "{a:?}"
        );
        assert!(
            a.contains(&"btrfs filesystem resize max /vol1".to_string()),
            "{a:?}"
        );
        // too small disk
        let p = plan_grow(
            &inv,
            &GrowPlanRequest {
                array: "md2".into(),
                disks: vec!["sdd".into()],
                level: None,
            },
        );
        assert!(
            p.errors.iter().any(|e| e.contains("too small")),
            "{:?}",
            p.errors
        );
        // raid5 → raid6 with one more disk: level change with a backup file
        let p = plan_grow(
            &inv,
            &GrowPlanRequest {
                array: "md2".into(),
                disks: vec!["sdf".into()],
                level: Some(RaidLevel::Raid6),
            },
        );
        assert!(p.valid, "{:?}", p.errors);
        assert_eq!(p.fault_tolerance, 2);
        assert!(argvs(&p).contains(&"mdadm --grow /dev/md2 --level=6 --raid-devices=4 --backup-file=/var/lib/rynas/md-reshape-md2.bak".to_string()));
        // raid10 cannot be grown here
        let p = plan_grow(
            &inv,
            &GrowPlanRequest {
                array: "md2".into(),
                disks: vec!["sdf".into()],
                level: Some(RaidLevel::Raid10),
            },
        );
        assert!(!p.valid);
    }

    #[test]
    fn replace_rebuild_and_hot() {
        let mut inv = nas_inventory();
        let mut new = inv.disks.iter().find(|d| d.name == "sda").unwrap().clone();
        new.name = "sdf".into();
        inv.disks.push(new);
        // md2: sdc1 is rebuilding, so the array is degraded → rebuild mode for sdb
        let p = plan_replace(
            &inv,
            &ReplacePlanRequest {
                array: "md2".into(),
                old: "sdc".into(),
                new_disk: "sdf".into(),
            },
        );
        assert!(p.valid, "{:?}", p.errors);
        let a = argvs(&p);
        assert_eq!(a[1], "sgdisk --replicate=/dev/sdf /dev/sda");
        assert!(
            a.contains(&"mdadm /dev/md2 --fail /dev/sdc1".to_string()),
            "{a:?}"
        );
        assert!(
            a.contains(&"mdadm /dev/md2 --add /dev/sdf1".to_string()),
            "{a:?}"
        );

        // healthy array: hot replace keeps redundancy
        let arr = inv.arrays.iter_mut().find(|a| a.name == "md2").unwrap();
        arr.sync = None;
        arr.health = ArrayHealth::Healthy;
        for m in &mut arr.members {
            m.role = MemberRole::Active;
        }
        let p = plan_replace(
            &inv,
            &ReplacePlanRequest {
                array: "md2".into(),
                old: "sdb1".into(),
                new_disk: "sdf".into(),
            },
        );
        let a = argvs(&p);
        assert!(
            a.contains(&"mdadm /dev/md2 --replace /dev/sdb1 --with /dev/sdf1".to_string()),
            "{a:?}"
        );
        assert_eq!(
            p.summary.iter().find(|(k, _)| k == "mode").unwrap().1,
            "hot replace"
        );

        let p = plan_replace(
            &inv,
            &ReplacePlanRequest {
                array: "md2".into(),
                old: "sdd".into(),
                new_disk: "sdf".into(),
            },
        );
        assert!(p.errors[0].contains("not a member"));
    }

    #[test]
    fn scrub() {
        let mut inv = nas_inventory();
        for a in &mut inv.arrays {
            a.sync = None;
        }
        let p = plan_scrub(
            &inv,
            &ScrubPlanRequest {
                filesystem: "/vol1".into(),
                md_check: true,
            },
        );
        assert!(p.valid, "{:?}", p.errors);
        let a = argvs(&p);
        assert_eq!(a[0], "btrfs scrub start -B -d /vol1");
        assert_eq!(a[1], "echo check > /sys/block/md2/md/sync_action");
        let p = plan_scrub(
            &inv,
            &ScrubPlanRequest {
                filesystem: "nope".into(),
                md_check: false,
            },
        );
        assert!(!p.valid);
    }
}
