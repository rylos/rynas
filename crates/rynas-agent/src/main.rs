//! `rynas-agent`: the PC side of rynas' PC backup, file method (D-PCBACKUP).
//!
//! ```text
//! rynas-agent pair <nas address> <code> [--fingerprint AA:BB…] [--yes] [--allow-http]
//! rynas-agent run                 the service (systemd unit rynas-agent.service)
//! rynas-agent backup              one backup now, in the foreground
//! rynas-agent status              pairing, plan, last backup
//! rynas-agent manifest [--print]  writes /.rynas/manifest.json (or prints it)
//! rynas-agent btrbk-setup ...     the btrfs method: btrbk.conf, key and manifest hooks
//! rynas-agent restore [...]       bare-metal restore from a live system (TUI)
//! ```
//!
//! The only local state is the pairing (`/var/lib/rynas-agent/state.json`); plans come from the
//! NAS. Everything runs as root (snapshots, freezes, block devices).

mod backend;
mod backup;
mod btrbk;
mod btrfs;
mod freeze;
// the whole GPT reader/writer of abb-restore: the btrfs method's restore will use the rest
#[allow(dead_code)]
mod gpt;
mod http;
mod lvm;
mod manifest;
mod mounts;
mod probe;
mod restore;
mod scheduler;
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
  rynas-agent btrbk-setup --nas ssh://HOST[:PORT]/PATH --device NAME [--retention nas|pc]
                          [--dry-run] [--yes]
  rynas-agent restore [--nas URL] [--code CODE] [--fingerprint FP] [--version ID]
                      [--disk /dev/X] [--confirm X] [--allow-http] [--dry-run]
  rynas-agent --version";

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn opt(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn positional(args: &[String]) -> Vec<String> {
    let with_value = ["--fingerprint", "--nas", "--code", "--version", "--disk", "--confirm", "--passphrase-file", "--device", "--retention", "--root"];
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
    let mut m = manifest::collect(&BTreeMap::new());
    // the btrfs method: the btrbk block and the boot copies (never for --print)
    if let Ok(conf) = std::fs::read_to_string(root.join(btrbk::CONF)) {
        for p in manifest::add_btrbk(&mut m, &mounts::read(), &conf, root, !print) {
            eprintln!("rynas-agent: {p}");
        }
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
            println!("{} copied to {} ({} files, {} bytes)", c.mount, c.dir, c.files, c.bytes);
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

/// `btrbk-setup`: this PC's btrbk for the NAS' btrfs method (`docs/research/pc-backup.md`
/// §11.2.5): `btrbk.conf` from the real subvolumes, the key, the manifest hooks, one
/// manifest. Nothing that exists is replaced without a diff and a confirmation (`--yes`),
/// and the old file is kept. `--root DIR` writes under DIR instead of `/` (tests).
fn btrbk_setup(args: &[String]) -> Result<(), String> {
    let dry = flag(args, "--dry-run");
    if !dry {
        root_only()?;
    }
    let root = std::path::PathBuf::from(opt(args, "--root").unwrap_or_else(|| "/".into()));
    let live = root == std::path::Path::new("/");
    let target = opt(args, "--nas").ok_or(USAGE)?;
    let device = opt(args, "--device").ok_or(USAGE)?;
    let nas_retention = match opt(args, "--retention").as_deref() {
        None | Some("nas") => true,
        Some("pc") => false,
        Some(o) => return Err(format!("--retention {o}: nas or pc")),
    };
    let agent = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/usr/bin/rynas-agent".into());
    let all = mounts::read();
    let layout = btrbk::layout(&all)?;
    let existing = std::fs::read_to_string(root.join(btrbk::CONF)).map(|t| btrbk::parse_conf(&t)).unwrap_or_default();
    let o = btrbk::Opts { device: device.clone(), target: target.clone(), nas_retention, agent: agent.clone() };
    let plan = btrbk::plan(&o, &layout, &existing, &all, btrbk::Distro::detect(&root))?;
    println!("Subvolumes of this PC:");
    for c in &layout.subvolumes {
        match (c.skip, plan.sent.iter().find(|s| s.path == c.path)) {
            (None, Some(s)) => println!("  send      {:<24} {} (snapshots named {})", c.mount, c.path, s.name),
            (Some(why), _) => println!("  not sent  {:<24} {} ({why})", c.mount, c.path),
            _ => {}
        }
    }
    println!("Local snapshots in {}", plan.snapshot_dir);
    let changes = btrbk::changes(&plan.files, &root);
    for c in &changes {
        let p = root.join(&c.file.rel);
        match &c.old {
            Some(old) => println!("\n--- {} (changes)\n{}", p.display(), btrbk::diff(old, &c.file.content)),
            None => println!("\n--- {} (new)\n{}", p.display(), c.file.content),
        }
    }
    if changes.is_empty() {
        println!("\nThe configuration and the hooks are already in place.");
    }
    for n in &plan.notes {
        println!("note: {n}");
    }
    let key = root.join(btrbk::KEY);
    let pubkey = std::path::PathBuf::from(format!("{}.pub", key.display()));
    if dry {
        if !pubkey.exists() {
            println!("would create the key {}", key.display());
        }
        println!("dry run: nothing written");
        return Ok(());
    }
    if changes.iter().any(btrbk::Change::replaces_foreign) && !flag(args, "--yes") {
        // SAFETY: plain query of a descriptor
        if unsafe { libc::isatty(0) } != 1 {
            return Err("existing files would change (diff above): run again with --yes to replace them (the old ones are kept)".into());
        }
        if !ask("Replace the files above? The old ones are kept as .bak [y/N] ").eq_ignore_ascii_case("y") {
            return Err("nothing changed".into());
        }
    }
    let stamp = now_ms().to_string();
    for b in btrbk::apply(&changes, &root, &stamp)? {
        println!("kept the old file as {}", b.display());
    }
    let snap = root.join(plan.snapshot_dir.trim_start_matches('/'));
    std::fs::create_dir_all(&snap).map_err(|e| format!("{}: {e}", snap.display()))?;
    if !pubkey.exists() {
        let dir = key.parent().unwrap_or(&root);
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let _ = std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
        let ok = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", &format!("btrbk@{device}"), "-f"])
            .arg(&key)
            .status()
            .map_err(|e| format!("ssh-keygen: {e} (install openssh)"))?;
        if !ok.success() {
            return Err("ssh-keygen failed".into());
        }
    }
    if live {
        let _ = std::process::Command::new("systemctl").arg("daemon-reload").status();
        manifest_cmd(&["rynas-agent".into(), "manifest".into()])?;
    }
    let line = std::fs::read_to_string(&pubkey).map_err(|e| format!("{}: {e}", pubkey.display()))?;
    let (host, port) = btrbk::target_host(&target);
    let p = port.filter(|p| *p != 22).map(|p| format!(" -p {p}")).unwrap_or_default();
    println!("\n1. Paste this public key into rynas (Backup, PCs: btrfs snapshots, {device}, Settings):\n\n{}\n", line.trim());
    println!("2. Then, on this PC:");
    println!("   sudo ssh -i /{}{p} {}@{host}     (accept the NAS key once)", btrbk::KEY, btrbk::ACCOUNT);
    println!("   sudo btrbk -v dryrun");
    println!("   sudo btrbk -v run");
    println!("   sudo systemctl enable --now btrbk.timer");
    Ok(())
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
        "btrbk-setup" => btrbk_setup(&args),
        "restore" => root_only().and_then(|_| restore::main(&args)),
        _ => Err(USAGE.into()),
    };
    if let Err(e) = r {
        eprintln!("rynas-agent: {e}");
        std::process::exit(1);
    }
}
