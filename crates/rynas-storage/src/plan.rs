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
    PlanAction, PlanArgs, PlanIssue, PlanIssueCode, PlanStep, PlanStepKind, PlanWarning,
    PlanWarningCode, RaidLevel, ReplacePlanRequest, ScrubPlanRequest, StoragePlan,
    VolumePlanRequest,
};

use crate::Inventory;

// disk layout constants and capacity arithmetic, shared with the web interface's calculator
pub use rynas_api_types::capacity::{END_RESERVE, capacity_factor};
pub(crate) use rynas_api_types::capacity::{LUKS_HEADER, MIB, PART_START, usable_partition};
use rynas_api_types::capacity::{
    MD_CHUNK, MIN_DISK, fits, md_array_size_of_components, md_component, md_data_offset,
    negligible_difference, shortfall,
};
/// Argon2id memory cap (KiB) so a 4 GB NAS does not run out of memory while unlocking.
pub const PBKDF_MEMORY_KIB: &str = "262144";
pub const COMPRESSION: &str = "zstd:1";

/// A step's description: what it does and the values it names. The English text is built
/// from them ([`step_english`]), so `description` and `kind`/`args` always agree.
pub(crate) struct D(pub(crate) PlanStepKind, pub(crate) PlanArgs);

/// A description without values.
pub(crate) fn d(kind: PlanStepKind) -> D {
    D(kind, PlanArgs::default())
}

/// A description about one disk, partition, array or mount point.
pub(crate) fn d_on(kind: PlanStepKind, subject: &str) -> D {
    D(kind, subject_args(subject))
}

pub(crate) fn subject_args(subject: &str) -> PlanArgs {
    PlanArgs {
        subject: Some(subject.to_string()),
        ..Default::default()
    }
}

pub(crate) fn step(desc: D, action: PlanAction) -> PlanStep {
    let D(kind, args) = desc;
    PlanStep {
        description: step_english(kind, &args),
        kind: Some(kind),
        args,
        action,
        destroys: Vec::new(),
        best_effort: false,
    }
}

pub(crate) fn cmd(desc: D, argv: &[&str]) -> PlanStep {
    step(
        desc,
        PlanAction::Command {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            stdin: None,
        },
    )
}

pub(crate) fn cmd_secret(desc: D, argv: &[&str], secret: &str) -> PlanStep {
    let mut s = cmd(desc, argv);
    if let PlanAction::Command { stdin, .. } = &mut s.action {
        *stdin = Some(secret.to_string());
    }
    s
}

pub(crate) fn destroying(mut s: PlanStep, devs: &[&str]) -> PlanStep {
    s.destroys = devs.iter().map(|d| d.to_string()).collect();
    s
}

pub(crate) fn config(desc: D, file: &str, entry: &str) -> PlanStep {
    step(
        desc,
        PlanAction::Config {
            file: file.into(),
            entry: entry.into(),
        },
    )
}

pub(crate) fn wait(desc: D, condition: &str) -> PlanStep {
    step(
        desc,
        PlanAction::Wait {
            condition: condition.into(),
        },
    )
}

fn sysfs(desc: D, path: &str, value: &str) -> PlanStep {
    step(
        desc,
        PlanAction::WriteSysfs {
            path: path.into(),
            value: value.into(),
        },
    )
}

/// English description of a step (task log, CLI); the UI translates `kind` + `args`.
pub(crate) fn step_english(kind: PlanStepKind, a: &PlanArgs) -> String {
    use PlanStepKind as K;
    let s = a.subject.as_deref().unwrap_or("");
    let text = match kind {
        K::WipeSignatures => return format!("Erase signatures on {s}"),
        K::ClearPartitionTable => return format!("Clear the partition table of {s}"),
        K::CreatePartition => {
            if let Some(part) = a.other.as_deref() {
                return format!("Create the tier partition {part} on {s} (the disk stays in use)");
            }
            let n = a.count.unwrap_or(1);
            if n > 1 {
                return format!("Create {n} Linux RAID partitions on {s}, one per tier");
            }
            return format!("Create one Linux RAID partition on {s}, 128 MiB left free at the end");
        }
        K::CreateArray => {
            let n = a.count.unwrap_or(0);
            return format!(
                "Create the md array ({}, {n} disk{})",
                a.level.map(level_name).unwrap_or(""),
                if n == 1 { "" } else { "s" }
            );
        }
        K::CreateMountPoint => return format!("Create the mount point {s}"),
        K::MountVolume => {
            return format!("Mount the volume at {s} (systemd mount unit from /etc/fstab)");
        }
        K::CopyPartitionTable => {
            let o = a.other.as_deref().unwrap_or("");
            return format!("Copy the partition table of {o} to {s}");
        }
        K::RandomizeGuids => return format!("Give {s} its own partition GUIDs"),
        K::ReplaceCopy => return format!("Copy {s} onto the new disk while it stays in the array"),
        K::RemoveReplaced => return format!("Remove {s} (marked faulty by the replace)"),
        K::MarkFailed => return format!("Mark {s} as failed"),
        K::RemoveMember => return format!("Remove {s} from the array"),
        K::CheckArray => {
            return format!("Check the parity/mirrors of {s} (read-only, counts mismatches)");
        }
        K::WaitCheck => return format!("Wait for the {s} check, then read mismatch_cnt"),
        K::CreatePv => return format!("Make {s} an LVM physical volume"),
        K::CreateVg => return format!("Create the volume group {s} from the tier arrays"),
        K::CreateLv => return format!("Create the logical volume {s} over all the free space"),
        K::ExtendVg => {
            let o = a.other.as_deref().unwrap_or("");
            return format!("Add {o} to the volume group {s}");
        }
        K::ResizePv => return format!("Grow the physical volume on {s} to the array size"),
        K::ExtendLv => return format!("Grow the logical volume {s} over the free space"),
        K::BitmapOff => {
            return format!("Turn off the write-intent bitmap of {s} for the level change");
        }
        K::BitmapOn => return format!("Turn the write-intent bitmap of {s} back on"),
        K::AddPartitionLive => {
            return format!("Tell the kernel about the new partition {s} (the disk is in use)");
        }
        K::DeletePartitions => {
            return format!("Delete the partitions {} of {s} (its other partitions stay)", a.items.join(", "));
        }
        K::RemovePartitionLive => {
            return format!("Tell the kernel that the partition {s} is gone (the disk is in use)");
        }
        K::BtrfsMountDegraded => {
            let n = a.count.unwrap_or(0);
            return format!("Mount {s} degraded, on the {n} device{} present", if n == 1 { "" } else { "s" });
        }
        K::BtrfsReplace => {
            let o = a.other.as_deref().unwrap_or("");
            return format!("Rebuild missing device {o} on {s}");
        }
        K::BtrfsWaitReplace => return format!("Wait for the rebuild on {s} to finish"),
        K::BtrfsResize => {
            let o = a.other.as_deref().unwrap_or("");
            return format!("Grow device {o} of {s} to the whole disk");
        }
        K::BtrfsRemoveMissing => {
            return format!("Remove the missing devices of {s} (the others take their data)");
        }
        K::BtrfsConvert if a.items.is_empty() => {
            return format!("Convert the chunks of {s} written with a weaker profile meanwhile back, if any");
        }
        K::BtrfsConvert => {
            return format!("Convert the chunks of {s} written with a weaker profile back to {}", a.items.join(", "));
        }
        K::WaitUdev => "Wait for udev to create the partition nodes",
        K::RecordArray => "Record the array so it is assembled at boot",
        K::LuksFormat => "Format the array as LUKS2 (Argon2id, 256 MiB memory, 4 KiB sectors)",
        K::AddRecoveryKey => "Add a recovery key (shown once: it must be saved before going on)",
        K::BackupLuksHeader => {
            "Back up the LUKS header (offered for download, then removed from the NAS)"
        }
        K::UnlockVolume => "Unlock the new volume",
        K::DeclareCrypttab => "Declare the encrypted volume, locked at boot (noauto)",
        K::CreateFilesystem => "Create the btrfs filesystem (data single, metadata DUP)",
        K::MountAtBoot => "Mount the volume at boot",
        K::CreateSnapshotSubvolume => {
            "Create the snapshot subvolume (btrbk snapshot_dir, hidden from shares)"
        }
        K::CreateCacheSubvolume => {
            "Create the cache subvolume (dedup hashfiles, excluded from snapshots)"
        }
        K::DisableCowCache => "Disable copy-on-write on the cache subvolume (SQLite files)",
        K::MirrorToRaid5 => "Convert the 2-disk mirror to RAID5 (instant, same data layout)",
        K::AddSpares => "Add the new partitions as spares",
        K::Reshape => {
            "Reshape the array onto the new disks (hours to days; the volume stays online)"
        }
        K::SparesToMirrors => "Turn the spares into active mirrors (resync)",
        K::WaitReshape => "Wait for the reshape to finish",
        K::WaitResync => "Wait for the resync to finish",
        K::GrowLuks => "Grow the LUKS mapping to the new array size",
        K::GrowFilesystem => "Grow the filesystem to the new size",
        K::AddSpare => "Add the new partition as a spare",
        K::WaitCopy => "Wait for the copy to finish",
        K::AddForRebuild => "Add the new partition: the rebuild starts",
        K::WaitRebuild => "Wait for the rebuild to finish",
        K::Scrub => "Verify every checksum and repair from a good copy (metadata DUP, md mirrors)",
        K::Other => "",
    };
    text.to_string()
}

/// Records a warning of the plan: its English text and its code with the values it names.
pub(crate) fn warn(p: &mut StoragePlan, code: PlanWarningCode, args: PlanArgs) {
    p.warnings.push(warning_english(code, &args));
    p.warning_items.push(PlanWarning { code, args });
}

/// A warning about one disk, array or device.
pub(crate) fn warn_on(p: &mut StoragePlan, code: PlanWarningCode, subject: &str) {
    warn(p, code, subject_args(subject));
}

/// English text of a warning (logs, CLI); the UI translates `code` + `args`.
fn warning_english(code: PlanWarningCode, a: &PlanArgs) -> String {
    use PlanWarningCode as W;
    let s = a.subject.as_deref().unwrap_or("");
    let items = a.items.join(", ");
    let text = match code {
        W::DataErased => return format!("{s}: existing data will be erased ({items})"),
        W::SmartWarnings => return format!("{s}: SMART warnings ({})", a.items.join("; ")),
        W::NoSmartData => return format!("{s}: no SMART data yet"),
        W::RemovableDisk => {
            return format!("{s}: USB/removable disk: unplugging it breaks the volume");
        }
        W::MixedSizes => {
            let gb = a.size_bytes.unwrap_or(0) as f64 / 1e9;
            return format!(
                "disks of different sizes: every member is used as large as the smallest, {gb:.1} GB unused"
            );
        }
        W::SmrRaid => {
            return format!(
                "SMR (shingled) disks: {items}: rebuilds can take days or time out; prefer CMR disks for RAID"
            );
        }
        W::SmrSlowWrites => return format!("SMR (shingled) disks: {items}: slow sustained writes"),
        W::DiskLargerThanMembers => {
            return format!(
                "{s} is larger than the members: the extra space stays unused until every member is replaced"
            );
        }
        W::SpareMember => return format!("{s} is a spare: it can simply be removed"),
        W::SmrSlowRebuild => return format!("{s}: SMR disk, the rebuild may be very slow"),
        W::OtherPartitions => {
            let n = a.count.unwrap_or(0);
            return format!(
                "{s} has {n} partitions: only the md members are rebuilt, copy the others (EFI, swap) separately"
            );
        }
        W::ArrayNotHealthy => {
            return format!(
                "{s} is not healthy: a scrub can find but not repair errors without redundancy"
            );
        }
        W::FlexUnusedSpace => {
            let gb = a.size_bytes.unwrap_or(0) as f64 / 1e9;
            return format!(
                "{gb:.1} GB on {items} stay unused until another disk reaches the same size"
            );
        }
        W::FlexLongReshape => {
            let gb = a.size_bytes.unwrap_or(0) as f64 / 1e9;
            let h = a.count.unwrap_or(0);
            return format!(
                "the tiers are reshaped one after the other ({gb:.0} GB, about {h} h); the volume stays online"
            );
        }
        W::FlexReducedToleranceWindow => {
            return format!(
                "tier {s} tolerates only one failed disk while it is reshaped to RAID6"
            );
        }
        W::DiskShortOfTier => {
            let gb = a.size_bytes.unwrap_or(0) as f64 / 1e9;
            let tier = a.count.unwrap_or(0);
            return format!("{s}: {gb:.1} GB missing for tier {tier}: it joins only the tiers below");
        }
        W::FlexNoRecentScrub => {
            let days = a.count.unwrap_or(0);
            return format!(
                "{s}: no scrub finished without errors in the last {days} days; a read error during the reshapes is the main risk: scrub first"
            );
        }
        W::FlexLeftoverDisk => {
            return format!(
                "{s} holds old slices of this volume in no tier and is not erased: add it to a volume or erase it (an old superblock can confuse assembling the tiers at boot)"
            );
        }
        W::MixedKinds => {
            "mixed disk types (HDD/SSD/NVMe): the array runs at the speed of the slowest"
        }
        W::SingleNoRedundancy => {
            "no redundancy: one disk failure loses the volume (a mirror disk can be added later)"
        }
        W::Raid0NoRedundancy => "RAID0 has no redundancy: any disk failure loses the whole volume",
        W::Raid5Rebuild => {
            "RAID5 with large or many HDDs: a second failure during the long rebuild loses the volume; consider RAID6"
        }
        W::Encrypted => {
            "encrypted: the volume stays locked after every boot until unlocked from rynas; keep the recovery key and the header backup off the NAS"
        }
        W::CompressionOff => "compression off: zstd:1 costs little CPU and usually saves space",
        W::SsdTrimOff => {
            "SSD under LUKS: TRIM is off by default because it reveals which blocks are free; enable 'discard' in crypttab if that is acceptable"
        }
        W::GrowFilesystemByHand => "no mounted btrfs on this array: grow the filesystem by hand",
        W::ScrubCost => {
            "scrub reads every allocated block: it takes hours on large HDD volumes and wakes sleeping disks; it never runs together with replication, balance or dedup on the same filesystem"
        }
        W::Other => "",
    };
    text.to_string()
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
        RaidLevel::Flex1 => "flex1",
        RaidLevel::Flex2 => "flex2",
    }
}

/// md level number for `mdadm --level`. A flexible RAID has one array per tier: its full
/// width tiers get the level named here, narrower ones are chosen by [`crate::flex::tiers`].
fn md_level(l: RaidLevel) -> &'static str {
    match l {
        RaidLevel::Single | RaidLevel::Raid1 => "1",
        RaidLevel::Raid0 => "0",
        RaidLevel::Raid5 | RaidLevel::Flex1 => "5",
        RaidLevel::Raid6 | RaidLevel::Flex2 => "6",
        RaidLevel::Raid10 => "10",
    }
}

/// (minimum disks, accepted count) for a level.
fn count_ok(l: RaidLevel, n: usize) -> Result<(), String> {
    if rynas_api_types::capacity::disk_count_ok(l, n) {
        return Ok(());
    }
    let min = rynas_api_types::capacity::level_min_disks(l);
    if n < min {
        return Err(format!(
            "{} needs at least {min} disks, {n} selected",
            level_name(l)
        ));
    }
    Err(match l {
        RaidLevel::Single => "single takes exactly one disk".to_string(),
        _ => format!("{} with {n} disks is not possible", level_name(l)),
    })
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

/// Records an error of the plan: its English text and its issue (code, request field and
/// the object it names).
pub(crate) fn fail(
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
pub(crate) fn check_new_disk(
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
        let args = PlanArgs {
            subject: Some(name.to_string()),
            items: usage.used_by.clone(),
            ..Default::default()
        };
        warn(p, PlanWarningCode::DataErased, args);
    }
    match d.health {
        HealthVerdict::Failing => {
            let m = format!(
                "{name}: SMART says the disk is failing ({})",
                d.health_reasons
                    .iter()
                    .filter(|r| r.acknowledged.is_none())
                    .map(|r| r.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            );
            fail(p, C::DiskFailing, field, Some(name), m)
        }
        HealthVerdict::Warning => {
            // the English text lists the messages, the UI translates the codes
            let bad = || {
                d.health_reasons
                    .iter()
                    .filter(|r| {
                        r.severity >= rynas_api_types::storage::Severity::Warning
                            && r.acknowledged.is_none()
                    })
            };
            let messages: Vec<&str> = bad().map(|r| r.message.as_str()).collect();
            p.warnings
                .push(format!("{name}: SMART warnings ({})", messages.join("; ")));
            p.warning_items.push(PlanWarning {
                code: PlanWarningCode::SmartWarnings,
                args: PlanArgs {
                    subject: Some(name.to_string()),
                    items: bad().map(|r| r.code.clone()).collect(),
                    ..Default::default()
                },
            });
        }
        HealthVerdict::Unknown => warn_on(p, PlanWarningCode::NoSmartData, name),
        HealthVerdict::Healthy => {}
    }
    if d.removable || d.transport == "usb" {
        warn_on(p, PlanWarningCode::RemovableDisk, name);
    }
    if d.size < MIN_DISK {
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

pub(crate) fn finish(mut p: StoragePlan) -> StoragePlan {
    p.valid = p.errors.is_empty();
    if !p.valid {
        p.steps.clear();
    }
    p
}

/// Erases `disk` and makes its md partition: up to [`END_RESERVE`] before the end, or up to
/// the absolute byte offset `end` (a disk joining an array it holds only without the
/// reserve).
fn partition_steps(steps: &mut Vec<PlanStep>, disk: &str, label: &str, end: Option<u64>) {
    let dev = format!("/dev/{disk}");
    steps.push(destroying(
        cmd(
            d_on(PlanStepKind::WipeSignatures, disk),
            &["wipefs", "--all", &dev],
        ),
        &[disk],
    ));
    steps.push(destroying(
        cmd(
            d_on(PlanStepKind::ClearPartitionTable, disk),
            &["sgdisk", "--zap-all", &dev],
        ),
        &[disk],
    ));
    let new = match end {
        None => "--new=1:0:-128M".to_string(),
        Some(e) => format!("--new=1:{}:{}", PART_START / 512, e / 512 - 1),
    };
    steps.push(cmd(
        d_on(PlanStepKind::CreatePartition, disk),
        &[
            "sgdisk",
            &new,
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
                a.md_name.as_deref().is_some_and(|n| {
                    let n = n.rsplit(':').next().unwrap_or(n);
                    // a flexible volume's tier arrays are <label>_t<N>
                    n == label
                        || n.strip_prefix(label)
                            .and_then(|r| r.strip_prefix("_t"))
                            .is_some_and(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()))
                })
            })
            || inv.lvm.vgs.iter().any(|v| v.name == crate::flex::plan::vg_name(label))
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
    if req.level.is_flexible() {
        if inv.lvm_missing {
            fail(&mut p, PlanIssueCode::LvmMissing, "level", None, "flexible RAID needs LVM: install lvm2".into());
            return finish(p);
        }
        return crate::flex::plan::plan_create(inv, req, p, &chosen);
    }

    // capacity
    let n = chosen.len() as u64;
    let sizes: Vec<u64> = chosen.iter().map(|d| d.size).collect();
    let min = sizes.iter().copied().min().unwrap_or(0);
    let cap = rynas_api_types::capacity::classic(req.level, &sizes, req.encrypt, inv.min_tier());
    p.raw_bytes = cap.raw;
    p.wasted_bytes = cap.wasted;
    p.usable_bytes = cap.usable;
    p.fault_tolerance = cap.tolerance;

    // warnings about the mix
    if p.wasted_bytes > 0 && n > 1 {
        let args = PlanArgs {
            size_bytes: Some(p.wasted_bytes),
            ..Default::default()
        };
        warn(&mut p, PlanWarningCode::MixedSizes, args);
    }
    mix_warnings(
        &mut p,
        &chosen,
        !matches!(req.level, RaidLevel::Single | RaidLevel::Raid0),
    );
    let level_warning = match req.level {
        RaidLevel::Single => Some(PlanWarningCode::SingleNoRedundancy),
        RaidLevel::Raid0 => Some(PlanWarningCode::Raid0NoRedundancy),
        RaidLevel::Raid5
            if chosen.iter().all(|d| d.kind == DiskKind::Hdd)
                && (n >= 6 || min >= 10_000_000_000_000) =>
        {
            Some(PlanWarningCode::Raid5Rebuild)
        }
        _ => None,
    };
    if let Some(code) = level_warning {
        warn(&mut p, code, PlanArgs::default());
    }
    if req.encrypt {
        warn(&mut p, PlanWarningCode::Encrypted, PlanArgs::default());
    }
    if !req.compress {
        warn(&mut p, PlanWarningCode::CompressionOff, PlanArgs::default());
    }
    if !p.errors.is_empty() {
        return finish(p);
    }

    // steps
    let md = format!("/dev/md/{label}");
    let mut steps = Vec::new();
    for d in &chosen {
        partition_steps(&mut steps, &d.name, label, None);
    }
    steps.push(cmd(d(PlanStepKind::WaitUdev), &["udevadm", "settle"]));
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
            D(
                PlanStepKind::CreateArray,
                PlanArgs {
                    level: Some(req.level),
                    count: Some(n),
                    ..Default::default()
                },
            ),
            &argv_ref,
        ),
        &part_names,
    ));
    steps.push(config(
        d(PlanStepKind::RecordArray),
        "/etc/mdadm/mdadm.conf",
        &format!("ARRAY {md} metadata=1.2 name=<host>:{label} UUID=<from mdadm --detail --brief>"),
    ));
    let ssd = chosen.iter().any(|d| d.kind != DiskKind::Hdd);
    let fs_dev = volume_top(&mut p, &mut steps, label, &md, req.encrypt, req.compress, ssd);
    p.summary = vec![
        ("level".into(), level_name(req.level).into()),
        ("md".into(), md),
        ("filesystem_device".into(), fs_dev),
        ("mountpoint".into(), format!("/{label}")),
    ];
    if req.encrypt {
        p.summary.push(("mapper".into(), format!("rynas-{label}")));
    }
    p.steps = steps;
    finish(p)
}

/// Warnings about the disks of a new volume: mixed kinds, SMR disks (`raid`: the volume has
/// redundancy, so rebuilds matter).
pub(crate) fn mix_warnings(p: &mut StoragePlan, chosen: &[DiskSummary], raid: bool) {
    let kinds: std::collections::HashSet<DiskKind> = chosen.iter().map(|d| d.kind).collect();
    if kinds.len() > 1 {
        warn(p, PlanWarningCode::MixedKinds, PlanArgs::default());
    }
    let smr: Vec<String> = chosen
        .iter()
        .filter(|d| d.smr)
        .map(|d| d.name.clone())
        .collect();
    if !smr.is_empty() {
        let code = if raid {
            PlanWarningCode::SmrRaid
        } else {
            PlanWarningCode::SmrSlowWrites
        };
        let args = PlanArgs {
            items: smr,
            ..Default::default()
        };
        warn(p, code, args);
    }
}

/// The steps of a new volume above its block device `base` (the md array, or the logical
/// volume of a flexible RAID): optional LUKS2, btrfs, the fstab entry, the mount and the
/// subvolumes. Returns the filesystem device.
pub(crate) fn volume_top(
    p: &mut StoragePlan,
    steps: &mut Vec<PlanStep>,
    label: &str,
    base: &str,
    encrypt: bool,
    compress: bool,
    ssd: bool,
) -> String {
    let mapper_name = format!("rynas-{label}");
    let mnt = format!("/{label}");
    let fs_dev = if encrypt {
        format!("/dev/mapper/{mapper_name}")
    } else {
        base.to_string()
    };
    if encrypt {
        steps.push(cmd_secret(
            d(PlanStepKind::LuksFormat),
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
                base,
            ],
            "passphrase",
        ));
        steps.push(cmd_secret(
            d(PlanStepKind::AddRecoveryKey),
            &[
                "systemd-cryptenroll",
                "--recovery-key",
                "--unlock-key-file=/dev/stdin",
                base,
            ],
            "passphrase",
        ));
        steps.push(cmd(
            d(PlanStepKind::BackupLuksHeader),
            &[
                "cryptsetup",
                "luksHeaderBackup",
                base,
                "--header-backup-file",
                &format!("/run/rynas/luks-header-{label}.img"),
            ],
        ));
        steps.push(cmd_secret(
            d(PlanStepKind::UnlockVolume),
            &["cryptsetup", "open", "--key-file", "-", base, &mapper_name],
            "passphrase",
        ));
        steps.push(config(
            d(PlanStepKind::DeclareCrypttab),
            "/etc/crypttab",
            &format!("{mapper_name} UUID=<luks uuid> none luks,noauto"),
        ));
        if ssd {
            warn(p, PlanWarningCode::SsdTrimOff, PlanArgs::default());
        }
    }
    steps.push(cmd(
        d(PlanStepKind::CreateFilesystem),
        // --force: fs_dev is the array (or LUKS mapping) this plan has just created, so any
        // signature on it is a leftover of the member disks' old content, which the user
        // already agreed to erase (md --create does not zero the data area: a filesystem from
        // an earlier, rolled back attempt shows through at the same offset)
        &[
            "mkfs.btrfs",
            "--force",
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
    if compress {
        opts.push_str(&format!(",compress={COMPRESSION}"));
    }
    // fstab first: systemd mounts the volume from it (a mount made inside rynasd's own
    // mount namespace would stay invisible to the rest of the system)
    steps.push(config(
        d(PlanStepKind::MountAtBoot),
        "/etc/fstab",
        &format!(
            "UUID=<btrfs uuid> {mnt} btrfs {opts},nofail{} 0 0",
            if encrypt { ",noauto" } else { "" }
        ),
    ));
    steps.push(cmd(
        d_on(PlanStepKind::CreateMountPoint, &mnt),
        &["mkdir", "-p", &mnt],
    ));
    steps.push(cmd(
        d_on(PlanStepKind::MountVolume, &mnt),
        &["systemctl", "start", &mount_unit(&mnt)],
    ));
    steps.push(cmd(
        d(PlanStepKind::CreateSnapshotSubvolume),
        &["btrfs", "subvolume", "create", &format!("{mnt}/.snapshots")],
    ));
    steps.push(cmd(
        d(PlanStepKind::CreateCacheSubvolume),
        &[
            "btrfs",
            "subvolume",
            "create",
            &format!("{mnt}/@rynas-cache"),
        ],
    ));
    steps.push(cmd(
        d(PlanStepKind::DisableCowCache),
        &["chattr", "+C", &format!("{mnt}/@rynas-cache")],
    ));
    fs_dev
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

/// Bytes a new member partition of `a` needs: the size md uses of each member
/// (`component_size`) after the data offset mdadm gives the new member
/// ([`need_for_component`]), or the smallest active member partition when md does not say
/// (md uses no more of any member than the smallest one holds).
fn member_need(inv: &Inventory, a: &MdArray) -> u64 {
    let active = || a.members.iter().filter(|m| m.role == MemberRole::Active);
    let smallest = active()
        .filter_map(|m| inv.graph.get(&m.device))
        .map(|d| d.size)
        .min()
        .unwrap_or(0);
    match a.component_size {
        Some(c) => need_for_component(c, active().filter_map(|m| m.data_offset).max()),
        None => smallest,
    }
}

/// The smallest member partition that holds a component of `c` bytes whatever data offset
/// `mdadm --add` gives it: mdadm picks the offset of a new member from that member's own
/// size (metadata 1.2 defaults, [`md_data_offset`]), not from the members already in the
/// array, so it may be larger than theirs (members made by an older mdadm, or smaller
/// ones). The partition holds the larger of the two.
fn need_for_component(c: u64, existing_offset: Option<u64>) -> u64 {
    let floor = existing_offset.unwrap_or(0);
    let mut need = c + floor.max(md_data_offset(c));
    // the default offset grows with the partition: a few rounds settle it
    for _ in 0..8 {
        let next = c + floor.max(md_data_offset(need));
        if next <= need {
            break;
        }
        need = next;
    }
    need
}

/// `mdadm --add` of a partition of `part` bytes leaves md at least `c` bytes after the data
/// offset it picks for it ([`need_for_component`]).
fn holds_component(part: u64, c: Option<u64>) -> bool {
    c.is_none_or(|c| part.saturating_sub(md_data_offset(part)) >= c)
}

/// The size md uses of each member of `a`: `component_size`, or what md would make of the
/// smallest member partition.
fn member_component(inv: &Inventory, a: &MdArray, level: RaidLevel) -> u64 {
    a.component_size
        .unwrap_or_else(|| md_component(level, member_need(inv, a)))
}

/// Where the md partition of a disk of `size` bytes joining an array whose members need
/// `need` bytes (md using `component` of each) ends: `None` for the usual partition (up to
/// [`END_RESERVE`] before the end), an absolute end for a disk too small for that (a disk
/// "of the same size" a few MB smaller: the partition holds exactly `need`), the bytes
/// missing when it is too small.
fn member_partition_end(size: u64, need: u64, component: Option<u64>) -> Result<Option<u64>, u64> {
    let usual = usable_partition(size);
    // a larger partition may get a larger data offset ([`holds_component`]): then the
    // partition holds exactly `need`
    if usual >= need && holds_component(usual, component) {
        return Ok(None);
    }
    let end = PART_START + need.div_ceil(MIB) * MIB;
    if fits(size, end) {
        Ok(Some(end))
    } else {
        Err(shortfall(size, end))
    }
}

/// A member size as a striped level keeps it: whole chunks.
fn striped_component(level: RaidLevel, component: u64) -> u64 {
    match level {
        RaidLevel::Single | RaidLevel::Raid1 => component,
        _ => component / MD_CHUNK * MD_CHUNK,
    }
}

/// `POST /api/v1/storage/plan/grow`
pub fn plan_grow(inv: &Inventory, req: &GrowPlanRequest) -> StoragePlan {
    if req.volume.is_some() {
        return crate::flex::plan::plan_grow(inv, req);
    }
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
    let need = member_need(inv, a);
    let label = a
        .md_name
        .as_deref()
        .map(|n| n.rsplit(':').next().unwrap_or(n).to_string())
        .unwrap_or_else(|| a.name.clone());
    let mut new: Vec<(DiskSummary, Option<u64>)> = Vec::new();
    for name in &req.disks {
        if let Some(d) = check_new_disk(inv, name, "disks", &mut p) {
            match member_partition_end(d.size, need, a.component_size) {
                Err(missing) => {
                    let m = format!(
                        "{name}: too small ({:.1} GB, {:.1} GB missing: members use {:.1} GB)",
                        d.size as f64 / 1e9,
                        missing as f64 / 1e9,
                        need as f64 / 1e9
                    );
                    fail(&mut p, PlanIssueCode::DiskTooSmall, "disks", Some(name), m);
                }
                Ok(end) => {
                    if !negligible_difference(usable_partition(d.size), need)
                        && usable_partition(d.size) > need
                    {
                        warn_on(&mut p, PlanWarningCode::DiskLargerThanMembers, name);
                    }
                    new.push((d, end));
                }
            }
        }
    }
    let component = member_component(inv, a, to);
    let (_, tol) = capacity_factor(to, new_n.max(1));
    p.usable_bytes = if count_ok(to, new_n as usize).is_ok() {
        // a change to a striped level keeps whole chunks of each member
        md_array_size_of_components(to, new_n as usize, striped_component(to, component))
    } else {
        0
    };
    p.fault_tolerance = tol;
    p.raw_bytes = new.iter().map(|(d, _)| d.size).sum();
    if !p.errors.is_empty() {
        return finish(p);
    }

    let md = a.path.clone();
    let mut steps = Vec::new();
    for (d, end) in &new {
        partition_steps(&mut steps, &d.name, &label, *end);
    }
    steps.push(cmd(d(PlanStepKind::WaitUdev), &["udevadm", "settle"]));
    let mut add: Vec<String> = vec!["mdadm".into(), md.clone(), "--add".into()];
    add.extend(
        new.iter()
            .map(|(d, _)| format!("/dev/{}", part_name(&d.name, 1))),
    );
    let backup = format!("--backup-file=/var/lib/rynas/md-reshape-{}.bak", a.name);
    let raid_devices = format!("--raid-devices={new_n}");
    let reshape = to != RaidLevel::Raid1;
    // a write-intent bitmap can block a level change or a reshape: off first, back on after
    let bitmap = reshape && a.bitmap.as_deref().is_some_and(|b| b != "none");
    if bitmap {
        steps.push(cmd(
            d_on(PlanStepKind::BitmapOff, &a.name),
            &["mdadm", "--grow", &md, "--bitmap=none"],
        ));
    }
    if from == RaidLevel::Raid1 && to == RaidLevel::Raid5 {
        steps.push(cmd(
            d(PlanStepKind::MirrorToRaid5),
            &["mdadm", "--grow", &md, "--level=5"],
        ));
    }
    steps.push(cmd(
        d(PlanStepKind::AddSpares),
        &add.iter().map(String::as_str).collect::<Vec<_>>(),
    ));
    let mut grow = vec!["mdadm", "--grow", &md, &raid_devices];
    let level6 = "--level=6";
    if from == RaidLevel::Raid5 && to == RaidLevel::Raid6 {
        grow.insert(3, level6);
        grow.push(&backup);
    }
    steps.push(cmd(
        d(if reshape {
            PlanStepKind::Reshape
        } else {
            PlanStepKind::SparesToMirrors
        }),
        &grow,
    ));
    steps.push(wait(
        d(if reshape {
            PlanStepKind::WaitReshape
        } else {
            PlanStepKind::WaitResync
        }),
        &format!("/sys/block/{}/md/sync_action == idle", a.name),
    ));
    if bitmap {
        steps.push(cmd(
            d_on(PlanStepKind::BitmapOn, &a.name),
            &["mdadm", "--grow", &md, "--bitmap=internal"],
        ));
    }
    if reshape {
        let (mapper, mnt) = stack_above(inv, &a.name);
        if let Some(m) = &mapper {
            steps.push(cmd_secret(
                d(PlanStepKind::GrowLuks),
                &["cryptsetup", "resize", "--key-file", "-", m],
                "passphrase",
            ));
        }
        match &mnt {
            Some(m) => steps.push(cmd(
                d(PlanStepKind::GrowFilesystem),
                &["btrfs", "filesystem", "resize", "max", m],
            )),
            None => warn(
                &mut p,
                PlanWarningCode::GrowFilesystemByHand,
                PlanArgs::default(),
            ),
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
    if req.volume.is_some() {
        return crate::flex::plan::plan_replace(inv, req);
    }
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
        warn_on(&mut p, PlanWarningCode::SpareMember, &member.device);
    }
    // the healthy members left to rebuild from
    let healthy: Vec<&rynas_api_types::storage::MdMember> = a
        .members
        .iter()
        .filter(|m| m.role == MemberRole::Active && m.device != member.device && m.disk.is_some())
        .collect();
    if healthy.is_empty() {
        let m = format!("{}: no healthy member left to rebuild from", a.name);
        fail(
            &mut p,
            PlanIssueCode::NoHealthyMember,
            "array",
            Some(&a.name),
            m,
        );
        return finish(p);
    }
    let disk_size = |name: &str| inv.disks.iter().find(|d| d.name == name).map(|d| d.size);
    // what the new member needs: md's data offset and the size md uses of each member, not
    // the size of another (maybe larger) member's disk
    let need = member_need(inv, a);
    let new = check_new_disk(inv, &req.new_disk, "new_disk", &mut p);
    // the partition table to copy: that of a healthy member's disk no larger than the new
    // one (the largest such); without one the new disk gets the md partition alone, sized
    // for the array, where the smallest member's partition starts
    let source = new.as_ref().and_then(|nd| {
        // reversed: of equal disks the first member is chosen
        healthy
            .iter()
            .rev()
            .filter_map(|m| {
                let disk = m.disk.clone()?;
                let size = disk_size(&disk)?;
                // a copy of its partition must hold what md uses after the data offset
                // mdadm picks for the new member, which may exceed this member's
                let part = inv.graph.get(&m.device).map_or(0, |d| d.size);
                (size <= nd.size && part >= need && holds_component(part, a.component_size))
                    .then_some((size, disk, m.device.clone()))
            })
            .max_by_key(|(size, _, _)| *size)
            .map(|(_, disk, dev)| (disk, dev))
    });
    let smallest = healthy
        .iter()
        .min_by_key(|m| inv.graph.get(&m.device).map_or(u64::MAX, |d| d.size))
        .copied()
        .unwrap_or(healthy[0]);
    let reference = match &source {
        Some((disk, _)) => disk.clone(),
        None => smallest.disk.clone().unwrap_or_default(),
    };
    let start = inv
        .graph
        .get(&smallest.device)
        .and_then(|d| d.start)
        .unwrap_or(PART_START);
    let fresh_end = start + need.div_ceil(MIB) * MIB;
    if let Some(d) = &new {
        if source.is_none() && !fits(d.size, fresh_end) {
            let m = format!(
                "{}: too small ({:.1} GB, {:.1} GB missing: the members use {:.1} GB from {:.1} MB on)",
                d.name,
                d.size as f64 / 1e9,
                shortfall(d.size, fresh_end) as f64 / 1e9,
                need as f64 / 1e9,
                start as f64 / 1e6
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
            warn_on(&mut p, PlanWarningCode::SmrSlowRebuild, &d.name);
        }
    }
    if let Some(s) = &a.sync
        && s.action != "recovery"
    {
        let m = format!("{} is busy ({}); wait for it to finish", a.name, s.action);
        fail(&mut p, PlanIssueCode::ArrayBusy, "array", Some(&a.name), m);
    }
    // other partitions on the members' disks (EFI, swap) are not copied by md
    let ref_parts = inv
        .graph
        .get(&reference)
        .map(|d| d.children.len())
        .unwrap_or(0);
    if ref_parts > 1 {
        let args = PlanArgs {
            subject: Some(reference.clone()),
            count: Some(ref_parts as u64),
            ..Default::default()
        };
        warn(&mut p, PlanWarningCode::OtherPartitions, args);
    }
    let tol = crate::md::tolerance(&a.level, a.raid_disks);
    p.fault_tolerance = tol;
    p.usable_bytes = a.size;
    if !p.errors.is_empty() {
        return finish(p);
    }
    let new = new.expect("checked above");
    let new_dev = format!("/dev/{}", new.name);
    // the partition number: the copied member's, else the replaced member's
    let part_no_of = |dev: &str, disk: Option<&str>| {
        inv.graph.get(dev).and_then(|d| d.partn).or_else(|| {
            dev.strip_prefix(disk.unwrap_or(""))
                .map(|s| s.trim_start_matches('p'))
                .and_then(|n| n.parse::<u32>().ok())
        })
    };
    let part_no = match &source {
        Some((disk, dev)) => part_no_of(dev, Some(disk)),
        None => part_no_of(&member.device, member.disk.as_deref()),
    }
    .unwrap_or(1);
    let new_part = format!("/dev/{}", part_name(&new.name, part_no));
    let old_dev = format!("/dev/{}", member.device);
    let md = a.path.clone();
    let wipe = destroying(
        cmd(
            d_on(PlanStepKind::WipeSignatures, &new.name),
            &["wipefs", "--all", &new_dev],
        ),
        &[&new.name],
    );
    let mut steps = match &source {
        Some((src, _)) => vec![
            wipe,
            destroying(
                cmd(
                    D(
                        PlanStepKind::CopyPartitionTable,
                        PlanArgs {
                            subject: Some(new.name.clone()),
                            other: Some(src.clone()),
                            ..Default::default()
                        },
                    ),
                    &[
                        "sgdisk",
                        &format!("--replicate={new_dev}"),
                        &format!("/dev/{src}"),
                    ],
                ),
                &[&new.name],
            ),
            // a larger disk: the backup GPT goes to its end
            cmd(
                d_on(PlanStepKind::RandomizeGuids, &new.name),
                &["sgdisk", "--randomize-guids", "--move-second-header", &new_dev],
            ),
        ],
        None => {
            let label = a
                .md_name
                .as_deref()
                .map(|n| n.rsplit(':').next().unwrap_or(n).to_string())
                .unwrap_or_else(|| a.name.clone());
            vec![
                wipe,
                destroying(
                    cmd(
                        d_on(PlanStepKind::ClearPartitionTable, &new.name),
                        &["sgdisk", "--zap-all", &new_dev],
                    ),
                    &[&new.name],
                ),
                cmd(
                    d_on(PlanStepKind::CreatePartition, &new.name),
                    &[
                        "sgdisk",
                        &format!("--new={part_no}:{}:{}", start / 512, fresh_end / 512 - 1),
                        &format!("--typecode={part_no}:FD00"),
                        &format!("--change-name={part_no}:rynas-{label}"),
                        &new_dev,
                    ],
                ),
            ]
        }
    };
    steps.push(cmd(d(PlanStepKind::WaitUdev), &["udevadm", "settle"]));
    let hot = member.role == MemberRole::Active && a.health == ArrayHealth::Healthy;
    if hot {
        // keeps full redundancy during the copy
        steps.push(cmd(
            d(PlanStepKind::AddSpare),
            &["mdadm", &md, "--add-spare", &new_part],
        ));
        steps.push(cmd(
            d_on(PlanStepKind::ReplaceCopy, &member.device),
            &["mdadm", &md, "--replace", &old_dev, "--with", &new_part],
        ));
        steps.push(wait(
            d(PlanStepKind::WaitCopy),
            &format!("/sys/block/{}/md/sync_action == idle", a.name),
        ));
        steps.push(cmd(
            d_on(PlanStepKind::RemoveReplaced, &member.device),
            &["mdadm", &md, "--remove", &old_dev],
        ));
    } else {
        if member.role != MemberRole::Faulty {
            steps.push(cmd(
                d_on(PlanStepKind::MarkFailed, &member.device),
                &["mdadm", &md, "--fail", &old_dev],
            ));
        }
        steps.push(cmd(
            d_on(PlanStepKind::RemoveMember, &member.device),
            &["mdadm", &md, "--remove", &old_dev],
        ));
        steps.push(cmd(
            d(PlanStepKind::AddForRebuild),
            &["mdadm", &md, "--add", &new_part],
        ));
        steps.push(wait(
            d(PlanStepKind::WaitRebuild),
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
            warn_on(&mut p, PlanWarningCode::ArrayNotHealthy, &a.name);
        }
        if req.md_check {
            array_busy(a, &mut p);
        }
    }
    // counted once: a btrfs RAID1 holds every byte on two devices
    p.usable_bytes = fs.usable_size();
    warn(&mut p, PlanWarningCode::ScrubCost, PlanArgs::default());
    if !p.errors.is_empty() {
        return finish(p);
    }
    let mut steps = vec![cmd(
        d(PlanStepKind::Scrub),
        &["btrfs", "scrub", "start", "-B", "-d", &mnt],
    )];
    if req.md_check {
        for a in &arrays {
            if a.level == "raid0" || a.level == "linear" {
                continue;
            }
            steps.push(sysfs(
                d_on(PlanStepKind::CheckArray, &a.name),
                &format!("/sys/block/{}/md/sync_action", a.name),
                "check",
            ));
            steps.push(wait(
                d_on(PlanStepKind::WaitCheck, &a.name),
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

    /// One code per English warning, in the same order and with the same text; every step
    /// has a kind whose English text is its description.
    fn assert_codes_match(p: &StoragePlan) {
        assert_eq!(p.warning_items.len(), p.warnings.len(), "{:?}", p.warnings);
        for (w, text) in p.warning_items.iter().zip(&p.warnings) {
            assert_ne!(w.code, PlanWarningCode::Other);
            if w.code != PlanWarningCode::SmartWarnings {
                assert_eq!(&warning_english(w.code, &w.args), text);
            }
        }
        for s in &p.steps {
            let kind = s.kind.expect("step kind");
            assert_ne!(kind, PlanStepKind::Other);
            assert_eq!(step_english(kind, &s.args), s.description);
        }
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
                "mkfs.btrfs --force --label vol2 --data single --metadata dup /dev/mapper/rynas-vol2",
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
        // every warning has its code, in the same order as the English texts
        assert_codes_match(&p);
        let codes: Vec<PlanWarningCode> = p.warning_items.iter().map(|w| w.code).collect();
        assert!(
            codes.contains(&PlanWarningCode::SingleNoRedundancy),
            "{codes:?}"
        );
        assert!(codes.contains(&PlanWarningCode::Encrypted), "{codes:?}");
        let smr = p
            .warning_items
            .iter()
            .find(|w| w.code == PlanWarningCode::SmrSlowWrites)
            .expect("SMR warning");
        assert_eq!(smr.args.items, ["sdd"]);
        // every step has its kind; the array step names level and disk count
        let create = p
            .steps
            .iter()
            .find(|s| s.kind == Some(PlanStepKind::CreateArray))
            .unwrap();
        assert_eq!(create.args.level, Some(RaidLevel::Single));
        assert_eq!(create.args.count, Some(1));
        assert_eq!(create.description, "Create the md array (single, 1 disk)");
        let mount = p
            .steps
            .iter()
            .find(|s| s.kind == Some(PlanStepKind::MountVolume))
            .unwrap();
        assert_eq!(mount.args.subject.as_deref(), Some("/vol2"));
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
        // what the larger disk's partition holds beyond the smaller one's
        assert_eq!(p.wasted_bytes, usable_partition(4_000_787_030_016) - usable_partition(2_000_398_934_016));
        let w = p.warnings.join("\n");
        assert!(w.contains("different sizes"), "{w}");
        assert!(w.contains("sde: existing data will be erased"), "{w}");
        assert!(w.contains("compression off"), "{w}");
        assert_codes_match(&p);
        let sizes = p
            .warning_items
            .iter()
            .find(|w| w.code == PlanWarningCode::MixedSizes)
            .unwrap();
        assert_eq!(sizes.args.size_bytes, Some(p.wasted_bytes));
        let erased = p
            .warning_items
            .iter()
            .find(|w| w.code == PlanWarningCode::DataErased)
            .unwrap();
        assert_eq!(erased.args.subject.as_deref(), Some("sde"));
        assert!(!erased.args.items.is_empty());
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
    fn flexible_levels_go_to_the_tier_planner() {
        let inv = nas_inventory();
        let req = |level| VolumePlanRequest {
            disks: vec!["sdd".into(), "sde".into()],
            level,
            encrypt: false,
            compress: true,
            label: "x".into(),
        };
        // 4 TB + 2 TB at 1-disk tolerance: one 2 TB mirror tier, 2 TB of sdd unused
        let p = plan_volume(&inv, &req(RaidLevel::Flex1));
        assert!(p.valid, "{:?}", p.errors);
        assert_codes_match(&p);
        assert_eq!(p.tiers_after.len(), 2);
        assert_eq!(p.tiers_after[0].level, Some(RaidLevel::Raid1));
        assert_eq!(p.tiers_after[1].level, None);
        assert!(p.warning_items.iter().any(|w| w.code == PlanWarningCode::FlexUnusedSpace));
        assert!(argvs(&p).iter().any(|a| a.starts_with("vgcreate rynas_x /dev/md/x_t1")));
        // 2-disk tolerance needs four disks
        let p = plan_volume(&inv, &req(RaidLevel::Flex2));
        assert!(!p.valid && p.steps.is_empty());
        assert!(p.issues.iter().any(|i| i.code == PlanIssueCode::DiskCount), "{:?}", p.issues);
        assert_codes_match(&p);
        assert_eq!(level_name(RaidLevel::Flex2), "flex2");
    }

    #[test]
    fn flexible_levels_need_lvm() {
        let mut inv = nas_inventory();
        inv.lvm_missing = true;
        let req = |level| VolumePlanRequest {
            disks: vec!["sdd".into(), "sde".into()],
            level,
            encrypt: false,
            compress: true,
            label: "x".into(),
        };
        let p = plan_volume(&inv, &req(RaidLevel::Flex1));
        assert!(!p.valid && p.steps.is_empty());
        assert_eq!(p.issues.iter().map(|i| i.code).collect::<Vec<_>>(), [PlanIssueCode::LvmMissing]);
        assert_codes_match(&p);
        // classic levels do not need it
        assert!(plan_volume(&inv, &req(RaidLevel::Raid1)).valid);
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
                volume: None,
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
                volume: None,
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
        assert_codes_match(&p);
        // too small disk
        let p = plan_grow(
            &inv,
            &GrowPlanRequest {
                array: "md2".into(),
                disks: vec!["sdd".into()],
                level: None,
                volume: None,
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
                volume: None,
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
                volume: None,
            },
        );
        assert!(!p.valid);
    }

    /// RAID1 → RAID5: the write-intent bitmap goes off before the level change and back on
    /// once the reshape ended.
    #[test]
    fn mirror_to_raid5_turns_the_bitmap_off_first() {
        let mut inv = nas_inventory();
        let a = inv.arrays.iter_mut().find(|a| a.name == "md0").unwrap();
        assert_eq!((a.level.as_str(), a.raid_disks), ("raid1", 2));
        a.bitmap = Some("internal".into());
        a.health = ArrayHealth::Healthy;
        a.sync = None;
        let mut new = inv.disks.iter().find(|d| d.name == "sda").unwrap().clone();
        new.name = "sdf".into();
        new.path = "/dev/sdf".into();
        inv.disks.push(new);
        let p = plan_grow(
            &inv,
            &GrowPlanRequest {
                array: "md0".into(),
                disks: vec!["sdf".into()],
                level: Some(RaidLevel::Raid5),
                volume: None,
            },
        );
        assert!(p.valid, "{:?}", p.errors);
        assert_codes_match(&p);
        let a = argvs(&p);
        let pos = |s: &str| a.iter().position(|x| x == s).unwrap_or_else(|| panic!("{s}: {a:?}"));
        let off = pos("mdadm --grow /dev/md0 --bitmap=none");
        let level = pos("mdadm --grow /dev/md0 --level=5");
        let grow = pos("mdadm --grow /dev/md0 --raid-devices=3");
        let on = pos("mdadm --grow /dev/md0 --bitmap=internal");
        assert!(off < level && level < grow && grow < on, "{a:?}");
        let wait = p
            .steps
            .iter()
            .position(|s| s.kind == Some(PlanStepKind::WaitReshape))
            .unwrap();
        let on_step = p
            .steps
            .iter()
            .position(|s| s.kind == Some(PlanStepKind::BitmapOn))
            .unwrap();
        assert!(wait < on_step);
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
                volume: None,
            },
        );
        assert!(p.valid, "{:?}", p.errors);
        let a = argvs(&p);
        assert_eq!(a[1], "sgdisk --replicate=/dev/sdf /dev/sda");
        assert_codes_match(&p);
        let copy = &p.steps[1];
        assert_eq!(copy.kind, Some(PlanStepKind::CopyPartitionTable));
        assert_eq!(copy.args.subject.as_deref(), Some("sdf"));
        assert_eq!(copy.args.other.as_deref(), Some("sda"));
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
                volume: None,
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
        assert_codes_match(&p);

        let p = plan_replace(
            &inv,
            &ReplacePlanRequest {
                array: "md2".into(),
                old: "sdd".into(),
                new_disk: "sdf".into(),
                volume: None,
            },
        );
        assert!(p.errors[0].contains("not a member"));
    }

    const TB: u64 = 1_000_000_000_000;

    /// md2 healthy over 4 + 4 + 8 TB (sda, sdb: 4 TB, sdc: 8 TB, partitions made by rynas)
    /// and a blank disk sdf of `new` bytes.
    fn mixed_raid5(new: u64) -> Inventory {
        let mut inv = nas_inventory();
        for (name, size) in [("sda", 4 * TB), ("sdb", 4 * TB), ("sdc", 8 * TB)] {
            if let Some(d) = inv.disks.iter_mut().find(|d| d.name == name) {
                d.size = size;
            }
            if let Some(g) = inv.graph.devs.get_mut(name) {
                g.size = size;
            }
            if let Some(g) = inv.graph.devs.get_mut(&format!("{name}1")) {
                g.size = usable_partition(size);
                g.start = Some(PART_START);
                g.partn = Some(1);
            }
        }
        let a = inv.arrays.iter_mut().find(|a| a.name == "md2").unwrap();
        a.sync = None;
        a.health = ArrayHealth::Healthy;
        a.degraded = 0;
        for m in &mut a.members {
            m.role = MemberRole::Active;
        }
        let mut d = inv.disks.iter().find(|d| d.name == "sdd").unwrap().clone();
        d.name = "sdf".into();
        d.path = "/dev/sdf".into();
        d.serial = "NEW-SDF".into();
        d.size = new;
        inv.disks.push(d);
        inv
    }

    fn grow_md2(inv: &Inventory) -> StoragePlan {
        plan_grow(
            inv,
            &GrowPlanRequest {
                array: "md2".into(),
                disks: vec!["sdf".into()],
                level: None,
                volume: None,
            },
        )
    }

    fn replace_md2(inv: &Inventory, old: &str) -> StoragePlan {
        plan_replace(
            inv,
            &ReplacePlanRequest {
                array: "md2".into(),
                old: old.into(),
                new_disk: "sdf".into(),
                volume: None,
            },
        )
    }

    /// RAID5 on 4 + 4 + 8: md uses 4 TB of each member, so a fourth 4 TB joins and the
    /// array grows by 4 TB (not 8).
    #[test]
    fn classic_grow_uses_what_md_uses_of_each_member() {
        let part = usable_partition(4 * TB);
        let p = grow_md2(&mixed_raid5(4 * TB));
        assert!(p.valid, "{:?}", p.errors);
        assert_codes_match(&p);
        assert_eq!(p.usable_bytes, 3 * md_component(RaidLevel::Raid5, part));
        assert!(argvs(&p).iter().any(|l| l.starts_with("sgdisk --new=1:0:-128M ")), "{:?}", argvs(&p));
        assert!(!p.warning_items.iter().any(|w| w.code == PlanWarningCode::DiskLargerThanMembers));
        // a "4 TB" of another make, 5 MB smaller: its partition holds exactly what md uses
        let p = grow_md2(&mixed_raid5(4 * TB - 5_000_000));
        assert!(p.valid, "{:?}", p.errors);
        let end = PART_START + part.div_ceil(MIB) * MIB;
        let want = format!("sgdisk --new=1:2048:{} --typecode=1:FD00 --change-name=1:rynas-vol1 /dev/sdf", end / 512 - 1);
        assert!(argvs(&p).contains(&want), "{:?}", argvs(&p));
        // md says what it uses: its component size and data offset
        let mut inv = mixed_raid5(4 * TB - 5_000_000);
        let c = md_component(RaidLevel::Raid5, part);
        let a = inv.arrays.iter_mut().find(|a| a.name == "md2").unwrap();
        a.component_size = Some(c);
        for m in &mut a.members {
            m.data_offset = Some(MD_OVERHEAD_TEST);
        }
        let p = grow_md2(&inv);
        assert!(p.valid, "{:?}", p.errors);
        assert_eq!(p.usable_bytes, 3 * c);
        let end = PART_START + (c + MD_OVERHEAD_TEST).div_ceil(MIB) * MIB;
        assert!(argvs(&p).iter().any(|l| l.starts_with(&format!("sgdisk --new=1:2048:{} ", end / 512 - 1))), "{:?}", argvs(&p));
        // an 8 TB is larger than what the members use
        let p = grow_md2(&mixed_raid5(8 * TB));
        assert!(p.warning_items.iter().any(|w| w.code == PlanWarningCode::DiskLargerThanMembers));
        // too small: the plan says how much is missing
        let p = grow_md2(&mixed_raid5(3 * TB));
        assert!(!p.valid);
        assert!(p.issues.iter().any(|i| i.code == PlanIssueCode::DiskTooSmall), "{:?}", p.issues);
        assert!(p.errors.iter().any(|e| e.contains("GB missing")), "{:?}", p.errors);
    }

    const MD_OVERHEAD_TEST: u64 = rynas_api_types::capacity::MD_OVERHEAD;

    /// RAID5 on 4 + 4 + 8: a new member needs what md uses of a member, never the size of the
    /// largest member's disk; its partition table is copied only from a disk no larger.
    #[test]
    fn classic_replace_needs_what_md_uses_not_the_largest_member() {
        let inv = mixed_raid5(4 * TB);
        for old in ["sdb", "sdc"] {
            let p = replace_md2(&inv, old);
            assert!(p.valid, "{old}: {:?}", p.errors);
            assert_codes_match(&p);
            let a = argvs(&p);
            assert_eq!(a[1], "sgdisk --replicate=/dev/sdf /dev/sda", "{old}");
            assert_eq!(a[2], "sgdisk --randomize-guids --move-second-header /dev/sdf");
            assert!(a.contains(&"mdadm /dev/md2 --add-spare /dev/sdf1".to_string()), "{a:?}");
        }
        // a "4 TB" 5 MB smaller than the others: a partition of its own, sized for the array
        let p = replace_md2(&mixed_raid5(4 * TB - 5_000_000), "sdb");
        assert!(p.valid, "{:?}", p.errors);
        assert_codes_match(&p);
        let a = argvs(&p);
        assert!(!a.iter().any(|l| l.contains("--replicate")), "{a:?}");
        assert_eq!(a[1], "sgdisk --zap-all /dev/sdf");
        let end = PART_START + usable_partition(4 * TB).div_ceil(MIB) * MIB;
        assert_eq!(a[2], format!("sgdisk --new=1:2048:{} --typecode=1:FD00 --change-name=1:rynas-vol1 /dev/sdf", end / 512 - 1));
        assert_eq!(p.steps[2].kind, Some(PlanStepKind::CreatePartition));
        // too small
        let p = replace_md2(&mixed_raid5(3 * TB), "sdb");
        assert!(!p.valid);
        assert!(p.issues.iter().any(|i| i.code == PlanIssueCode::DiskTooSmall), "{:?}", p.issues);
        assert!(p.errors.iter().any(|e| e.contains("GB missing")), "{:?}", p.errors);
    }

    /// Members made with a small data offset (an older mdadm): a new member's partition holds
    /// the component after the offset mdadm picks for the new member itself, the larger one.
    #[test]
    fn new_member_holds_the_default_data_offset() {
        let c = 3_900 * 1000 * MIB;
        // existing members at 1 MiB: the new partition gets the default (129 MiB) room
        let need = need_for_component(c, Some(MIB));
        assert_eq!(need, c + md_data_offset(need));
        assert!(holds_component(need, Some(c)));
        assert!(!holds_component(c + MIB, Some(c)));
        // existing members with a larger offset than the default keep theirs
        assert_eq!(need_for_component(c, Some(200 * MIB)), c + 200 * MIB);
        // small members: the default grows with the size, the result settles on a fit
        for c in [100 * MIB, 1000 * MIB, 63 * 1024 * MIB, 64 * 1024 * MIB] {
            let need = need_for_component(c, None);
            assert!(holds_component(need, Some(c)), "{c}");
        }
        // a disk whose usual partition is just past a step of the default offset: the
        // partition is sized exactly, never the usual one that md could not fill
        let c = 64 * 1024 * MIB - 70 * MIB;
        let need = need_for_component(c, Some(MIB));
        let size = need + PART_START + 128 * MIB;
        match member_partition_end(size, need, Some(c)) {
            Ok(None) => assert!(holds_component(usable_partition(size), Some(c))),
            Ok(Some(end)) => assert!(holds_component(end - PART_START, Some(c))),
            Err(m) => panic!("{m} missing"),
        }
        // the add plan sizes the partition with the default offset, not the members' 1 MiB
        let mut inv = mixed_raid5(4 * TB - 5_000_000);
        let part = usable_partition(4 * TB);
        let c = md_component(RaidLevel::Raid5, part) - 200 * MIB;
        let a = inv.arrays.iter_mut().find(|a| a.name == "md2").unwrap();
        a.component_size = Some(c);
        for m in &mut a.members {
            m.data_offset = Some(MIB);
        }
        let p = grow_md2(&inv);
        assert!(p.valid, "{:?}", p.errors);
        let line = argvs(&p).into_iter().find(|l| l.starts_with("sgdisk --new=1:")).expect("partition");
        let end = line.split(':').nth(2).and_then(|x| x.split(' ').next()).unwrap();
        let size = match end.parse::<u64>() {
            Ok(sector) => (sector + 1) * 512 - PART_START,
            // the usual partition, up to the reserve before the end
            Err(_) => usable_partition(4 * TB - 5_000_000),
        };
        assert!(holds_component(size, Some(c)), "{line}");
    }

    /// An inventory of blank HDDs of `sizes` (sda, sdb, ...).
    fn blank(sizes: &[u64]) -> (Inventory, Vec<String>) {
        let mut inv = Inventory::default();
        let mut names = Vec::new();
        for (i, size) in sizes.iter().enumerate() {
            let name = format!("sd{}", (b'a' + i as u8) as char);
            let d = crate::lsblk::BlockDev {
                kname: name.clone(),
                name: name.clone(),
                path: format!("/dev/{name}"),
                kind: "disk".into(),
                size: *size,
                rotational: true,
                serial: Some(format!("SER-{name}")),
                ..Default::default()
            };
            inv.graph.devs.insert(name.clone(), d);
            let d = inv.graph.get(&name).unwrap();
            let s = crate::inventory::summary(d, &inv.graph, None, None, rynas_api_types::storage::PowerState::Active, 0, false);
            inv.disks.push(s);
            names.push(name);
        }
        (inv, names)
    }

    /// The web interface's calculator and the plans agree at the edges too: disks of the
    /// same size from different makes, disks too small for a tier, odd RAID10, rests thinner
    /// than a tier, encryption.
    #[test]
    fn the_calculator_matches_the_plans_at_the_edges() {
        let g = 1u64 << 30;
        let cases: [&[u64]; 8] = [
            &[8 * TB, 8 * TB + 5_000_000, 8 * TB - 5_000_000],
            &[10 * g, 10 * g],
            &[10 * g, 4 * TB],
            &[4 * TB; 5],
            &[4 * TB, 4 * TB, 4 * TB + 15 * g, 4 * TB + 16 * g + 200 * MIB],
            &[100 * MIB, 4 * TB, 4 * TB, 4 * TB],
            &[10 * g, 10 * g, 4 * TB, 4 * TB],
            &[4 * TB, 8 * TB, 8 * TB, 12 * TB, 12 * TB + 3_000_000],
        ];
        for sizes in cases {
            let (inv, names) = blank(sizes);
            for level in [
                RaidLevel::Single, RaidLevel::Raid0, RaidLevel::Raid1, RaidLevel::Raid5, RaidLevel::Raid6,
                RaidLevel::Raid10, RaidLevel::Flex1, RaidLevel::Flex2,
            ] {
                for encrypt in [false, true] {
                    let req = VolumePlanRequest {
                        disks: names.clone(),
                        level,
                        encrypt,
                        compress: true,
                        label: "vol9".into(),
                    };
                    let p = plan_volume(&inv, &req);
                    let c = rynas_api_types::capacity::volume(level, sizes, encrypt, inv.min_tier());
                    assert_eq!(p.valid, c.is_some(), "{level:?} {sizes:?}: {:?}", p.errors);
                    if let Some(c) = c {
                        assert_eq!(
                            (p.usable_bytes, p.raw_bytes, p.wasted_bytes, p.fault_tolerance),
                            (c.usable, c.raw, c.wasted, c.tolerance),
                            "{level:?} {sizes:?} {encrypt}"
                        );
                        // the warnings name the same unused space as the bar
                        let warned: u64 = p
                            .warning_items
                            .iter()
                            .filter(|w| matches!(w.code, PlanWarningCode::MixedSizes | PlanWarningCode::FlexUnusedSpace))
                            .filter_map(|w| w.args.size_bytes)
                            .sum();
                        assert_eq!(warned, c.wasted, "{level:?} {sizes:?}");
                    } else {
                        assert!(!p.issues.is_empty());
                    }
                }
            }
        }
        // near-equal disks: no warning about mixed sizes
        let (inv, names) = blank(&[8 * TB, 8 * TB + 5_000_000, 8 * TB - 5_000_000]);
        let req = VolumePlanRequest { disks: names, level: RaidLevel::Raid5, encrypt: false, compress: true, label: "vol9".into() };
        let p = plan_volume(&inv, &req);
        assert!(!p.warning_items.iter().any(|w| w.code == PlanWarningCode::MixedSizes), "{:?}", p.warnings);
        // RAID10 over five disks: md's near=2 layout
        let (inv, names) = blank(&[4 * TB; 5]);
        let req = VolumePlanRequest { disks: names, level: RaidLevel::Raid10, encrypt: false, compress: true, label: "vol9".into() };
        let p = plan_volume(&inv, &req);
        assert!(p.valid, "{:?}", p.errors);
        assert!(argvs(&p).iter().any(|l| l.contains("--level=10 --raid-devices=5")), "{:?}", argvs(&p));
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
        assert_codes_match(&p);
        assert!(
            p.warning_items
                .iter()
                .any(|w| w.code == PlanWarningCode::ScrubCost)
        );
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
