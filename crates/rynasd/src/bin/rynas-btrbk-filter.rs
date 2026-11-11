//! `rynas-btrbk-filter`: the forced command of every PC key of the `rynas-pcbackup` account
//! (Backup › PCs). sshd runs it with the device's arguments; the command btrbk asked for is
//! in `SSH_ORIGINAL_COMMAND`. Accepted commands run without a shell; every decision goes to
//! syslog (`auth`). See [`rynas_btrfs::receive_filter`].

use std::ffi::CString;
use std::process::{Command, ExitCode, Stdio};

use rynas_api_types::pcbackup::DeletePolicy;
use rynas_btrfs::receive::{delete_refusal, scan_dir};
use rynas_btrfs::receive_filter::{FilterArgs, Plan, Prog, check, locate, parse_args, verify_root};

fn log(prio: libc::c_int, msg: &str) {
    let ident = c"rynas-btrbk-filter";
    let text = CString::new(msg.replace('\0', " ")).unwrap_or_default();
    // SAFETY: static identity, "%s" format with one NUL-terminated argument
    unsafe {
        libc::openlog(ident.as_ptr(), libc::LOG_PID, libc::LOG_AUTH);
        libc::syslog(prio, c"%s".as_ptr(), text.as_ptr());
    }
}

fn refuse(a: Option<&FilterArgs>, why: &str, cmd: &str) -> ExitCode {
    let dev = a.map(|a| a.device.as_str()).unwrap_or("?");
    log(libc::LOG_ERR, &format!("REJECT device={dev}: {why}: {cmd:?}"));
    eprintln!("rynas: refused: {why}");
    ExitCode::from(1)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The NAS rules on the snapshots a delete names, on a fresh listing of the folder.
fn deletes_allowed(a: &FilterArgs, plan: &Plan) -> Result<(), String> {
    if plan.deletes.is_empty() {
        return Ok(());
    }
    let (entries, _) =
        scan_dir(std::path::Path::new(&a.root)).map_err(|e| format!("{}: {e}", a.root))?;
    let policy = DeletePolicy {
        // with --delete-interrupted only interrupted receives may go (delete_refusal lets
        // those through before it looks at `allowed`)
        allowed: a.delete && !a.interrupted_only,
        keep_last: a.keep_last,
        min_age_days: a.min_age_days,
    };
    let now = now_ms();
    for n in &plan.deletes {
        if let Some(why) = delete_refusal(n, &entries, &policy, now) {
            return Err(why);
        }
    }
    Ok(())
}

fn run(plan: &Plan) -> Result<ExitCode, String> {
    let root = unsafe { libc::geteuid() } == 0;
    let sudo = locate("sudo");
    let mut children = Vec::new();
    let mut prev: Option<std::process::ChildStdout> = None;
    let n = plan.stages.len();
    for (i, st) in plan.stages.iter().enumerate() {
        let bin = locate(st.prog.name()).ok_or_else(|| format!("{} is not installed", st.prog.name()))?;
        let mut c = if st.prog == Prog::Btrfs && !root {
            let mut c = Command::new(sudo.as_ref().ok_or("sudo is not installed")?);
            c.arg("-n").arg(&bin);
            c
        } else {
            Command::new(&bin)
        };
        c.args(&st.args)
            .env_clear()
            .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
            .env("LC_ALL", "C");
        c.stdin(match prev.take() {
            Some(p) => Stdio::from(p),
            None => Stdio::inherit(),
        });
        let last = i + 1 == n;
        c.stdout(if last { Stdio::inherit() } else { Stdio::piped() });
        let mut child = c.spawn().map_err(|e| format!("{}: {e}", st.prog.name()))?;
        if !last {
            prev = child.stdout.take();
        }
        children.push(child);
    }
    let mut code = 0;
    for mut ch in children {
        let st = ch.wait().map_err(|e| e.to_string())?;
        if !st.success() {
            code = st.code().unwrap_or(1);
        }
    }
    Ok(ExitCode::from(code.clamp(0, 255) as u8))
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let a = match parse_args(&argv) {
        Ok(a) => a,
        Err(e) => return refuse(None, &format!("bad key line: {e}"), ""),
    };
    let Some(raw) = std::env::var_os("SSH_ORIGINAL_COMMAND") else {
        // `ssh rynas-pcbackup@nas` by hand: the connection test of the setup steps
        log(libc::LOG_INFO, &format!("TEST device={}: login without command", a.device));
        println!(
            "rynas: the key of {} works. This account only accepts btrbk (folder {}).",
            a.device, a.root
        );
        return ExitCode::SUCCESS;
    };
    let Ok(cmd) = raw.into_string() else {
        return refuse(Some(&a), "command is not UTF-8", "");
    };
    let plan = match check(&cmd, &a) {
        Ok(p) => p,
        Err(e) => return refuse(Some(&a), &e, &cmd),
    };
    // the folder itself, checked now (no symlink on the way, not inside a received
    // snapshot), before btrfs runs as root on it
    let mut _lock = None;
    if plan.stages.iter().any(|s| s.prog == Prog::Btrfs) {
        let dir = match verify_root(&a.root) {
            Ok(d) => d,
            Err(e) => return refuse(Some(&a), &e, &cmd),
        };
        if plan.receive {
            // one receive at a time per device (the lock goes with the process)
            use std::os::fd::AsRawFd;
            // SAFETY: valid descriptor
            if unsafe { libc::flock(dir.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return refuse(Some(&a), "another receive of this device is running", &cmd);
            }
        }
        _lock = Some(dir);
    }
    if let Err(e) = deletes_allowed(&a, &plan) {
        return refuse(Some(&a), &e, &cmd);
    }
    log(libc::LOG_INFO, &format!("ACCEPT device={}: {}", a.device, plan.summary));
    match run(&plan) {
        Ok(c) => c,
        Err(e) => {
            log(libc::LOG_ERR, &format!("FAILED device={}: {e}", a.device));
            eprintln!("rynas: {e}");
            ExitCode::from(1)
        }
    }
}
