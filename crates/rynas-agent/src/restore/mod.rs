//! `rynas-agent restore`: bare-metal restore from any live Linux, no ISO (D-PCBACKUP).
//!
//! The TUI of this version does one thing: NAS → fingerprint → restore code → PC → version →
//! target disk(s) → plan → the disk's name typed → restore → reboot. The same steps run
//! without the TUI when every answer is on the command line (tests, scripts):
//! `--nas URL --fingerprint FP --code CODE --version ID|latest --disk /dev/X --confirm X`.
//!
//! Sources: [`restic::ResticSource`] (file method). The btrfs method's source plugs into
//! [`source::RestoreSource`] with [`source::Fill::BtrfsPool`]; until it is built the TUI lists
//! it and says it is not in this build.

pub mod exec;
pub mod plan;
pub mod restic;
pub mod source;
mod tui;

use std::path::Path;

use crate::http::{Client, Endpoint};
use crate::probe;
use source::RestoreSource;

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
        let found = ["/usr/sbin", "/sbin", "/usr/bin", "/bin"].iter().any(|d| Path::new(d).join(&t).is_file());
        if !found {
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
    let scripted = opt(args, "--code").is_some() && opt(args, "--disk").is_some();
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
    let mut src = restic::ResticSource::login(Client::new(ep, &fp), &opt(args, "--code").unwrap_or_default(), opt(args, "--device").as_deref())?;
    let versions = src.versions()?;
    let want = opt(args, "--version").unwrap_or_else(|| "latest".into());
    let v = if want == "latest" {
        versions.first().cloned()
    } else {
        versions.iter().find(|x| x.id.starts_with(&want)).cloned()
    }
    .ok_or("no such version")?;
    eprintln!("PC {}: version {} ({} files)", src.device(), &v.id[..8], v.files);
    let m = src.manifest(&v)?;
    let disk = opt(args, "--disk").unwrap_or_default();
    let disk = disk.trim_start_matches("/dev/").to_string();
    let t = target_disks().into_iter().find(|d| d.name == disk).ok_or_else(|| format!("/dev/{disk}: no such disk"))?;
    if let Some(why) = exec::disk_in_use(&t.name) {
        return Err(format!("/dev/{}: {why}", t.name));
    }
    let src_disk = source_disk(&m).ok_or("the manifest has no disk with a partition table")?;
    let names = vec![t.name.clone()];
    let p = plan::build(&m, &[(src_disk, t.clone())], &in_use_except(&names))?;
    for l in plan::describe(&m, &p) {
        println!("{l}");
    }
    let problems = preflight(&m, &p);
    if !problems.is_empty() {
        return Err(problems.join("\n"));
    }
    if flag(args, "--dry-run") {
        println!("dry run: nothing written");
        return Ok(());
    }
    if opt(args, "--confirm").as_deref() != Some(t.name.as_str()) {
        return Err(format!("--confirm {} is needed: the disk will be erased", t.name));
    }
    let pass = match opt(args, "--passphrase-file") {
        Some(f) => Some(std::fs::read_to_string(&f).map_err(|e| format!("{f}: {e}"))?.trim_end_matches('\n').to_string()),
        None => None,
    };
    let mut log = exec::Log {
        lines: Vec::new(),
        echo: Box::new(|s| eprintln!("{s}")),
    };
    let r = exec::execute(&mut src, &v, &m, &p, &exec::Options { passphrase: pass.as_deref() }, &mut log);
    let tail: String = {
        let t = log.text();
        let cut = t.len().saturating_sub(60 << 10);
        let cut = (cut..t.len()).find(|i| t.is_char_boundary(*i)).unwrap_or(t.len());
        t[cut..].to_string()
    };
    match &r {
        Ok(()) => src.report(&v, true, "restored", &tail),
        Err(e) => src.report(&v, false, e, &tail),
    }
    r
}
