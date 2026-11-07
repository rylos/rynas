//! Linux and Samba accounts behind a trait: [`HostAccounts`] reads `/etc/passwd`,
//! `/etc/group`, `/etc/shadow` and runs `useradd`/`usermod`/`userdel`/`groupadd`/`gpasswd`/
//! `chpasswd`/`pdbedit` (argv only, secrets on stdin); [`MemoryAccounts`] keeps everything
//! in memory for tests and `--dry-system`.
//!
//! Every change is an [`AccountOp`] value, so tests can assert the exact sequence of
//! operations (e.g. that the Samba password is written after the shadow one).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rynas_tasks::{Cmd, CmdError};

/// A password or password hash: never printed, wiped on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<secret>")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // SAFETY: zero bytes are valid UTF-8; the string is not used afterwards.
        unsafe {
            for b in self.0.as_bytes_mut() {
                std::ptr::write_volatile(b, 0);
            }
        }
    }
}

/// A line of `/etc/passwd` (plus the expiry from `/etc/shadow`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SysUser {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    /// First field of GECOS (the full name).
    pub full_name: String,
    pub home: String,
    pub shell: String,
    /// Shadow account expiry, days since the epoch (`1` = disabled by rynas).
    pub expire_days: Option<i64>,
}

/// A line of `/etc/group`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SysGroup {
    pub name: String,
    pub gid: u32,
    pub members: Vec<String>,
}

/// An entry of the Samba passdb (tdbsam).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SambaUser {
    pub name: String,
    pub disabled: bool,
}

/// Shadow account expiry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expire {
    Never,
    /// Expired on day 1 of the epoch: the account is disabled.
    Disabled,
    /// `YYYY-MM-DD`.
    Date(String),
}

impl Expire {
    fn arg(&self) -> String {
        match self {
            Expire::Never => String::new(),
            Expire::Disabled => "1".into(),
            Expire::Date(d) => d.clone(),
        }
    }

    /// Days since the epoch as stored in `/etc/shadow`.
    pub fn days(&self) -> Option<i64> {
        match self {
            Expire::Never => None,
            Expire::Disabled => Some(1),
            Expire::Date(d) => crate::validate::date_to_days(d),
        }
    }
}

/// One change to the accounts of the system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountOp {
    /// `groupadd -g <gid> <name>`, or `groupadd -r <name>` (system group) without gid.
    GroupAdd {
        name: String,
        gid: Option<u32>,
    },
    GroupDel {
        name: String,
    },
    /// `gpasswd -a <user> <group>`.
    MemberAdd {
        group: String,
        user: String,
    },
    /// `gpasswd -d <user> <group>`.
    MemberDel {
        group: String,
        user: String,
    },
    UserAdd {
        name: String,
        uid: u32,
        /// Primary group.
        group: String,
        full_name: String,
        home: String,
        /// `useradd -m` (Debian's way, with `/etc/skel`); `false`: `-M`, the home was made by
        /// rynas (a subvolume in the homes folder).
        create_home: bool,
        shell: String,
        expire: Expire,
    },
    UserMod {
        name: String,
        full_name: Option<String>,
        shell: Option<String>,
        expire: Option<Expire>,
        /// `usermod -d` without `-m`: the files were moved before (home move).
        home: Option<String>,
    },
    UserDel {
        name: String,
        remove_home: bool,
    },
    /// `usermod -u <uid> <name>` (UID change; usermod also gives the home directory's files
    /// to the new uid, the rest goes through the ownership remap).
    UserUid {
        name: String,
        uid: u32,
    },
    /// `groupmod -g <gid> <name>` (users with it as primary group follow; files do not).
    GroupGid {
        name: String,
        gid: u32,
    },
    /// `net cache flush`: Samba forgets cached SID/uid mappings after an id change.
    SambaCacheFlush,
    /// `chpasswd -c YESCRYPT` with `name:password` on stdin.
    SetShadowPassword {
        name: String,
        password: Secret,
    },
    /// `chpasswd -e` with `name:hash` on stdin (rollback of a failed password change).
    RestoreShadowHash {
        name: String,
        hash: Secret,
    },
    /// `pdbedit -a -t -u <name>` with the password twice on stdin.
    SambaAdd {
        name: String,
        password: Secret,
    },
    /// `smbpasswd -s <name>` with the password twice on stdin (existing Samba account).
    SambaSetPassword {
        name: String,
        password: Secret,
    },
    /// `pdbedit -x -u <name>`.
    SambaDel {
        name: String,
    },
    /// `pdbedit -r -c "[D]"` / `"[]"` (only N, D, H, L, X can be set).
    SambaFlags {
        name: String,
        disabled: bool,
    },
}

impl AccountOp {
    /// Short command name, also used by [`MemoryAccounts`] to inject failures.
    pub fn kind(&self) -> &'static str {
        match self {
            AccountOp::GroupAdd { .. } => "groupadd",
            AccountOp::GroupDel { .. } => "groupdel",
            AccountOp::MemberAdd { .. } => "gpasswd-add",
            AccountOp::MemberDel { .. } => "gpasswd-del",
            AccountOp::UserAdd { .. } => "useradd",
            AccountOp::UserMod { .. } => "usermod",
            AccountOp::UserDel { .. } => "userdel",
            AccountOp::UserUid { .. } => "usermod-uid",
            AccountOp::GroupGid { .. } => "groupmod-gid",
            AccountOp::SambaCacheFlush => "net-cache-flush",
            AccountOp::SetShadowPassword { .. } => "chpasswd",
            AccountOp::RestoreShadowHash { .. } => "chpasswd-restore",
            AccountOp::SambaAdd { .. } => "pdbedit-add",
            AccountOp::SambaSetPassword { .. } => "smbpasswd",
            AccountOp::SambaDel { .. } => "pdbedit-delete",
            AccountOp::SambaFlags { .. } => "pdbedit-flags",
        }
    }

    /// Log line (no secrets).
    pub fn describe(&self) -> String {
        match self {
            AccountOp::GroupAdd { name, gid: Some(g) } => format!("groupadd {name} (gid {g})"),
            AccountOp::GroupAdd { name, gid: None } => format!("groupadd {name} (system)"),
            AccountOp::GroupDel { name } => format!("groupdel {name}"),
            AccountOp::MemberAdd { group, user } => format!("gpasswd: add {user} to {group}"),
            AccountOp::MemberDel { group, user } => {
                format!("gpasswd: remove {user} from {group}")
            }
            AccountOp::UserAdd { name, uid, .. } => format!("useradd {name} (uid {uid})"),
            AccountOp::UserMod {
                name,
                full_name,
                shell,
                expire,
                home,
            } => {
                let mut what = Vec::new();
                if full_name.is_some() {
                    what.push("full name".to_string());
                }
                if let Some(h) = home {
                    what.push(format!("home {h}"));
                }
                if let Some(s) = shell {
                    what.push(format!("shell {s}"));
                }
                if let Some(e) = expire {
                    what.push(match e {
                        Expire::Never => "no expiry".into(),
                        Expire::Disabled => "disabled".into(),
                        Expire::Date(d) => format!("expires {d}"),
                    });
                }
                format!("usermod {name}: {}", what.join(", "))
            }
            AccountOp::UserDel { name, remove_home } => {
                format!(
                    "userdel {name}{}",
                    if *remove_home { " (home removed)" } else { "" }
                )
            }
            AccountOp::UserUid { name, uid } => format!("usermod {name}: uid {uid}"),
            AccountOp::GroupGid { name, gid } => format!("groupmod {name}: gid {gid}"),
            AccountOp::SambaCacheFlush => "net cache flush".into(),
            AccountOp::SetShadowPassword { name, .. } => {
                format!("chpasswd: new password for {name}")
            }
            AccountOp::RestoreShadowHash { name, .. } => {
                format!("chpasswd: previous password of {name} restored")
            }
            AccountOp::SambaAdd { name, .. } => format!("pdbedit: Samba account for {name}"),
            AccountOp::SambaSetPassword { name, .. } => {
                format!("smbpasswd: new Samba password for {name}")
            }
            AccountOp::SambaDel { name } => format!("pdbedit: Samba account of {name} removed"),
            AccountOp::SambaFlags { name, disabled } => format!(
                "pdbedit: Samba account of {name} {}",
                if *disabled { "disabled" } else { "enabled" }
            ),
        }
    }
}

pub trait Accounts: Send + Sync {
    fn users(&self) -> Result<Vec<SysUser>, String>;
    fn groups(&self) -> Result<Vec<SysGroup>, String>;
    /// The Samba passdb; `None` when Samba is not installed.
    fn samba_users(&self) -> Result<Option<Vec<SambaUser>>, String>;
    /// Current shadow hash of a user (to undo a half-done password change).
    fn shadow_hash(&self, name: &str) -> Result<Option<Secret>, String>;
    fn exec(&self, op: &AccountOp) -> Result<(), String>;
}

// ---------------------------------------------------------------------------------------------
// parsing
// ---------------------------------------------------------------------------------------------

pub fn parse_passwd(text: &str) -> Vec<SysUser> {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            if f.len() < 7 {
                return None;
            }
            Some(SysUser {
                name: f[0].into(),
                uid: f[2].parse().ok()?,
                gid: f[3].parse().ok()?,
                full_name: f[4].split(',').next().unwrap_or("").into(),
                home: f[5].into(),
                shell: f[6].into(),
                expire_days: None,
            })
        })
        .collect()
}

pub fn parse_group(text: &str) -> Vec<SysGroup> {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            if f.len() < 4 {
                return None;
            }
            Some(SysGroup {
                name: f[0].into(),
                gid: f[2].parse().ok()?,
                members: f[3]
                    .split(',')
                    .filter(|m| !m.is_empty())
                    .map(String::from)
                    .collect(),
            })
        })
        .collect()
}

/// `name -> (hash, expire days)` from `/etc/shadow`.
fn parse_shadow(text: &str) -> BTreeMap<String, (String, Option<i64>)> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            if f.len() < 8 {
                return None;
            }
            Some((f[0].to_string(), (f[1].to_string(), f[7].parse().ok())))
        })
        .collect()
}

/// `pdbedit -L -v` output: "Unix username:" and "Account Flags:" lines.
pub fn parse_pdbedit_verbose(text: &str) -> Vec<SambaUser> {
    let mut out = Vec::new();
    let mut cur: Option<SambaUser> = None;
    for l in text.lines() {
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim() {
            "Unix username" => {
                if let Some(u) = cur.take() {
                    out.push(u);
                }
                cur = Some(SambaUser {
                    name: v.into(),
                    disabled: false,
                });
            }
            "Account Flags" => {
                if let Some(u) = cur.as_mut() {
                    u.disabled = v.trim_matches(['[', ']']).contains('D');
                }
            }
            _ => {}
        }
    }
    out.extend(cur);
    out
}

// ---------------------------------------------------------------------------------------------
// the real system
// ---------------------------------------------------------------------------------------------

/// The accounts of the host. Reads below `root` ("/" in production).
pub struct HostAccounts {
    root: PathBuf,
}

const CMD_TIMEOUT: Duration = Duration::from_secs(60);

impl HostAccounts {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn read(&self, abs: &str) -> Result<String, String> {
        let p = rynas_config::files::under_root(&self.root, Path::new(abs));
        std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))
    }
}

fn run(cmd: Cmd) -> Result<(), String> {
    let name = cmd.program().to_string_lossy().into_owned();
    cmd.timeout(CMD_TIMEOUT)
        .run_blocking()
        .map(|_| ())
        .map_err(|e| format!("{name}: {e}"))
}

fn twice(p: &Secret) -> Vec<u8> {
    format!("{0}\n{0}\n", p.expose()).into_bytes()
}

impl Accounts for HostAccounts {
    fn users(&self) -> Result<Vec<SysUser>, String> {
        let mut users = parse_passwd(&self.read("/etc/passwd")?);
        // only root can read the shadow file: without it there are no expiry dates
        if let Ok(t) = self.read("/etc/shadow") {
            let sh = parse_shadow(&t);
            for u in &mut users {
                u.expire_days = sh.get(&u.name).and_then(|s| s.1);
            }
        }
        Ok(users)
    }

    fn groups(&self) -> Result<Vec<SysGroup>, String> {
        Ok(parse_group(&self.read("/etc/group")?))
    }

    fn samba_users(&self) -> Result<Option<Vec<SambaUser>>, String> {
        match Cmd::new("pdbedit")
            .args(["-L", "-v"])
            .timeout(CMD_TIMEOUT)
            .run_blocking()
        {
            Ok(o) => Ok(Some(parse_pdbedit_verbose(&o.stdout))),
            Err(CmdError::Spawn(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("pdbedit -L: {e}")),
        }
    }

    fn shadow_hash(&self, name: &str) -> Result<Option<Secret>, String> {
        let t = self.read("/etc/shadow")?;
        Ok(parse_shadow(&t).remove(name).map(|(h, _)| Secret::new(h)))
    }

    fn exec(&self, op: &AccountOp) -> Result<(), String> {
        match op {
            AccountOp::GroupAdd { name, gid } => run(match gid {
                Some(g) => Cmd::new("groupadd").args(["-g", &g.to_string(), name]),
                None => Cmd::new("groupadd").args(["-r", name]),
            }),
            AccountOp::GroupDel { name } => run(Cmd::new("groupdel").arg(name)),
            AccountOp::MemberAdd { group, user } => {
                run(Cmd::new("gpasswd").args(["-a", user, group]))
            }
            AccountOp::MemberDel { group, user } => {
                run(Cmd::new("gpasswd").args(["-d", user, group]))
            }
            AccountOp::UserAdd {
                name,
                uid,
                group,
                full_name,
                home,
                create_home,
                shell,
                expire,
            } => {
                let mut c = Cmd::new("useradd").args([
                    "-u",
                    &uid.to_string(),
                    "-g",
                    group,
                    "-N",
                    "-c",
                    full_name,
                    "-d",
                    home,
                ]);
                c = if *create_home {
                    c.args(["-m", "-K", "HOME_MODE=0700"])
                } else {
                    c.arg("-M")
                };
                c = c.args(["-s", shell]);
                if *expire != Expire::Never {
                    c = c.args(["-e", &expire.arg()]);
                }
                run(c.arg(name))
            }
            AccountOp::UserMod {
                name,
                full_name,
                shell,
                expire,
                home,
            } => {
                let mut c = Cmd::new("usermod");
                if let Some(f) = full_name {
                    c = c.args(["-c", f]);
                }
                if let Some(h) = home {
                    c = c.args(["-d", h]);
                }
                if let Some(s) = shell {
                    c = c.args(["-s", s]);
                }
                if let Some(e) = expire {
                    c = c.args(["-e", &e.arg()]);
                }
                run(c.arg(name))
            }
            AccountOp::UserDel { name, remove_home } => {
                let mut c = Cmd::new("userdel");
                if *remove_home {
                    c = c.arg("-r");
                }
                run(c.arg(name))
            }
            AccountOp::UserUid { name, uid } => {
                let c = Cmd::new("usermod").args(["-u", &uid.to_string(), name]);
                match c.timeout(CMD_TIMEOUT).run_blocking() {
                    Ok(_) => Ok(()),
                    // 12 = E_HOMEDIR: passwd is written already, only some home files could
                    // not change owner (read-only snapshot inside...): the remap that follows
                    // reports them
                    Err(CmdError::Failed(o)) if o.code == Some(12) => {
                        tracing::warn!("usermod -u {name}: {}", o.stderr.trim());
                        Ok(())
                    }
                    Err(e) => Err(format!("usermod: {e}")),
                }
            }
            AccountOp::GroupGid { name, gid } => {
                run(Cmd::new("groupmod").args(["-g", &gid.to_string(), name]))
            }
            AccountOp::SambaCacheFlush => run(Cmd::new("net").args(["cache", "flush"])),
            AccountOp::SetShadowPassword { name, password } => run(Cmd::new("chpasswd")
                .args(["-c", "YESCRYPT"])
                .stdin(format!("{name}:{}\n", password.expose()))),
            AccountOp::RestoreShadowHash { name, hash } => run(Cmd::new("chpasswd")
                .arg("-e")
                .stdin(format!("{name}:{}\n", hash.expose()))),
            AccountOp::SambaAdd { name, password } => run(Cmd::new("pdbedit")
                .args(["-a", "-t", "-u", name])
                .stdin(twice(password))),
            AccountOp::SambaSetPassword { name, password } => run(Cmd::new("smbpasswd")
                .args(["-s", name])
                .stdin(twice(password))),
            AccountOp::SambaDel { name } => run(Cmd::new("pdbedit").args(["-x", "-u", name])),
            AccountOp::SambaFlags { name, disabled } => run(Cmd::new("pdbedit").args([
                "-r",
                "-u",
                name,
                "-c",
                if *disabled { "[D]" } else { "[]" },
            ])),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// in memory
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct MemoryAccountsState {
    pub users: Vec<SysUser>,
    pub groups: Vec<SysGroup>,
    /// `None`: Samba not installed.
    pub samba: Option<BTreeMap<String, bool>>,
    /// Shadow hash per user (the "hash" of a password `p` is `hash(p)`).
    pub shadow: BTreeMap<String, String>,
    /// Samba "NT hash" per user, same fake scheme.
    pub samba_pw: BTreeMap<String, String>,
    /// Every operation, in order ([`AccountOp::describe`]).
    pub log: Vec<String>,
    /// Kinds ([`AccountOp::kind`]) that fail.
    pub fail: Vec<String>,
}

/// Accounts kept in memory: tests and `--dry-system`.
pub struct MemoryAccounts {
    state: Mutex<MemoryAccountsState>,
}

impl Default for MemoryAccounts {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryAccounts {
    /// Empty system with the `users` group (gid 100) and Samba installed.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(MemoryAccountsState {
                groups: vec![SysGroup {
                    name: "users".into(),
                    gid: 100,
                    members: Vec::new(),
                }],
                samba: Some(BTreeMap::new()),
                ..Default::default()
            }),
        }
    }

    /// A copy of the host's users and groups (read-only access), for `--dry-system`.
    pub fn from_host() -> Self {
        let m = Self::new();
        {
            let mut s = m.state();
            if let Ok(t) = std::fs::read_to_string("/etc/passwd") {
                s.users = parse_passwd(&t);
            }
            if let Ok(t) = std::fs::read_to_string("/etc/group") {
                s.groups = parse_group(&t);
            }
        }
        m
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, MemoryAccountsState> {
        self.state.lock().unwrap()
    }
}

fn fake_hash(p: &str) -> String {
    format!("hash({p})")
}

impl MemoryAccountsState {
    fn group_mut(&mut self, name: &str) -> Result<&mut SysGroup, String> {
        self.groups
            .iter_mut()
            .find(|g| g.name == name)
            .ok_or_else(|| format!("group '{name}' does not exist"))
    }

    fn user_mut(&mut self, name: &str) -> Result<&mut SysUser, String> {
        self.users
            .iter_mut()
            .find(|u| u.name == name)
            .ok_or_else(|| format!("user '{name}' does not exist"))
    }

    fn samba_mut(&mut self) -> Result<&mut BTreeMap<String, bool>, String> {
        self.samba
            .as_mut()
            .ok_or_else(|| "pdbedit: not installed".to_string())
    }
}

impl Accounts for MemoryAccounts {
    fn users(&self) -> Result<Vec<SysUser>, String> {
        Ok(self.state().users.clone())
    }

    fn groups(&self) -> Result<Vec<SysGroup>, String> {
        Ok(self.state().groups.clone())
    }

    fn samba_users(&self) -> Result<Option<Vec<SambaUser>>, String> {
        Ok(self.state().samba.as_ref().map(|m| {
            m.iter()
                .map(|(n, d)| SambaUser {
                    name: n.clone(),
                    disabled: *d,
                })
                .collect()
        }))
    }

    fn shadow_hash(&self, name: &str) -> Result<Option<Secret>, String> {
        Ok(self.state().shadow.get(name).cloned().map(Secret::new))
    }

    fn exec(&self, op: &AccountOp) -> Result<(), String> {
        let mut s = self.state();
        s.log.push(op.describe());
        if s.fail.iter().any(|f| f == op.kind()) {
            return Err(format!("{}: exit code 1: injected failure", op.kind()));
        }
        match op {
            AccountOp::GroupAdd { name, gid } => {
                if s.groups.iter().any(|g| g.name == *name) {
                    return Err(format!("groupadd: group '{name}' already exists"));
                }
                let gid = gid.unwrap_or_else(|| {
                    (100..999)
                        .rev()
                        .find(|g| !s.groups.iter().any(|x| x.gid == *g))
                        .unwrap_or(999)
                });
                s.groups.push(SysGroup {
                    name: name.clone(),
                    gid,
                    members: Vec::new(),
                });
            }
            AccountOp::GroupDel { name } => {
                let before = s.groups.len();
                s.groups.retain(|g| g.name != *name);
                if s.groups.len() == before {
                    return Err(format!("groupdel: group '{name}' does not exist"));
                }
            }
            AccountOp::MemberAdd { group, user } => {
                if !s.users.iter().any(|u| u.name == *user) {
                    return Err(format!("gpasswd: user '{user}' does not exist"));
                }
                let g = s.group_mut(group)?;
                if !g.members.contains(user) {
                    g.members.push(user.clone());
                }
            }
            AccountOp::MemberDel { group, user } => {
                let g = s.group_mut(group)?;
                g.members.retain(|m| m != user);
            }
            AccountOp::UserAdd {
                name,
                uid,
                group,
                full_name,
                home,
                shell,
                expire,
                ..
            } => {
                if s.users.iter().any(|u| u.name == *name || u.uid == *uid) {
                    return Err(format!(
                        "useradd: user '{name}' or uid {uid} already exists"
                    ));
                }
                let gid = s
                    .groups
                    .iter()
                    .find(|g| g.name == *group)
                    .map(|g| g.gid)
                    .ok_or_else(|| format!("useradd: group '{group}' does not exist"))?;
                s.users.push(SysUser {
                    name: name.clone(),
                    uid: *uid,
                    gid,
                    full_name: full_name.clone(),
                    home: home.clone(),
                    shell: shell.clone(),
                    expire_days: expire.days(),
                });
                s.shadow.insert(name.clone(), "!".into());
            }
            AccountOp::UserMod {
                name,
                full_name,
                shell,
                expire,
                home,
            } => {
                let u = s.user_mut(name)?;
                if let Some(f) = full_name {
                    u.full_name = f.clone();
                }
                if let Some(h) = home {
                    u.home = h.clone();
                }
                if let Some(sh) = shell {
                    u.shell = sh.clone();
                }
                if let Some(e) = expire {
                    u.expire_days = e.days();
                }
            }
            AccountOp::UserDel { name, .. } => {
                let before = s.users.len();
                s.users.retain(|u| u.name != *name);
                if s.users.len() == before {
                    return Err(format!("userdel: user '{name}' does not exist"));
                }
                for g in &mut s.groups {
                    g.members.retain(|m| m != name);
                }
                s.shadow.remove(name);
            }
            AccountOp::UserUid { name, uid } => {
                if s.users.iter().any(|u| u.uid == *uid && u.name != *name) {
                    return Err(format!("usermod: UID '{uid}' already exists"));
                }
                s.user_mut(name)?.uid = *uid;
            }
            AccountOp::GroupGid { name, gid } => {
                if s.groups.iter().any(|g| g.gid == *gid && g.name != *name) {
                    return Err(format!("groupmod: GID '{gid}' already exists"));
                }
                let old = s.group_mut(name)?.gid;
                s.group_mut(name)?.gid = *gid;
                for u in s.users.iter_mut().filter(|u| u.gid == old) {
                    u.gid = *gid;
                }
            }
            AccountOp::SambaCacheFlush => {
                s.samba_mut()?;
            }
            AccountOp::SetShadowPassword { name, password } => {
                s.user_mut(name)?;
                s.shadow.insert(name.clone(), fake_hash(password.expose()));
            }
            AccountOp::RestoreShadowHash { name, hash } => {
                s.user_mut(name)?;
                s.shadow.insert(name.clone(), hash.expose().to_string());
            }
            AccountOp::SambaAdd { name, password } => {
                s.user_mut(name)?;
                let sm = s.samba_mut()?;
                if sm.contains_key(name) {
                    return Err(format!("pdbedit: user {name} already exists"));
                }
                sm.insert(name.clone(), false);
                s.samba_pw
                    .insert(name.clone(), fake_hash(password.expose()));
            }
            AccountOp::SambaSetPassword { name, password } => {
                if !s.samba_mut()?.contains_key(name) {
                    return Err(format!("smbpasswd: no Samba account {name}"));
                }
                s.samba_pw
                    .insert(name.clone(), fake_hash(password.expose()));
            }
            AccountOp::SambaDel { name } => {
                if s.samba_mut()?.remove(name).is_none() {
                    return Err(format!("pdbedit: no Samba account {name}"));
                }
                s.samba_pw.remove(name);
            }
            AccountOp::SambaFlags { name, disabled } => {
                let sm = s.samba_mut()?;
                match sm.get_mut(name) {
                    Some(d) => *d = *disabled,
                    None => return Err(format!("pdbedit: no Samba account {name}")),
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsers() {
        let p = parse_passwd(
            "root:x:0:0:root:/root:/bin/bash\nalice:x:1001:100:Alice Smith,,,:/home/alice:/usr/sbin/nologin\nbad line\n",
        );
        assert_eq!(p.len(), 2);
        assert_eq!(p[1].full_name, "Alice Smith");
        assert_eq!(p[1].uid, 1001);
        let g = parse_group("users:x:100:\nstaff:x:1000:alice,bob\n");
        assert_eq!(g[1].members, ["alice", "bob"]);
        let sh = parse_shadow("alice:$y$j9T$abc:20000:0:99999:7::1:\nbob:!:20000:0:99999:7:::\n");
        assert_eq!(sh["alice"].1, Some(1));
        assert_eq!(sh["bob"].1, None);
        let s = parse_pdbedit_verbose(
            "Unix username:        alice\nNT username:\nAccount Flags:        [DU         ]\n---------------\nUnix username:        bob\nAccount Flags:        [U          ]\n",
        );
        assert_eq!(
            s,
            [
                SambaUser {
                    name: "alice".into(),
                    disabled: true
                },
                SambaUser {
                    name: "bob".into(),
                    disabled: false
                }
            ]
        );
    }

    #[test]
    fn secrets_are_not_printed() {
        let op = AccountOp::SambaAdd {
            name: "alice".into(),
            password: Secret::new("hunter2"),
        };
        assert!(!format!("{op:?}").contains("hunter2"));
        assert!(!op.describe().contains("hunter2"));
    }
}
