//! The SSH service (D-SSH, `docs/phase2/ssh.md`): OpenSSH's sshd configured through one
//! drop-in, `/etc/ssh/sshd_config.d/00-rynas.conf`, which is also the saved state (no other
//! copy of the settings: the file is read back, a hand edit shows as drift).
//!
//! An apply:
//! 1. checks the settings, the port against the listening sockets, and that an
//!    administrator who can log in now still can afterwards ([`Ssh::apply`], `ssh_lockout`);
//! 2. checks the candidate with `sshd -t -f` on a copy of `sshd_config` whose `Include`
//!    lines name the candidate where the drop-in will be ([`config::candidate_main`]);
//! 3. records what to go back to (`<state>/ssh/trial.json`) and, for a change that can shut
//!    someone out, arms `rynas-ssh-rollback.timer` (`rynasctl ssh rollback`): it outlives
//!    rynasd, and rynasd also rolls back an expired trial itself ([`Ssh::expire`]);
//! 4. writes the drop-in, brings the units to the wanted state (`ssh.service`; socket
//!    activation is switched off) and reloads sshd: existing sessions stay;
//! 5. checks that sshd runs, else goes back at once.
//!
//! Every writer (rynasd, `rynasctl ssh rollback`) takes `<state>/ssh/lock`.

pub mod config;
pub mod fake;
pub mod keys;
pub mod logins;

use std::collections::BTreeSet;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rynas_api_types::ssh::*;
use serde::{Deserialize, Serialize};

pub const SERVICE: &str = "ssh.service";
pub const SOCKET: &str = "ssh.socket";
pub const ROLLBACK_UNIT: &str = "rynas-ssh-rollback";
pub const RYNASCTL: &str = "/usr/libexec/rynas/rynasctl";
const SSHD: &str = "/usr/sbin/sshd";
const PASSWD: &str = "/etc/passwd";
const GROUP: &str = "/etc/group";
const ADMIN_GROUPS: [&str; 2] = [rynas_api_types::ADMIN_GROUP, "sudo"];

/// What a command printed.
#[derive(Debug, Clone, Default)]
pub struct Out {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

/// Runs sshd, systemctl, systemd-run, journalctl.
pub trait SshRunner: Send + Sync {
    fn run(&self, argv: &[&str]) -> std::result::Result<Out, String>;
    /// A stand-in (tests, dry system): no wait for sshd to come up.
    fn simulated(&self) -> bool {
        false
    }
}

/// The machine.
pub struct HostRunner;

impl SshRunner for HostRunner {
    fn run(&self, argv: &[&str]) -> std::result::Result<Out, String> {
        let (p, rest) = argv.split_first().ok_or("empty command")?;
        let c = rynas_tasks::Cmd::new(p).args(rest.iter().copied()).timeout(Duration::from_secs(60));
        let shown = c.display();
        let o = c.run_blocking().map_err(|e| format!("{shown}: {e}"))?;
        Ok(Out { code: o.code, stdout: o.stdout, stderr: o.stderr })
    }
}

#[derive(Debug)]
pub enum SshError {
    /// 400 `invalid_value` about a field of the request.
    Invalid(&'static str, String),
    /// 409 `ssh_lockout`.
    Lockout(String),
    /// 409 `conflict`.
    Conflict(String),
    /// 404.
    NotFound(String),
    /// The change failed and was undone (500).
    Failed(String),
}

impl std::fmt::Display for SshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SshError::Invalid(_, m)
            | SshError::Lockout(m)
            | SshError::Conflict(m)
            | SshError::NotFound(m)
            | SshError::Failed(m) => f.write_str(m),
        }
    }
}

pub type Result<T> = std::result::Result<T, SshError>;

/// State of the two units sshd can run from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Units {
    pub service_enabled: bool,
    pub service_active: bool,
    pub socket_enabled: bool,
    pub socket_active: bool,
}

impl Units {
    fn wanted(enabled: bool) -> Units {
        Units { service_enabled: enabled, service_active: enabled, socket_enabled: false, socket_active: false }
    }
    fn on(&self) -> bool {
        self.service_enabled || self.socket_enabled
    }
    fn running(&self) -> bool {
        self.service_active || self.socket_active
    }
}

/// `<state>/ssh/trial.json`: what a change goes back to.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Trial {
    token: String,
    owner: String,
    deadline_ms: u64,
    timeout_s: u32,
    port: u16,
    /// The drop-in before the change (`None`: there was none).
    previous: Option<String>,
    units: Units,
}

/// A local account as `/etc/passwd` and `/etc/group` describe it.
#[derive(Debug, Clone)]
struct Account {
    name: String,
    uid: u32,
    gid: u32,
    home: String,
    shell: String,
    groups: Vec<String>,
}

pub struct Ssh {
    root: PathBuf,
    state: PathBuf,
    run: Arc<dyn SshRunner>,
    rynasctl: String,
}

fn under(root: &Path, p: &str) -> PathBuf {
    root.join(p.trim_start_matches('/'))
}

fn now_ms() -> u64 {
    rynas_tasks::now_ms()
}

/// An exclusive `flock`, released on drop.
struct Lock(#[allow(dead_code)] std::fs::File);

/// The accounts that can hold keys: root and the local users.
fn keyed(uid: u32) -> bool {
    uid == 0 || (rynas_api_types::LOCAL_ID_MIN..=rynas_api_types::LOCAL_ID_MAX).contains(&uid)
}

fn valid_group(g: &str) -> bool {
    !g.is_empty()
        && g.len() <= 32
        && g.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && g.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.-".contains(c))
}

fn shell_level(a: &Account) -> SshLevel {
    let s = a.shell.rsplit('/').next().unwrap_or("");
    if s.is_empty() || s == "nologin" || s == "false" {
        // ForceCommand internal-sftp needs no shell
        if a.groups.iter().any(|g| g == SSH_SFTP_GROUP) { SshLevel::Sftp } else { SshLevel::None }
    } else if a.groups.iter().any(|g| g == SSH_SFTP_GROUP) {
        SshLevel::Sftp
    } else {
        SshLevel::Shell
    }
}

pub use rynas_api_types::ssh::{admin_can_log_in, passes};

fn token() -> String {
    let mut b = [0u8; 18];
    let _ = getrandom::fill(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Ports in `LISTEN` state in `/proc/net/tcp` text.
pub fn listening(proc_tcp: &str) -> BTreeSet<u16> {
    proc_tcp
        .lines()
        .skip(1)
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let local = w.nth(1)?;
            let st = w.nth(1)?;
            (st == "0A").then(|| u16::from_str_radix(local.rsplit(':').next()?, 16).ok())?
        })
        .collect()
}

impl Ssh {
    /// `root`: where `/etc`, `/proc` and the homes are ("/" on the machine); `state`: rynasd's
    /// state directory (the engine uses `<state>/ssh`).
    pub fn new(root: impl Into<PathBuf>, state: impl Into<PathBuf>, run: Arc<dyn SshRunner>) -> Self {
        Ssh { root: root.into(), state: state.into().join("ssh"), run, rynasctl: RYNASCTL.into() }
    }

    pub fn with_rynasctl(mut self, path: &str) -> Self {
        self.rynasctl = path.into();
        self
    }

    fn path(&self, p: &str) -> PathBuf {
        under(&self.root, p)
    }

    fn read(&self, p: &str) -> Option<String> {
        std::fs::read_to_string(self.path(p)).ok()
    }

    fn list_dir(&self, dir: &str) -> Vec<String> {
        std::fs::read_dir(self.path(dir))
            .map(|rd| rd.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect())
            .unwrap_or_default()
    }

    fn dropin_path(&self) -> PathBuf {
        self.path(&format!("{}/{}", config::DROPIN_DIR, config::DROPIN_NAME))
    }

    fn lock(&self) -> Result<Lock> {
        let io = |e: std::io::Error| SshError::Failed(format!("{}: {e}", self.state.display()));
        std::fs::create_dir_all(&self.state).map_err(io)?;
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.state.join("lock"))
            .map_err(io)?;
        // SAFETY: flock on a descriptor we own
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } < 0 {
            return Err(io(std::io::Error::last_os_error()));
        }
        Ok(Lock(f))
    }

    fn run(&self, argv: &[&str]) -> Out {
        self.run.run(argv).unwrap_or_else(|e| Out { code: None, stdout: String::new(), stderr: e })
    }

    fn installed(&self) -> bool {
        self.path(SSHD).exists()
    }

    fn units(&self) -> Units {
        let enabled = |u: &str| self.run(&["systemctl", "is-enabled", u]).stdout.trim().starts_with("enabled");
        let active = |u: &str| self.run(&["systemctl", "is-active", u]).stdout.trim() == "active";
        Units {
            service_enabled: enabled(SERVICE),
            service_active: active(SERVICE),
            socket_enabled: enabled(SOCKET),
            socket_active: active(SOCKET),
        }
    }

    /// Brings the units from `from` to `to`; sshd reloads when it keeps running (new
    /// configuration, sessions stay). Errors are collected, not fatal (rollback goes on).
    fn set_units(&self, from: Units, to: Units) -> Vec<String> {
        let mut errs = Vec::new();
        let mut sc = |args: &[&str]| {
            let mut argv = vec!["systemctl"];
            argv.extend_from_slice(args);
            let o = self.run(&argv);
            if !o.ok() {
                errs.push(format!("{}: {}", argv.join(" "), o.stderr.trim()));
            }
        };
        if from.socket_active && !to.socket_active {
            sc(&["stop", SOCKET]);
        }
        if from.socket_enabled && !to.socket_enabled {
            sc(&["disable", SOCKET]);
        }
        if from.service_active && !to.service_active {
            sc(&["stop", SERVICE]);
        }
        if from.service_enabled != to.service_enabled {
            sc(&[if to.service_enabled { "enable" } else { "disable" }, SERVICE]);
        }
        if to.service_active {
            sc(&[if from.service_active { "reload" } else { "start" }, SERVICE]);
        }
        if to.socket_enabled && !from.socket_enabled {
            sc(&["enable", SOCKET]);
        }
        if to.socket_active && !from.socket_active {
            sc(&["start", SOCKET]);
        }
        errs
    }

    fn accounts_raw(&self) -> Vec<Account> {
        let groups: Vec<(String, u32, Vec<String>)> = self
            .read(GROUP)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split(':').collect();
                let members = f.get(3).map(|m| m.split(',').filter(|x| !x.is_empty()).map(String::from).collect()).unwrap_or_default();
                Some((f.first()?.to_string(), f.get(2)?.parse().ok()?, members))
            })
            .collect();
        self.read(PASSWD)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split(':').collect();
                if f.len() < 7 {
                    return None;
                }
                let (uid, gid): (u32, u32) = (f[2].parse().ok()?, f[3].parse().ok()?);
                let name = f[0].to_string();
                let mut gs: Vec<String> = groups.iter().filter(|g| g.1 == gid).map(|g| g.0.clone()).collect();
                gs.extend(groups.iter().filter(|g| g.2.contains(&name)).map(|g| g.0.clone()));
                gs.sort();
                gs.dedup();
                Some(Account { name, uid, gid, home: f[5].into(), shell: f[6].into(), groups: gs })
            })
            .collect()
    }

    fn account(&self, name: &str) -> Result<Account> {
        self.accounts_raw()
            .into_iter()
            .find(|a| a.name == name && keyed(a.uid))
            .ok_or_else(|| SshError::NotFound(format!("no local account {name}")))
    }

    /// Root and the local users, with their level and key count.
    pub fn accounts(&self) -> Vec<SshAccount> {
        self.accounts_raw()
            .into_iter()
            .filter(|a| keyed(a.uid))
            .map(|a| {
                let keys = keys::read(&self.path(&a.home), a.uid, a.gid)
                    .map(|t| keys::parse_file(&t).0.iter().filter(|k| !k.weak).count() as u32)
                    .unwrap_or(0);
                SshAccount {
                    admin: a.uid == 0 || a.groups.iter().any(|g| ADMIN_GROUPS.contains(&g.as_str())),
                    level: if a.uid == 0 && shell_level(&a) == SshLevel::Sftp { SshLevel::Shell } else { shell_level(&a) },
                    name: a.name,
                    uid: a.uid,
                    groups: a.groups,
                    keys,
                }
            })
            .collect()
    }

    fn effective(&self) -> config::Effective {
        let o = self.run(&["sshd", "-T"]);
        if o.ok() { config::effective(&o.stdout) } else { config::Effective::default() }
    }

    fn layout(&self) -> config::Layout {
        let main = self.read(config::MAIN).unwrap_or_default();
        config::layout(&main, &|d| self.list_dir(d), &|p| self.read(p))
    }

    /// The drop-in's settings (`None`: not managed).
    fn managed(&self, units: Units) -> Option<(SshSettings, bool)> {
        let text = std::fs::read_to_string(self.dropin_path()).ok()?;
        let global: String = text.lines().take_while(|l| !l.trim_start().to_ascii_lowercase().starts_with("match ")).map(|l| format!("{l}\n")).collect();
        let e = config::effective(&global);
        let mut s = config::adopt(&e, units.on());
        if e.root_login.is_none() {
            s.root_login = SshRootLogin::No;
        }
        let drift = config::render(&s) != text;
        Some((s, drift))
    }

    fn current(&self, units: Units) -> (SshSettings, bool, bool) {
        match self.managed(units) {
            Some((s, drift)) => (s, true, drift),
            None => (config::adopt(&self.effective(), units.on()), false, false),
        }
    }

    fn read_trial(&self) -> Option<Trial> {
        let t = std::fs::read_to_string(self.state.join("trial.json")).ok()?;
        serde_json::from_str(&t).ok()
    }

    fn write_trial(&self, t: Option<&Trial>) -> Result<()> {
        let p = self.state.join("trial.json");
        let io = |e: std::io::Error| SshError::Failed(format!("{}: {e}", p.display()));
        match t {
            Some(t) => {
                let tmp = self.state.join("trial.json.new");
                std::fs::write(&tmp, serde_json::to_vec_pretty(t).expect("serializable")).map_err(io)?;
                std::fs::rename(&tmp, &p).map_err(io)
            }
            None => match std::fs::remove_file(&p) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io(e)),
                _ => Ok(()),
            },
        }
    }

    fn write_dropin(&self, text: Option<&str>) -> std::result::Result<(), String> {
        let p = self.dropin_path();
        let shown = p.display().to_string();
        match text {
            None => match std::fs::remove_file(&p) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("{shown}: {e}")),
                _ => Ok(()),
            },
            Some(t) => {
                use std::io::Write;
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::create_dir_all(p.parent().expect("has a parent")).map_err(|e| format!("{shown}: {e}"))?;
                let tmp = p.with_extension("conf.rynas-new");
                let _ = std::fs::remove_file(&tmp);
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o644)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&tmp)
                    .map_err(|e| format!("{shown}: {e}"))?;
                f.write_all(t.as_bytes()).and_then(|_| f.sync_all()).map_err(|e| format!("{shown}: {e}"))?;
                std::fs::rename(&tmp, &p).map_err(|e| format!("{shown}: {e}"))
            }
        }
    }

    fn host_keys(&self) -> Vec<SshHostKey> {
        let mut names: Vec<String> = self
            .list_dir("/etc/ssh")
            .into_iter()
            .filter(|n| n.starts_with("ssh_host_") && n.ends_with("_key.pub"))
            .collect();
        names.sort();
        let order = |k: &SshHostKey| ["ED25519", "ECDSA", "RSA"].iter().position(|x| *x == k.kind).unwrap_or(9);
        let mut v: Vec<SshHostKey> = names
            .iter()
            .filter_map(|n| keys::host_key(&self.read(&format!("/etc/ssh/{n}"))?))
            .collect();
        v.sort_by_key(order);
        v
    }

    /// `GET /services/ssh` (the trial with its token: the caller strips it for others).
    pub fn state(&self) -> SshState {
        let installed = self.installed();
        if !installed {
            return SshState {
                problems: vec![SshProblem { code: SshProblemCode::NotInstalled, subject: None }],
                dropin: format!("{}/{}", config::DROPIN_DIR, config::DROPIN_NAME),
                ..SshState::default()
            };
        }
        let units = self.units();
        let (settings, managed, drift) = self.current(units);
        let eff = self.effective();
        let layout = self.layout();
        let mut problems = Vec::new();
        let p = |code, subject: Option<String>| SshProblem { code, subject };
        if !layout.includes_dropin {
            problems.push(p(SshProblemCode::NotIncluded, None));
        }
        if drift {
            problems.push(p(SshProblemCode::Drift, None));
        }
        if units.socket_enabled || units.socket_active {
            problems.push(p(SshProblemCode::SocketActivation, None));
        }
        if units.on() && !units.running() {
            problems.push(p(SshProblemCode::NotRunning, None));
        }
        if managed && layout.includes_dropin && !eff.ports.is_empty() {
            let (keys, extra) = config::differences(&settings, &eff);
            for k in keys {
                let file = config::who_sets(&layout, k).unwrap_or(config::MAIN);
                problems.push(p(SshProblemCode::Overridden, Some(format!("{k} {file}"))));
            }
            if !extra.is_empty() {
                let ports: Vec<String> = extra.iter().map(u16::to_string).collect();
                problems.push(p(SshProblemCode::ExtraPorts, Some(ports.join(", "))));
            }
        }
        let trial = self.read_trial().map(|t| SshTrial {
            token: t.token,
            owner: t.owner,
            deadline_ms: t.deadline_ms,
            timeout_s: t.timeout_s,
            port: t.port,
        });
        SshState {
            installed,
            managed,
            settings,
            running: units.running(),
            ports: eff.ports,
            dropin: format!("{}/{}", config::DROPIN_DIR, config::DROPIN_NAME),
            host_keys: self.host_keys(),
            accounts: self.accounts(),
            problems,
            trial,
        }
    }

    fn validate(&self, s: &mut SshSettings) -> Result<()> {
        if s.port == 0 {
            return Err(SshError::Invalid("port", "the port must be 1-65535".into()));
        }
        let mut seen = Vec::new();
        for g in &s.allow_groups {
            let g = g.trim().to_string();
            if !valid_group(&g) {
                return Err(SshError::Invalid("allow_groups", format!("{g:?} is not a group name")));
            }
            if !seen.contains(&g) && !SSH_BASE_GROUPS.contains(&g.as_str()) {
                seen.push(g);
            }
        }
        s.allow_groups = seen;
        Ok(())
    }

    fn ports_in_use(&self) -> BTreeSet<u16> {
        let mut v = BTreeSet::new();
        for f in ["/proc/net/tcp", "/proc/net/tcp6"] {
            v.extend(listening(&self.read(f).unwrap_or_default()));
        }
        v
    }

    /// `sshd -t` on the candidate where the drop-in will be.
    fn check_candidate(&self, text: &str) -> Result<()> {
        let dir = self.state.join("check");
        let io = |e: std::io::Error| SshError::Failed(format!("{}: {e}", dir.display()));
        std::fs::create_dir_all(&dir).map_err(io)?;
        let cand = dir.join(config::DROPIN_NAME);
        std::fs::write(&cand, text).map_err(io)?;
        let main = self.read(config::MAIN).unwrap_or_default();
        let cand_s = cand.display().to_string();
        let main_text = config::candidate_main(&main, &cand_s, &|d| self.list_dir(d));
        let main_p = dir.join("sshd_config");
        std::fs::write(&main_p, main_text).map_err(io)?;
        let o = self.run(&["sshd", "-t", "-f", &main_p.display().to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
        if !o.ok() {
            let why = format!("{}{}", o.stderr.trim(), o.stdout.trim());
            return Err(SshError::Invalid("settings", format!("sshd refuses the configuration: {why}")));
        }
        Ok(())
    }

    fn arm_timer(&self, seconds: u32) -> std::result::Result<(), String> {
        let timer = format!("{ROLLBACK_UNIT}.timer");
        let service = format!("{ROLLBACK_UNIT}.service");
        let _ = self.run.run(&["systemctl", "stop", &timer]);
        let _ = self.run.run(&["systemctl", "reset-failed", &timer, &service]);
        let unit = format!("--unit={ROLLBACK_UNIT}");
        let on = format!("--on-active={seconds}s");
        let o = self.run(&[
            "systemd-run", &unit, "--description=rynas: undo an unconfirmed SSH change", &on,
            "--timer-property=AccuracySec=1s", "--collect", &self.rynasctl, "ssh", "rollback",
        ]);
        if o.ok() { Ok(()) } else { Err(format!("cannot arm the rollback timer: {}", o.stderr.trim())) }
    }

    fn disarm_timer(&self) {
        let _ = self.run.run(&["systemctl", "stop", &format!("{ROLLBACK_UNIT}.timer")]);
    }

    /// Waits (up to 5 s) for sshd to run after a change and listen on `port`.
    fn came_up(&self, port: u16) -> bool {
        for i in 0..10 {
            if self.run(&["systemctl", "is-active", SERVICE]).stdout.trim() == "active"
                && (self.run.simulated() || self.ports_in_use().contains(&port))
            {
                return true;
            }
            if self.run.simulated() || i == 9 {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        false
    }

    /// `PUT /services/ssh`: the trial when the change waits for a confirmation.
    pub fn apply(&self, mut new: SshSettings, timeout_s: Option<u32>, owner: &str) -> Result<Option<SshTrial>> {
        let timeout_s = timeout_s.unwrap_or(SSH_TIMEOUT_DEFAULT_S);
        if !(SSH_TIMEOUT_MIN_S..=SSH_TIMEOUT_MAX_S).contains(&timeout_s) {
            return Err(SshError::Invalid(
                "timeout_s",
                format!("the confirmation time must be {SSH_TIMEOUT_MIN_S}-{SSH_TIMEOUT_MAX_S} s"),
            ));
        }
        self.validate(&mut new)?;
        let _lock = self.lock()?;
        if self.read_trial().is_some() {
            return Err(SshError::Conflict("an SSH change waits for its confirmation: confirm it or undo it first".into()));
        }
        if !self.installed() {
            return Err(SshError::Conflict("openssh-server is not installed".into()));
        }
        let units = self.units();
        let (cur, managed, _) = self.current(units);
        let eff = self.effective();
        // the port: free, unless sshd itself has it
        if !eff.ports.contains(&new.port) && self.ports_in_use().contains(&new.port) {
            return Err(SshError::Invalid("port", format!("port {} is already in use", new.port)));
        }
        // nobody who can log in as an administrator now may lose it
        let accounts = self.accounts();
        let mut cur_eff = cur.clone();
        cur_eff.enabled = units.running() || units.on();
        let before = accounts.iter().any(|a| admin_can_log_in(a, &cur_eff));
        let after = accounts.iter().any(|a| admin_can_log_in(a, &new));
        if new.enabled && before && !after {
            let with_shell: Vec<&SshAccount> = accounts
                .iter()
                .filter(|a| a.admin && a.level == SshLevel::Shell && passes(&a.groups, &new))
                .collect();
            let msg = if with_shell.is_empty() {
                "no administrator would be allowed to log in: give one the SSH shell level (Users) or allow one of their groups".to_string()
            } else if !new.password_auth {
                "password login off, but no administrator who may log in has a key: add a key first".to_string()
            } else {
                "no administrator could log in with these settings".to_string()
            };
            return Err(SshError::Lockout(msg));
        }
        let text = config::render(&new);
        if !self.layout().includes_dropin {
            return Err(SshError::Conflict(format!(
                "{} does not include {}/*.conf: rynas settings would not be read",
                config::MAIN,
                config::DROPIN_DIR
            )));
        }
        self.check_candidate(&text)?;

        let target = Units::wanted(new.enabled);
        let risky = !managed && cur != new
            || managed && (cur.enabled != new.enabled
                || cur.port != new.port
                || cur.password_auth != new.password_auth
                || cur.root_login != new.root_login
                || cur.allow != new.allow
                || (new.allow == SshAllow::Listed && cur.allow_groups != new.allow_groups)
                || cur.tcp_forwarding != new.tcp_forwarding)
            || units != target && (units.on() != new.enabled || units.socket_enabled || units.socket_active);
        let previous = std::fs::read_to_string(self.dropin_path()).ok();
        let trial = Trial {
            token: token(),
            owner: owner.into(),
            deadline_ms: now_ms() + u64::from(timeout_s) * 1000,
            timeout_s,
            port: new.port,
            previous: previous.clone(),
            units,
        };
        if risky {
            self.write_trial(Some(&trial))?;
            if let Err(e) = self.arm_timer(timeout_s) {
                let _ = self.write_trial(None);
                return Err(SshError::Failed(e));
            }
        }
        let undo = |why: String| -> SshError {
            let _ = self.write_dropin(previous.as_deref());
            let _ = self.set_units(self.units(), units);
            if risky {
                self.disarm_timer();
                let _ = self.write_trial(None);
            }
            SshError::Failed(format!("{why}: the previous SSH settings are back"))
        };
        if let Err(e) = self.write_dropin(Some(&text)) {
            return Err(undo(e));
        }
        let errs = self.set_units(units, target);
        if new.enabled && !self.came_up(new.port) {
            let detail = if errs.is_empty() { String::new() } else { format!(" ({})", errs.join("; ")) };
            return Err(undo(format!("sshd did not start{detail}")));
        }
        for e in errs {
            tracing::warn!("ssh: {e}");
        }
        Ok(risky.then_some(SshTrial {
            token: trial.token,
            owner: trial.owner,
            deadline_ms: trial.deadline_ms,
            timeout_s,
            port: new.port,
        }))
    }

    /// `POST /services/ssh/confirm`.
    pub fn confirm(&self, token: &str) -> Result<()> {
        let _lock = self.lock()?;
        let Some(t) = self.read_trial() else {
            return Err(SshError::Conflict("no SSH change waits for a confirmation (undone already?)".into()));
        };
        if t.token.is_empty() || !subtle_eq(t.token.as_bytes(), token.as_bytes()) {
            return Err(SshError::Invalid("token", "this is not the token of the change under test".into()));
        }
        self.disarm_timer();
        self.write_trial(None)
    }

    /// Puts back what the change under test replaced; `false` when there was none.
    pub fn rollback(&self, reason: &str) -> Result<bool> {
        let _lock = self.lock()?;
        let Some(t) = self.read_trial() else { return Ok(false) };
        tracing::warn!("ssh: undoing the unconfirmed change ({reason})");
        let mut errs = Vec::new();
        if let Err(e) = self.write_dropin(t.previous.as_deref()) {
            errs.push(e);
        }
        errs.extend(self.set_units(self.units(), t.units));
        self.disarm_timer();
        self.write_trial(None)?;
        if errs.is_empty() {
            Ok(true)
        } else {
            Err(SshError::Failed(format!("previous SSH settings restored with errors: {}", errs.join("; "))))
        }
    }

    /// Rolls back a trial whose time is over (rynasd's watch, after a restart).
    pub fn expire(&self, now: u64) -> Result<bool> {
        match self.read_trial() {
            Some(t) if now >= t.deadline_ms => self.rollback("timeout"),
            _ => Ok(false),
        }
    }

    /// The trial, if any.
    pub fn trial_owner(&self) -> Option<String> {
        self.read_trial().map(|t| t.owner)
    }

    // ---------------------------------------------------------------- keys

    pub fn keys(&self, user: &str) -> Result<SshKeyList> {
        let a = self.account(user)?;
        let text = keys::read(&self.path(&a.home), a.uid, a.gid).map_err(SshError::Failed)?;
        let (keys, other_lines) = keys::parse_file(&text);
        Ok(SshKeyList { user: a.name, keys, other_lines })
    }

    pub fn add_key(&self, user: &str, line: &str) -> Result<SshKey> {
        let a = self.account(user)?;
        let (clean, key) = keys::check_new(line).map_err(|e| SshError::Invalid("key", e))?;
        let fp = key.fingerprint.clone();
        keys::update(&self.path(&a.home), a.uid, a.gid, &|old| {
            if keys::parse_file(old).0.iter().any(|k| k.fingerprint == fp) {
                return Err(format!("already there: {fp}"));
            }
            let mut t = old.to_string();
            if !t.is_empty() && !t.ends_with('\n') {
                t.push('\n');
            }
            t.push_str(&clean);
            t.push('\n');
            Ok(t)
        })
        .map_err(|e| if e.starts_with("already there") { SshError::Conflict(e) } else { SshError::Failed(e) })?;
        Ok(key)
    }

    pub fn remove_key(&self, user: &str, fingerprint: &str) -> Result<()> {
        let a = self.account(user)?;
        // the last key of the administrator who keeps password login off: refused
        let accounts = self.accounts();
        let units = self.units();
        let (cur, _, _) = self.current(units);
        let mut cur_eff = cur;
        cur_eff.enabled = units.on() || units.running();
        let mut after: Vec<SshAccount> = accounts.clone();
        if let Some(x) = after.iter_mut().find(|x| x.name == a.name) {
            x.keys = x.keys.saturating_sub(1);
        }
        let before_ok = accounts.iter().any(|x| admin_can_log_in(x, &cur_eff));
        let after_ok = after.iter().any(|x| admin_can_log_in(x, &cur_eff));
        if before_ok && !after_ok {
            return Err(SshError::Lockout(
                "this is the last key of the only administrator who can log in (password login is off): turn password login on or add another key first".into(),
            ));
        }
        keys::update(&self.path(&a.home), a.uid, a.gid, &|old| {
            keys::without(old, fingerprint).ok_or_else(|| "no such key".to_string())
        })
        .map_err(|e| if e == "no such key" { SshError::NotFound(e) } else { SshError::Failed(e) })
    }

    /// `GET /services/ssh/logins`.
    pub fn logins(&self) -> SshLogins {
        let argv = logins::argv();
        logins::summarize(&self.run(&argv).stdout)
    }
}

/// Equal in constant time (the confirmation token).
fn subtle_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests;
