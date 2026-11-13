//! `rynas-agent`: the PC side of rynas' PC backup, file method (D-PCBACKUP).
//!
//! ```text
//! rynas-agent pair <nas address> <code> [--fingerprint AA:BB…] [--yes] [--allow-http]
//! rynas-agent run                 the service (systemd unit rynas-agent.service)
//! rynas-agent backup              one backup now, in the foreground
//! rynas-agent status              pairing, plan, last backup
//! rynas-agent manifest [--print]  writes /.rynas/manifest.json (or prints it)
//!                                 (PCs that push with btrbk: run it before each btrbk run)
//! rynas-agent restore [...]       bare-metal restore from a live system (TUI)
//! ```
//!
//! The only local state is the pairing (`/var/lib/rynas-agent/state.json`); plans come from the
//! NAS. Everything runs as root (snapshots, freezes, block devices).

mod backend;
mod backup;
mod bootcopy;
mod btrbk;
mod btrfs;
mod freeze;
// the whole GPT reader/writer of abb-restore: the btrfs method's restore will use the rest
#[allow(dead_code)]
mod gpt;
mod http;
// the NAS' btrfs ioctls on descriptors (rynas-btrfs, std + libc only), shared as a source
// file like the stream guard below
#[path = "../../rynas-btrfs/src/ioctl.rs"]
#[allow(dead_code)]
mod ioctl;
mod lvm;
mod manifest;
mod mounts;
mod probe;
mod restore;
mod scheduler;
mod send;
// the NAS' own guard for `btrfs send` streams (rynas-btrfs), shared as a source file: the
// restore checks the streams it receives from the NAS the way the NAS checks the PC's
#[path = "../../rynas-btrfs/src/send_stream.rs"]
#[allow(dead_code)]
mod send_stream;
mod state;

use std::collections::BTreeMap;
use std::io::{BufRead, Write};

use rynas_api_types::pcagent::*;

use crate::http::{Client, Endpoint};
use crate::state::{State, now_ms};

const USAGE: &str = "usage:
  rynas-agent pair <nas address> <code> [--fingerprint AA:BB:..] [--yes] [--allow-http]
                   (--yes over https needs --fingerprint)
  rynas-agent run
  rynas-agent backup
  rynas-agent status
  rynas-agent manifest [--print] [--quiet]
  rynas-agent restore [--nas URL] [--code CODE] [--fingerprint FP] [--version ID]
                      [--disk /dev/X] [--confirm X] [--allow-http] [--dry-run]
                      [--method files|btrfs] [--mode whole|system|custom|add]
                      [--subvol NAME[=SNAPSHOT]]... [--add-to /dev/X] [--esp-mib N]
                      [--passphrase-file F] [--no-nvram]
  rynas-agent --version";

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn opt(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn positional(args: &[String]) -> Vec<String> {
    let with_value = [
        "--fingerprint",
        "--nas",
        "--code",
        "--version",
        "--disk",
        "--confirm",
        "--passphrase-file",
        "--token",
        "--snapshot-dir",
        "--subvol",
        "--method",
        "--mode",
        "--add-to",
        "--esp-mib",
        "--pool",
        "--device",
    ];
    let mut out = Vec::new();
    let mut skip = false;
    // the program's own name is not an argument
    for a in args.iter().skip(1) {
        if skip {
            skip = false;
            continue;
        }
        if with_value.contains(&a.as_str()) {
            skip = true;
            continue;
        }
        if !a.starts_with("--") {
            out.push(a.clone());
        }
    }
    out
}

fn root_only() -> Result<(), String> {
    // SAFETY: plain syscall
    if unsafe { libc::geteuid() } != 0 {
        return Err("run rynas-agent as root (sudo)".into());
    }
    Ok(())
}

fn ask(q: &str) -> String {
    eprint!("{q}");
    let _ = std::io::stderr().flush();
    let mut s = String::new();
    let _ = std::io::stdin().lock().read_line(&mut s);
    s.trim().to_string()
}

/// The root filesystem is btrfs: the btrfs method is the better fit (equal choice, the NAS
/// receives `btrfs send` streams; set up from the NAS' PC page).
fn btrfs_root() -> bool {
    mounts::read().iter().rev().find(|m| m.path == "/").is_some_and(|m| m.fstype == "btrfs")
}

fn pair(args: &[String]) -> Result<(), String> {
    root_only()?;
    let pos = positional(args);
    let (Some(url), Some(code)) = (pos.get(1), pos.get(2)) else {
        return Err(USAGE.into());
    };
    let ep = Endpoint::parse(url)?;
    if !ep.tls && !flag(args, "--allow-http") {
        return Err("plain http sends the token unprotected: use https, or --allow-http on a trusted link".into());
    }
    if btrfs_root() && !flag(args, "--yes") {
        eprintln!("This PC's root filesystem is btrfs. rynas offers two methods for PCs:");
        eprintln!("  - btrfs snapshots: the NAS receives the PC's snapshots (recommended for btrfs);");
        eprintln!("  - file backup: files into a repository on the NAS (this command).");
        if !ask("Continue with the file backup? [y/N] ").eq_ignore_ascii_case("y") {
            return Err("stopped: add the PC with the btrfs snapshots method on the NAS".into());
        }
    }
    // the certificate first: shown, compared, pinned
    let mut pin = String::new();
    if ep.tls {
        let probe = Client::new(ep.clone(), "");
        let _ = probe.request("GET", "/api/v1/auth/session", &[], &[]);
        let seen = probe.seen_fingerprint().ok_or_else(|| format!("{}: no TLS answer", ep.url()))?;
        match opt(args, "--fingerprint") {
            Some(f) if http::normalize_fp(&f) == seen => {}
            Some(f) => return Err(format!("the NAS shows certificate {seen}, not {}", http::normalize_fp(&f))),
            // --yes never accepts an unchecked certificate: anyone in the path could pose as
            // the NAS and take the code and the PC's repository password
            None if flag(args, "--yes") => {
                return Err(format!(
                    "--yes needs --fingerprint over https (the NAS shows certificate {seen}): copy the whole command from the NAS' \"Add a PC\" dialog"
                ));
            }
            None => {
                eprintln!("NAS certificate (SHA-256):\n  {seen}");
                eprintln!("Compare it with the fingerprint in the NAS' \"Add a PC\" dialog.");
                if !ask("Is it the same? [y/N] ").eq_ignore_ascii_case("y") {
                    return Err("not paired".into());
                }
            }
        }
        pin = seen;
    }
    let client = Client::new(ep.clone(), &pin);
    let os = std::fs::read_to_string("/etc/os-release")
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string()))
        .unwrap_or_default();
    let req = PairRequest {
        code: code.clone(),
        hostname: manifest::hostname(),
        machine_id: std::fs::read_to_string("/etc/machine-id").unwrap_or_default().trim().to_string(),
        os,
        arch: std::env::consts::ARCH.into(),
        agent_version: env!("CARGO_PKG_VERSION").into(),
        root_btrfs: Some(btrfs::root_is_btrfs()),
    };
    let r: PairResponse = client.post_json("/api/v1/pcbackup/agent/pair", &req)?;
    let st = State {
        url: ep.url(),
        fingerprint: pin,
        device: r.device.clone(),
        group: r.group.clone(),
        token: r.token,
        repo_password: r.repo_password,
        nas_name: r.nas_name.clone(),
        plan: r.plan,
        ..Default::default()
    };
    st.save()?;
    println!("paired with {} as {} (group {})", if r.nas_name.is_empty() { ep.url() } else { r.nas_name }, r.device, r.group);
    println!("start the service: systemctl enable --now rynas-agent.service");
    Ok(())
}

/// One backup (in the foreground, or as the service's child with `--service`).
fn backup_cmd(_args: &[String]) -> Result<(), String> {
    root_only()?;
    // before any thread: this process gets mounts of its own
    backup::private_mounts()?;
    extern "C" fn on_signal(_: libc::c_int) {
        freeze::thaw_all_from_signal();
        // SAFETY: async-signal-safe exit; snapshots are removed by the next start
        unsafe { libc::_exit(130) };
    }
    // SAFETY: the handler only does async-signal-safe calls
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
    backup::cleanup_leftovers();
    let mut st = State::load()?;
    // the newest plan (excludes, freeze)
    if let Ok(c) = backup::client_of(&st)
        && let Ok(p) = c.get_json::<AgentPlan>("/api/v1/pcbackup/agent/plan")
    {
        st.plan = p;
    }
    st.last_attempt_ms = Some(now_ms());
    let _ = st.save();
    let run = backup::run(&st);
    // saved after the thaw (never during a freeze)
    if run.ok {
        st.last_ok_ms = Some(run.ended_ms);
        st.failures = 0;
    } else {
        st.failures += 1;
    }
    let _ = st.save();
    let changed = if run.changed_count > 0 { format!(", {} files changed while read", run.changed_count) } else { String::new() };
    match &run.error {
        None => {
            println!(
                "version {} saved: {} files, {} bytes read, {} new{changed}",
                run.snapshot.as_deref().map(|s| &s[..s.len().min(8)]).unwrap_or("?"),
                run.files,
                run.bytes_read,
                run.bytes_added
            );
            Ok(())
        }
        Some(e) => Err(format!("backup failed: {e}")),
    }
}

fn status() -> Result<(), String> {
    let st = State::load()?;
    println!("NAS          {} ({})", st.url, st.nas_name);
    println!("PC           {} (group {})", st.device, st.group);
    let p = &st.plan.plan;
    println!(
        "plan         every {} h, {}{}",
        p.interval_hours,
        if p.freeze == FreezeMode::Freeze { format!("freeze up to {} s", p.freeze_max_secs) } else { "read in use".into() },
        if p.paused { ", paused" } else { "" }
    );
    let when = |ms: Option<u64>| ms.map(|m| format!("{} min ago", now_ms().saturating_sub(m) / 60_000)).unwrap_or_else(|| "never".into());
    println!("last good    {}", when(st.last_ok_ms));
    println!("last attempt {}{}", when(st.last_attempt_ms), if st.failures > 0 { format!(" ({} failed in a row)", st.failures) } else { String::new() });
    if let Some(d) = scheduler::next_due(&st, now_ms()) {
        let left = d.saturating_sub(now_ms()) / 60_000;
        println!("next         in about {left} min (when the service runs)");
    }
    Ok(())
}

fn manifest_cmd(args: &[String]) -> Result<(), String> {
    root_only()?;
    let print = flag(args, "--print");
    let quiet = flag(args, "--quiet");
    let root = std::path::Path::new("/");
    // one at a time with the agent's own runs (a backup writes the manifest too)
    let _lock = if print { None } else { Some(manifest::lock(root, std::time::Duration::from_secs(120))?) };
    bootcopy::trigger_automounts(root);
    let mut m = manifest::collect(&BTreeMap::new());
    // PCs that push with btrbk: the btrbk block and the boot copies (never for --print)
    match btrbk::read_conf(root) {
        Ok(Some(conf)) => {
            for p in manifest::add_btrbk(&mut m, &mounts::read(), &conf, root, !print) {
                eprintln!("rynas-agent: {p}");
            }
        }
        Ok(None) => {}
        Err(e) => eprintln!("rynas-agent: {e}: the manifest has no btrbk block"),
    }
    if print {
        println!("{}", serde_json::to_string_pretty(&m).map_err(|e| e.to_string())?);
        return Ok(());
    }
    let p = manifest::write(&m, root)?;
    if quiet {
        return Ok(());
    }
    println!("{} written", p.display());
    if let Some(b) = &m.btrbk {
        for c in &b.boot_copies {
            if c.stale {
                println!("{}: copy failed ({}), the previous copy in {} is kept", c.mount, c.error.as_deref().unwrap_or("?"), c.dir);
            } else {
                println!("{} copied to {} ({} files, {} bytes)", c.mount, c.dir, c.files, c.bytes);
            }
            if !c.skipped.is_empty() {
                println!("  left out as key material: {}", c.skipped.join(", "));
            }
        }
    }
    let reasons = m.ready_reasons();
    if !reasons.is_empty() {
        println!("bare-metal notes: {reasons:?}");
    }
    Ok(())
}

/// The `--subvol A=B` options of the hidden test commands (one or more).
fn subvol_args(args: &[String]) -> Result<Vec<(String, String)>, String> {
    let mut subvols = Vec::new();
    for (i, a) in args.iter().enumerate() {
        if a == "--subvol" {
            let v = args.get(i + 1).ok_or(USAGE)?;
            let (n, p) = v.split_once('=').ok_or("--subvol NAME=PATH")?;
            subvols.push((n.to_string(), p.to_string()));
        }
    }
    if subvols.is_empty() {
        return Err("--subvol NAME=PATH (one or more)".into());
    }
    Ok(subvols)
}

/// `btrfs-send` (hidden, for tests on throwaway filesystems): the btrfs method's snapshot,
/// parent, send and keep steps for the subvolumes named on the command line, to the NAS
/// named on the command line (no pairing state, no manifest, no lock on `/`).
fn btrfs_send_cmd(args: &[String]) -> Result<(), String> {
    root_only()?;
    let nas = opt(args, "--nas").ok_or(USAGE)?;
    let token = opt(args, "--token").ok_or(USAGE)?;
    let dir = opt(args, "--snapshot-dir").ok_or(USAGE)?;
    if !dir.starts_with('/') {
        return Err("--snapshot-dir: an absolute path".into());
    }
    let subvols = subvol_args(args)?;
    let ep = Endpoint::parse(&nas)?;
    if !ep.tls && !flag(args, "--allow-http") {
        return Err("plain http: add --allow-http".into());
    }
    let client = Client::new(ep, &opt(args, "--fingerprint").unwrap_or_default()).with_token(&token);
    send::test_command(&client, &dir, &subvols)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("");
    let r = match cmd {
        "--version" | "-V" | "version" => {
            println!("rynas-agent {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "pair" => pair(&args),
        "run" => root_only().and_then(|_| scheduler::run_service()),
        "backup" => backup_cmd(&args),
        "status" => status(),
        "manifest" => manifest_cmd(&args),
        "btrfs-send" => btrfs_send_cmd(&args),
        // hidden, tests: what a run sends of the subvolumes nested in the given ones
        "btrfs-nested" => root_only().and_then(|_| send::nested_test_command(std::path::Path::new(&opt(&args, "--root").unwrap_or_else(|| "/".into())), &subvol_args(&args)?)),
        // hidden, tests: the btrfs restore's pool step into a mounted throwaway filesystem
        "btrfs-restore-pool" => root_only().and_then(|_| restore::pool_test(&args)),
        "restore" => root_only().and_then(|_| restore::main(&args)),
        _ => Err(USAGE.into()),
    };
    if let Err(e) = r {
        eprintln!("rynas-agent: {e}");
        std::process::exit(1);
    }
}
