//! `rynas-agent restore`: bare-metal restore from any live Linux, no ISO (D-PCBACKUP).
//!
//! The TUI of this version does one thing: NAS → fingerprint → restore code (or an
//! administrator with TOTP, then the PC from a list) → PC → version →
//! target disk(s) → plan → the disk's name typed → restore → reboot. The same steps run
//! without the TUI when every answer is on the command line (tests, scripts):
//! `--nas URL --fingerprint FP --code CODE --version ID|latest --disk /dev/X --confirm X`.
//!
//! Sources: [`restic::ResticSource`] (file method) and [`btrfs::BtrfsSource`] (btrfs
//! snapshots received by the NAS, with the [`legacy`] reader for snapshots without
//! manifest). A restore code (or the administrator login) opens a session for one PC; the
//! PC has file backups, a folder of btrfs snapshots, or both (`--method files|btrfs`).
//!
//! The btrfs method's scripted options: `--version <run>|latest`, `--mode
//! whole|system|custom|add`, `--subvol NAME[=SNAPSHOT]` (the custom or added subvolumes, and
//! per-subvolume snapshots), `--add-to /dev/X` (the PC's existing btrfs filesystem, for
//! `add`; it must be the PC's own filesystem by UUID unless `--other-filesystem`), `--esp-mib
//! N` (the guided layout of a legacy PC without partition table dump).

pub mod btrfs;
pub mod exec;
pub mod fstab;
pub mod legacy;
pub mod paths;
pub mod plan;
pub mod restic;
pub mod source;
mod tui;

use std::path::Path;
use std::sync::Arc;

use rynas_api_types::pcagent::{RestoreDevice, RestoreLogin, RestoreSession};

use crate::http::{Client, Endpoint};
use crate::probe;
use source::RestoreSource;

/// A restore session from a one-time code: the client with the session's token, and the PC
/// (`device` picks one when the code covers several).
pub fn login(client: Client, code: &str, device: Option<&str>) -> Result<(Arc<Client>, RestoreDevice), String> {
    let s: RestoreSession = client.post_json("/api/v1/pcbackup/agent/restore/login", &RestoreLogin { code: code.to_string() })?;
    let dev = match device {
        Some(d) => s.devices.iter().find(|x| x.device == d).cloned(),
        None => s.devices.first().cloned(),
    }
    .ok_or("the restore code covers no such PC")?;
    Ok((Arc::new(client.with_token(&s.token)), dev))
}

/// The subvolume options of the command line: names (custom, add) and overrides.
fn subvol_args(args: &[String]) -> (Vec<String>, Vec<(String, String)>) {
    let mut names = Vec::new();
    let mut over = Vec::new();
    for (i, a) in args.iter().enumerate() {
        if a == "--subvol"
            && let Some(v) = args.get(i + 1)
        {
            match v.split_once('=') {
                Some((n, s)) => {
                    names.push(n.to_string());
                    over.push((n.to_string(), s.to_string()));
                }
                None => names.push(v.clone()),
            }
        }
    }
    (names, over)
}

/// The btrfs method's own preflight (on top of [`preflight`] when formatting): `btrfs` for
/// the receive when adding (formatting has it in the plan's tools), the boot files (a copy
/// in the snapshot or a known regeneration tool), memory for buffers.
fn btrfs_preflight(m: &rynas_api_types::pcagent::Manifest, src: &btrfs::BtrfsSource, adding: bool) -> (Vec<String>, Vec<String>) {
    let (mut problems, mut notes) = (Vec::new(), Vec::new());
    if adding && !exec::has("btrfs") {
        problems.push(format!("btrfs is missing on this live system: install {}", plan::package_of("btrfs")));
    }
    if m.boot.uefi && src.boot_copies(m).is_empty() && m.boot.initramfs == rynas_api_types::pcagent::InitramfsTool::Unknown && src.legacy.is_none() {
        notes.push("no copy of the boot files in the snapshot and no known initramfs tool: the boot step may need a hand".into());
    }
    if let Some(b) = m.btrbk.as_ref() {
        for c in b.boot_copies.iter().filter(|c| c.stale) {
            notes.push(format!("the copy of {} in the snapshot is an older one (its last copy failed: {})", c.mount, c.error.as_deref().unwrap_or("?")));
        }
    }
    let avail = std::fs::read_to_string("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:").and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok()))
        .unwrap_or(u64::MAX);
    if avail < 256 << 10 {
        problems.push(format!("only {} MiB of memory are free on this live system: the receive needs about 256 MiB", avail >> 10));
    }
    (problems, notes)
}

fn opt(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// Disks that could be targets, with their identity.
pub fn target_disks() -> Vec<plan::TargetDisk> {
    probe::disks()
        .into_iter()
        .map(|b| {
            let (model, serial, _) = probe::identity(&b);
            plan::TargetDisk {
                name: b.name.clone(),
                size_bytes: b.size_bytes,
                logical_sector: b.logical_sector,
                model,
                serial,
            }
        })
        .collect()
}

/// IDs in use on attached devices other than the targets (their partitions may still carry
/// the old IDs: they are about to be wiped).
fn in_use_except(targets: &[String]) -> impl Fn(&str) -> bool + '_ {
    move |id: &str| {
        for dir in ["/dev/disk/by-uuid", "/dev/disk/by-partuuid"] {
            let Ok(rd) = std::fs::read_dir(dir) else { continue };
            for e in rd.flatten() {
                if !e.file_name().to_string_lossy().eq_ignore_ascii_case(id) {
                    continue;
                }
                let dev = std::fs::canonicalize(e.path()).ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default();
                let on_target = targets.iter().any(|t| dev == *t || (dev.starts_with(t.as_str()) && dev[t.len()..].trim_start_matches('p').chars().all(|c| c.is_ascii_digit())));
                if !on_target {
                    return true;
                }
            }
        }
        false
    }
}

/// The PC's disk this version restores: the first with a partition table that holds the root
/// filesystem (directly or in LUKS), else the first with a table.
pub fn source_disk(m: &rynas_api_types::pcagent::Manifest) -> Option<usize> {
    use rynas_api_types::pcagent::FsSource;
    let disk_of = |s: &FsSource| match s {
        FsSource::Partition { disk, .. } => Some(*disk),
        FsSource::Luks { luks } => m.luks.get(*luks).and_then(|l| match l.source {
            FsSource::Partition { disk, .. } => Some(disk),
            _ => None,
        }),
        _ => None,
    };
    m.root()
        .and_then(|r| disk_of(&r.source))
        .filter(|d| m.disks.get(*d).is_some_and(|x| x.table.is_some()))
        .or_else(|| m.disks.iter().position(|d| d.table.is_some()))
}

/// Preflight: the tools the plan needs on the live system, and the firmware mode.
fn preflight(m: &rynas_api_types::pcagent::Manifest, p: &plan::Plan) -> Vec<String> {
    let mut problems = Vec::new();
    for t in plan::tools(p) {
        if !exec::has(&t) {
            problems.push(format!("{t} is missing on this live system: install {}", plan::package_of(&t)));
        }
    }
    let uefi = Path::new("/sys/firmware/efi").exists();
    if m.boot.uefi != uefi {
        problems.push(format!(
            "the PC booted in {} mode, this live system in {}: boot the live system in {} mode",
            if m.boot.uefi { "UEFI" } else { "BIOS" },
            if uefi { "UEFI" } else { "BIOS" },
            if m.boot.uefi { "UEFI" } else { "BIOS" }
        ));
    }
    problems
}

pub fn main(args: &[String]) -> Result<(), String> {
    let scripted = opt(args, "--code").is_some() && (opt(args, "--disk").is_some() || opt(args, "--add-to").is_some());
    if !scripted {
        return tui::run(args);
    }
    let url = opt(args, "--nas").ok_or("--nas is needed")?;
    let ep = Endpoint::parse(&url)?;
    if !ep.tls && !flag(args, "--allow-http") {
        return Err("plain http: add --allow-http on a trusted link".into());
    }
    let fp = opt(args, "--fingerprint").unwrap_or_default();
    if ep.tls && fp.is_empty() {
        return Err("--fingerprint is needed with https (shown on the NAS' PC page)".into());
    }
    let (client, dev) = login(Client::new(ep, &fp), &opt(args, "--code").unwrap_or_default(), opt(args, "--device").as_deref())?;
    let use_btrfs = match opt(args, "--method").as_deref() {
        Some("btrfs") => true,
        Some("files") => false,
        Some(x) => return Err(format!("--method {x}: files or btrfs")),
        None => !dev.has_files(),
    };
    if use_btrfs {
        if !dev.btrfs {
            return Err(format!("{} has no btrfs snapshots on the NAS (--method files)", dev.device));
        }
        return run_btrfs(args, client, dev);
    }
    if !dev.has_files() {
        return Err(format!("{} has no file backups on the NAS (--method btrfs)", dev.device));
    }
    let mut src = restic::ResticSource::from_session(client, dev);
    let versions = src.versions()?;
    let v = source::pick_version(&versions, &opt(args, "--version").unwrap_or_else(|| "latest".into()), true).ok_or("no such version")?;
    eprintln!("PC {}: version {} ({} files)", src.device(), &v.id[..8.min(v.id.len())], v.files);
    let m = src.manifest(&v)?;
    let t = scripted_target(args)?;
    let src_disk = source_disk(&m).ok_or("the manifest has no disk with a partition table")?;
    let names = vec![t.name.clone()];
    let p = plan::build(&m, &[(src_disk, t.clone())], &in_use_except(&names))?;
    for l in plan::describe(&m, &p, false) {
        println!("{l}");
    }
    let problems = preflight(&m, &p);
    if !problems.is_empty() {
        return Err(problems.join("\n"));
    }
    if flag(args, "--dry-run") {
        for l in plan::dry_run_rewrites(&m, &[]) {
            println!("{l}");
        }
        println!("dry run: nothing written");
        return Ok(());
    }
    run_execute(args, &mut src, &v, &m, &p, &t)
}

/// The target disk of the scripted mode (`--disk`), checked to be free.
fn scripted_target(args: &[String]) -> Result<plan::TargetDisk, String> {
    let disk = opt(args, "--disk").unwrap_or_default();
    let disk = disk.trim_start_matches("/dev/").to_string();
    let t = target_disks().into_iter().find(|d| d.name == disk).ok_or_else(|| format!("/dev/{disk}: no such disk"))?;
    if let Some(why) = exec::disk_in_use(&t.name) {
        return Err(format!("/dev/{}: {why}", t.name));
    }
    Ok(t)
}

/// The end of the scripted mode for a formatting restore: the confirmation, the passphrase,
/// the work, the report.
fn run_execute(args: &[String], src: &mut dyn RestoreSource, v: &source::Version, m: &rynas_api_types::pcagent::Manifest, p: &plan::Plan, t: &plan::TargetDisk) -> Result<(), String> {
    if opt(args, "--confirm").as_deref() != Some(t.name.as_str()) {
        return Err(format!("--confirm {} is needed: the disk will be erased", t.name));
    }
    let pass = match opt(args, "--passphrase-file") {
        Some(f) => Some(std::fs::read_to_string(&f).map_err(|e| format!("{f}: {e}"))?.trim_end_matches('\n').to_string()),
        None => None,
    };
    let mut log = exec::Log::new(Box::new(|s| eprintln!("{s}")));
    let r = exec::execute(src, v, m, p, &exec::Options { passphrase: pass.as_deref() }, &mut log);
    finish(src, v, r, &log)
}

/// Reports to the NAS (the end of the log) and says what to check.
fn finish(src: &mut dyn RestoreSource, v: &source::Version, r: Result<(), String>, log: &exec::Log) -> Result<(), String> {
    let text = log.text();
    let cut = text.len().saturating_sub(60 << 10);
    let cut = (cut..text.len()).find(|i| text.is_char_boundary(*i)).unwrap_or(text.len());
    match &r {
        Ok(()) if log.problems.is_empty() => src.report(v, true, "restored", &text[cut..]),
        Ok(()) => src.report(v, true, &format!("restored; check before rebooting: {}", log.problems.join("; ")), &text[cut..]),
        Err(e) => src.report(v, false, e, &text[cut..]),
    }
    r?;
    if !log.problems.is_empty() {
        eprintln!("restored, but check this before rebooting:");
        for p in &log.problems {
            eprintln!("  - {p}");
        }
    }
    Ok(())
}

/// The btrfs method in the scripted mode.
fn run_btrfs(args: &[String], client: Arc<Client>, dev: RestoreDevice) -> Result<(), String> {
    let mut src = btrfs::BtrfsSource::new(client, dev)?;
    if let Some(n) = opt(args, "--esp-mib") {
        src.esp_mib = n.parse().ok().filter(|n| (64..=8192).contains(n)).ok_or("--esp-mib: 64 to 8192")?;
    }
    let versions = src.versions()?;
    let v = source::pick_version(&versions, &opt(args, "--version").unwrap_or_else(|| "latest".into()), false).ok_or("no such run of btrfs snapshots")?;
    eprintln!("PC {}: btrfs snapshots of {}", src.device(), v.id);
    let m = src.manifest(&v)?;
    let mode = match opt(args, "--mode") {
        Some(s) => btrfs::Mode::parse(&s).ok_or("--mode: whole, system, custom or add")?,
        None => btrfs::Mode::Whole,
    };
    let (names, over) = subvol_args(args);
    if mode == btrfs::Mode::Add {
        let device = opt(args, "--add-to").ok_or("--add-to /dev/X: the PC's btrfs filesystem to add to")?;
        let other_ok = flag(args, "--other-filesystem");
        let restored = src.prepare_add(&v, &device, &names, &over, other_ok)?;
        for l in src.describe() {
            println!("{l}");
        }
        let (problems, notes) = btrfs_preflight(&m, &src, true);
        for n in notes {
            println!("note: {n}");
        }
        if !problems.is_empty() {
            return Err(problems.join("\n"));
        }
        if flag(args, "--dry-run") {
            println!("dry run: nothing written");
            return Ok(());
        }
        let name = device.trim_start_matches("/dev/").to_string();
        if opt(args, "--confirm").as_deref() != Some(name.as_str()) {
            return Err(format!("--confirm {name} is needed: subvolumes are added to that filesystem"));
        }
        let mut log = exec::Log::new(Box::new(|s| eprintln!("{s}")));
        let r = exec::execute_add(&mut src, &v, &m, &device, &restored, other_ok, &mut log);
        return finish(&mut src, &v, r, &log);
    }
    src.choose(&v, mode, &names, &over, &|_| false)?;
    let t = scripted_target(args)?;
    let pm = src.planned_manifest(&m, t.logical_sector);
    let src_disk = source_disk(&pm).ok_or("the manifest has no disk with a partition table")?;
    let p = plan::build(&pm, &[(src_disk, t.clone())], &in_use_except(std::slice::from_ref(&t.name)))?;
    for l in plan::describe(&pm, &p, true).into_iter().chain(src.describe()) {
        println!("{l}");
    }
    let mut problems = preflight(&pm, &p);
    let (more, notes) = btrfs_preflight(&pm, &src, false);
    problems.extend(more);
    for n in notes {
        println!("note: {n}");
    }
    if !problems.is_empty() {
        return Err(problems.join("\n"));
    }
    if flag(args, "--dry-run") {
        for l in plan::dry_run_rewrites(&pm, &src.skipped_subvolumes()) {
            println!("{l}");
        }
        println!("dry run: nothing written");
        return Ok(());
    }
    run_execute(args, &mut src, &v, &pm, &p, &t)
}

/// `rynas-agent btrfs-restore-pool` (hidden, for tests on throwaway filesystems): a restore
/// session from a code, then only the btrfs source's pool step (receive, checks, writable
/// subvolumes, the snapshot folder, the subvolumes made empty) into the top level of a btrfs
/// filesystem already mounted at `--pool`, then the fixes of the restored root's
/// `/etc/fstab`. No disk is partitioned or formatted, nothing is mounted, no boot step.
pub fn pool_test(args: &[String]) -> Result<(), String> {
    let url = opt(args, "--nas").ok_or("--nas is needed")?;
    let ep = Endpoint::parse(&url)?;
    if !ep.tls && !flag(args, "--allow-http") {
        return Err("plain http: add --allow-http".into());
    }
    let pool = std::path::PathBuf::from(opt(args, "--pool").ok_or("--pool <top level of a mounted btrfs>")?);
    let (client, dev) = login(Client::new(ep, &opt(args, "--fingerprint").unwrap_or_default()), &opt(args, "--code").ok_or("--code")?, opt(args, "--device").as_deref())?;
    let mut src = btrfs::BtrfsSource::new(client, dev)?;
    let versions = src.versions()?;
    let v = source::pick_version(&versions, &opt(args, "--version").unwrap_or_else(|| "latest".into()), false).ok_or("no such run")?;
    let m = src.manifest(&v)?;
    let mode = btrfs::Mode::parse(&opt(args, "--mode").unwrap_or_else(|| "whole".into())).ok_or("--mode")?;
    let (names, over) = subvol_args(args);
    src.choose(&v, mode, &names, &over, &|p| paths::in_pool(&pool, p).is_ok_and(|x| x.exists()))?;
    for l in src.describe() {
        println!("{l}");
    }
    let subvolumes = m.root().map(|f| f.subvolumes.clone()).unwrap_or_default();
    let pools = vec![source::PoolTarget { uuid: m.root().map(|f| f.uuid.clone()).unwrap_or_default(), top_level: pool.clone(), subvolumes: subvolumes.clone() }];
    let mut log = exec::Log::new(Box::new(|s| eprintln!("{s}")));
    let r = src.fill_pool(&v, &pools, &|_, _| {}, &mut log);
    let skipped = src.skipped_subvolumes();
    let r = r.and_then(|()| if mode.formats() { exec::finish_pool(&mut log, &pool, &subvolumes, &skipped) } else { Ok(()) });
    // the restored root's fstab gets the restore's text fixes (no IDs change here)
    let root_path = m.btrbk.as_ref().and_then(|b| b.subvolumes.iter().find(|s| s.mount == "/")).map(|s| s.path.clone());
    let r = r.and_then(|()| match root_path {
        Some(rp) if mode.formats() => {
            exec::rewrite_in(&mut log, &paths::in_pool(&pool, &rp)?, "/etc/fstab", &|t| plan::fix_fstab(t, &m, &skipped, &[]));
            Ok(())
        }
        _ => Ok(()),
    });
    finish(&mut src, &v, r, &log)?;
    println!("pool filled from run {}", v.id);
    Ok(())
}
