//! `rynas-agent`: the PC side of rynas' PC backup, file method (D-PCBACKUP).
//!
//! ```text
//! rynas-agent pair <nas address> <code> [--fingerprint AA:BB…] [--yes] [--allow-http]
//! rynas-agent run                 the service (systemd unit rynas-agent.service)
//! rynas-agent backup              one backup now, in the foreground
//! rynas-agent status              pairing, plan, last backup
//! rynas-agent set-server <url>    a new NAS address, checked with the pinned CA and identity
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
// reading ahead between the stages of a stream, shared with rynasd the same way
#[path = "../../rynas-btrfs/src/readahead.rs"]
#[allow(dead_code)]
mod readahead;
mod state;
mod trust;

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
  rynas-agent set-server <nas address> [--allow-http]
                   (the NAS moved to an address this PC does not know: no pairing again)
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
        let probe = Client::probe(ep.clone());
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
    let mut st = State {
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
    // the NAS' certificate authority and identity, sent over the connection the code and the
    // fingerprint authenticate: pinned once a connection trusting only them works. Never from
    // an answer over plain http: anyone on the link could have written it.
    if !ep.tls {
        eprintln!("paired over plain http: this PC trusts no certificate of the NAS; pair it again over https for a NAS that can change its name or address");
    } else if let Some(t) = &r.trust {
        let proved = trust::check(t).and_then(|c| {
            let eps = trust::endpoints(&ep.url(), &c.addresses)?;
            trust::prove(&c, eps, &st.token)
        });
        match proved {
            Ok((at, now)) => {
                st.nas_id = now.nas_id;
                st.cas = now.cas;
                st.addresses = now.addresses;
                st.trust_checked_ms = Some(now_ms());
                st.url = at;
                st.fingerprint.clear();
            }
            Err(e) => eprintln!("the NAS' certificate authority could not be checked ({e}): its certificate stays pinned, the service tries again later"),
        }
    }
    st.save()?;
    println!("paired with {} as {} (group {})", if r.nas_name.is_empty() { ep.url() } else { r.nas_name }, r.device, r.group);
    println!("start the service: systemctl enable --now rynas-agent.service");
    Ok(())
}

/// One backup (in the foreground, or as the service's child with `--service`).
fn backup_cmd(args: &[String]) -> Result<(), String> {
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
    // only what this run changes: the service may refresh the address or the trust meanwhile
    let _ = State::update(|s| {
        s.plan = st.plan.clone();
        s.last_attempt_ms = st.last_attempt_ms;
    });
    let run = backup::run(&st, scheduler::trigger_of(opt(args, "--trigger").as_deref()));
    // saved after the thaw (never during a freeze)
    let _ = State::update(|s| {
        if run.ok {
            s.last_ok_ms = Some(run.ended_ms);
            s.failures = 0;
        } else {
            s.failures += 1;
        }
    });
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

/// The hidden test commands run only with `RYNAS_AGENT_STATE` set to a folder other than the
/// service's: a test's own pairing, never the PC's.
fn test_state_only() -> Result<(), String> {
    match std::env::var_os("RYNAS_AGENT_STATE") {
        Some(d) if std::path::Path::new(&d) != std::path::Path::new(state::STATE_DIR) => Ok(()),
        _ => Err("a test command: set RYNAS_AGENT_STATE to a test state folder".into()),
    }
}

/// Hidden, tests (`schedule-test`): a stand-in for a backup run. It reports a good run with
/// its trigger after two seconds and keeps the outcome in the state as a backup does; it
/// reads and sends nothing of the PC.
fn test_run(args: &[String]) -> Result<(), String> {
    let st = State::load()?;
    let started_ms = now_ms();
    let _ = State::update(|s| s.last_attempt_ms = Some(started_ms));
    std::thread::sleep(std::time::Duration::from_secs(2));
    let run = rynas_api_types::pcagent::PcRun {
        run_id: format!("run-{started_ms}"),
        started_ms,
        ended_ms: now_ms(),
        ok: true,
        trigger: Some(scheduler::trigger_of(opt(args, "--trigger").as_deref())),
        ..Default::default()
    };
    let r = rynas_api_types::pcagent::AgentReport { run: run.clone(), manifest: None, agent_version: env!("CARGO_PKG_VERSION").into(), root_btrfs: None };
    backup::client_of(&st)?.post_json::<_, serde_json::Value>("/api/v1/pcbackup/agent/report", &r)?;
    let _ = State::update(|s| {
        s.last_ok_ms = Some(run.ended_ms);
        s.failures = 0;
    });
    println!("test run reported");
    Ok(())
}

fn status() -> Result<(), String> {
    let st = State::load()?;
    println!("NAS          {} ({})", st.url, st.nas_name);
    println!("PC           {} (group {})", st.device, st.group);
    if st.nas_id.is_empty() && st.fingerprint.is_empty() {
        println!("trust        none (paired over plain http; pair again over https to trust the NAS' certificate authority)");
    } else if st.nas_id.is_empty() {
        println!("trust        the NAS certificate {} (pinned at pairing; replaced by the NAS' certificate authority at the next contact with a NAS that has it)", st.fingerprint);
    } else {
        println!("trust        the NAS' certificate authority, NAS identity {}", st.nas_id);
        if !st.addresses.is_empty() {
            println!("addresses    {}", st.addresses.join(", "));
        }
    }
    let p = &st.plan.plan;
    let sched = st.plan.schedule();
    println!(
        "plan         every {} h{}, {}{}",
        sched.every_h,
        if sched.window.is_some() { " inside the allowed window" } else { "" },
        if p.freeze == FreezeMode::Freeze { format!("freeze up to {} s", p.freeze_max_secs) } else { "read in use".into() },
        if p.paused { ", paused" } else { "" }
    );
    let when = |ms: Option<u64>| ms.map(|m| format!("{} min ago", now_ms().saturating_sub(m) / 60_000)).unwrap_or_else(|| "never".into());
    println!("last good    {}", when(st.last_ok_ms));
    println!("last attempt {}{}", when(st.last_attempt_ms), if st.failures > 0 { format!(" ({} failed in a row)", st.failures) } else { String::new() });
    if let Some(d) = scheduler::next_due(&st, now_ms(), now_ms()) {
        let left = d.saturating_sub(now_ms()) / 60_000;
        println!("next         in about {left} min (when the service runs)");
    }
    Ok(())
}

/// `set-server <url>`: the NAS answers at an address this PC does not know (its name and
/// addresses all changed). Checked as every connection is ([`trust::set_server`]), then
/// stored first and the service restarted.
fn set_server(args: &[String]) -> Result<(), String> {
    root_only()?;
    let pos = positional(args);
    let url = pos.get(1).ok_or(USAGE)?;
    let ep = Endpoint::parse(url)?;
    if !ep.tls && !flag(args, "--allow-http") {
        return Err("plain http sends the token unprotected: use https, or --allow-http on a trusted link".into());
    }
    let mut st = State::load()?;
    for note in trust::set_server(&mut st, &ep)? {
        eprintln!("{note}");
    }
    st.save()?;
    println!("NAS address: {}", st.url);
    // the service holds the old address in memory
    let restarted = ["/usr/bin/systemctl", "/bin/systemctl"]
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .is_some_and(|systemctl| {
            std::process::Command::new(systemctl)
                .args(["try-restart", "rynas-agent.service"])
                .stdin(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        });
    if !restarted {
        println!("restart the service if it runs: systemctl restart rynas-agent.service");
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
        "run" => root_only().and_then(|_| scheduler::run_service(scheduler::Mode::default())),
        // hidden, tests: the service's schedule against a NAS, with a stand-in for each run
        // (`test-run`: it reports a run and backs nothing up), for `--for` seconds; the state
        // is `$RYNAS_AGENT_STATE`
        // (only with a state of its own: never against the paired PC's real state, where a
        // stand-in run would tell the NAS of a backup that never happened)
        "schedule-test" => test_state_only().and_then(|_| scheduler::run_service(scheduler::Mode { fake: true, for_s: Some(opt(&args, "--for").and_then(|s| s.parse().ok()).unwrap_or(60)) })),
        "test-run" => test_state_only().and_then(|_| test_run(&args)),
        "backup" => backup_cmd(&args),
        "status" => status(),
        "set-server" => set_server(&args),
        "manifest" => manifest_cmd(&args),
        "btrfs-send" => btrfs_send_cmd(&args),
        // hidden, tests: what a run sends of the subvolumes nested in the given ones
        "btrfs-nested" => root_only().and_then(|_| send::nested_test_command(std::path::Path::new(&opt(&args, "--root").unwrap_or_else(|| "/".into())), &subvol_args(&args)?)),
        // hidden, tests: the btrfs restore's pool step into a mounted throwaway filesystem
        "btrfs-restore-pool" => root_only().and_then(|_| restore::pool_test(&args)),
        "restore" => root_only().and_then(|_| restore::main(&args)),
        // hidden, tests: a restore's writing phase when the terminal closes (restore::hangup)
        "hangup-test" => restore::hangup::test_command(&args),
        _ => Err(USAGE.into()),
    };
    if let Err(e) = r {
        eprintln!("rynas-agent: {e}");
        std::process::exit(1);
    }
}
