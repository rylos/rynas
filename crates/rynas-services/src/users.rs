//! `users` domain: local Linux users and groups (uid/gid 1000–59999) and their Samba
//! accounts.
//!
//! There are no files to render: [`Applier::reconcile`] compares the wanted users and
//! groups with `/etc/passwd`/`/etc/group` and runs the commands that close the gap
//! (`groupadd`, `useradd`, `usermod`, `gpasswd`, `pdbedit`, `userdel`, `groupdel`). Only
//! users and groups of the configuration are touched, plus deletions of those that were in
//! the previous value; users created outside rynas are left alone (and reported by the
//! drift check). Membership changes are limited to the groups rynas manages, so `sudo`,
//! `docker`... memberships survive.
//!
//! Passwords never go through the configuration store: [`set_password`] runs `chpasswd`
//! (yescrypt) and then the Samba password as one transaction, restoring the previous shadow
//! hash when the Samba step fails. rynas stores no hash of its own.
//!
//! Homes: a user whose home is `/srv/rynas/homes/<name>` (the user homes service, see
//! [`crate::homes`]) gets it created by rynas as a subvolume in the homes folder before
//! `useradd -M`; with `remove_homes` that subvolume is deleted after `userdel`. Other homes
//! are created and removed by `useradd -m`/`userdel -r`. A home changes only through the
//! home move, which copies the files first and then applies a document whose `home` differs
//! from the previous one: reconcile runs `usermod -d` (never `-m`) when the new folder exists.
//!
//! Limits: a deleted home directory (`remove_homes`) cannot come back on rollback.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rynas_api_types::{
    LOCAL_ID_MAX, LOCAL_ID_MIN, LocalGroup, LocalUser, ShellAccess, StateDrift, StateDriftArgs,
    StateDriftCode, StateDriftKind, UsersConfig,
};
use rynas_config::{Applier, ApplyCtx, Invalid, ManagedFile};
use serde_json::Value;

use crate::accounts::{AccountOp, Accounts, Expire, SambaUser, Secret, SysGroup, SysUser};
use crate::homes::{self, HomesHost};
use crate::validate::{self, account_name, one_line};

pub const USERS_KEY: &str = "users";
/// Members may log in to rynas (with `sudo`).
pub const ADMIN_GROUP: &str = "rynas-admin";
/// Members may log in over SSH with a shell (sshd `AllowGroups`, phase 2 plugin).
pub const SSH_GROUP: &str = "rynas-ssh";
/// Members may use SFTP only (chrooted, phase 2 plugin).
pub const SFTP_GROUP: &str = "rynas-sftp";
/// Groups that grant a role or a kind of access rather than files.
const SPECIAL_GROUPS: [&str; 3] = [ADMIN_GROUP, SSH_GROUP, SFTP_GROUP];
/// Primary group of the users created by rynas.
pub const PRIMARY_GROUP: &str = "users";
pub const NOLOGIN: &str = "/usr/sbin/nologin";
pub const LOGIN_SHELL: &str = "/bin/bash";

fn in_range(id: u32) -> bool {
    (LOCAL_ID_MIN..=LOCAL_ID_MAX).contains(&id)
}

/// A shell that lets the user in (not `nologin`/`false`).
pub fn is_login_shell(s: &str) -> bool {
    !(s.is_empty() || s.ends_with("/nologin") || s.ends_with("/false"))
}

pub fn parse(v: Option<&Value>) -> Result<UsersConfig, String> {
    match v {
        Some(v) => {
            serde_json::from_value(v.clone()).map_err(|e| format!("invalid users document: {e}"))
        }
        None => Ok(UsersConfig::default()),
    }
}

fn expire_of(u: &LocalUser) -> Expire {
    match (&u.expires, u.disabled) {
        (_, true) => Expire::Disabled,
        (Some(d), false) => Expire::Date(d.clone()),
        (None, false) => Expire::Never,
    }
}

fn shell_of(u: &LocalUser) -> &'static str {
    if u.shell == ShellAccess::Shell {
        LOGIN_SHELL
    } else {
        NOLOGIN
    }
}

/// Groups a user should belong to among those rynas manages.
fn wanted_groups(u: &LocalUser) -> BTreeSet<String> {
    let mut g: BTreeSet<String> = u.groups.iter().cloned().collect();
    if u.admin {
        g.insert(ADMIN_GROUP.into());
    }
    match u.shell {
        ShellAccess::Shell => {
            g.insert(SSH_GROUP.into());
        }
        ShellAccess::Sftp => {
            g.insert(SFTP_GROUP.into());
        }
        ShellAccess::None => {}
    }
    g
}

/// Lowest free id in the local range.
pub fn next_free_id(used: &BTreeSet<u32>) -> Option<u32> {
    (LOCAL_ID_MIN..=LOCAL_ID_MAX).find(|i| !used.contains(i))
}

pub struct UsersDomain {
    accounts: Arc<dyn Accounts>,
    homes: Option<Arc<dyn HomesHost>>,
}

impl UsersDomain {
    pub fn new(accounts: Arc<dyn Accounts>) -> Self {
        Self {
            accounts,
            homes: None,
        }
    }

    /// Homes in the homes folder are created and removed through `host`.
    pub fn with_homes(mut self, host: Arc<dyn HomesHost>) -> Self {
        self.homes = Some(host);
        self
    }

    fn homes_host(&self) -> Result<&dyn HomesHost, String> {
        self.homes
            .as_deref()
            .ok_or_else(|| "user homes are not available in this rynasd".to_string())
    }

    pub fn accounts(&self) -> &Arc<dyn Accounts> {
        &self.accounts
    }

    fn exec(&self, op: AccountOp, ctx: &ApplyCtx) -> Result<(), String> {
        (ctx.log)(&op.describe());
        self.accounts.exec(&op)
    }

    /// Checks and normalizes a document: sorted lists, defaults filled in, free uids/gids
    /// picked for `0`, no clash with system accounts.
    pub fn normalize(&self, mut c: UsersConfig) -> Result<UsersConfig, Invalid> {
        let sys_users = self.accounts.users()?;
        let sys_groups = self.accounts.groups()?;

        // groups
        let mut gids: BTreeSet<u32> = sys_groups.iter().map(|g| g.gid).collect();
        gids.extend(c.groups.iter().map(|g| g.gid));
        let mut seen = BTreeSet::new();
        for g in &mut c.groups {
            g.name = g.name.trim().to_string();
            g.description = g.description.trim().to_string();
            if !account_name(&g.name) {
                return Err(Invalid::at(
                    "name",
                    format!(
                        "invalid group name {:?}: use a-z, 0-9, '_' and '-', starting with a letter or '_' (max 32)",
                        g.name
                    ),
                ));
            }
            if validate::RESERVED_NAMES.contains(&g.name.as_str()) {
                return Err(Invalid::at(
                    "name",
                    format!("{} is a system group and cannot be managed here", g.name),
                ));
            }
            one_line(&g.description, 256).map_err(|e| {
                Invalid::at("description", format!("group {}: description {e}", g.name))
            })?;
            if !seen.insert(g.name.clone()) {
                return Err(Invalid::at(
                    "name",
                    format!("group {} is listed twice", g.name),
                ));
            }
            if g.gid == 0 {
                // an existing local group of that name is adopted
                g.gid = match sys_groups
                    .iter()
                    .find(|s| s.name == g.name && in_range(s.gid))
                {
                    Some(s) => s.gid,
                    None => next_free_id(&gids).ok_or("no free gid left")?,
                };
                gids.insert(g.gid);
            }
            if !in_range(g.gid) {
                return Err(Invalid::at(
                    "gid",
                    format!(
                        "group {}: gid {} is outside {LOCAL_ID_MIN}-{LOCAL_ID_MAX}",
                        g.name, g.gid
                    ),
                ));
            }
            if let Some(s) = sys_groups
                .iter()
                .find(|s| s.name == g.name && s.gid != g.gid)
            {
                return Err(Invalid::at(
                    "name",
                    format!("group {} already exists with gid {}", g.name, s.gid),
                ));
            }
            if let Some(s) = sys_groups
                .iter()
                .find(|s| s.gid == g.gid && s.name != g.name)
            {
                return Err(Invalid::at(
                    "gid",
                    format!("gid {} is already used by group {}", g.gid, s.name),
                ));
            }
        }
        let mut by_gid = BTreeSet::new();
        for g in &c.groups {
            if !by_gid.insert(g.gid) {
                return Err(format!("gid {} is used twice", g.gid).into());
            }
        }
        let group_names: BTreeSet<&str> = c.groups.iter().map(|g| g.name.as_str()).collect();

        // users
        let mut uids: BTreeSet<u32> = sys_users.iter().map(|u| u.uid).collect();
        uids.extend(c.users.iter().map(|u| u.uid));
        let mut seen = BTreeSet::new();
        for u in &mut c.users {
            u.name = u.name.trim().to_string();
            u.full_name = u.full_name.trim().to_string();
            u.email = u.email.trim().to_string();
            u.description = u.description.trim().to_string();
            let n = u.name.clone();
            if !account_name(&n) {
                return Err(Invalid::at(
                    "name",
                    format!(
                        "invalid user name {n:?}: use a-z, 0-9, '_' and '-', starting with a letter or '_' (max 32)"
                    ),
                ));
            }
            if validate::RESERVED_NAMES.contains(&n.as_str()) {
                return Err(Invalid::at(
                    "name",
                    format!("{n} is a system account and cannot be managed here"),
                ));
            }
            if !seen.insert(n.clone()) {
                return Err(Invalid::at("name", format!("user {n} is listed twice")));
            }
            one_line(&u.full_name, 128)
                .map_err(|e| Invalid::at("full_name", format!("user {n}: full name {e}")))?;
            if u.full_name.contains([':', ',']) {
                return Err(Invalid::at(
                    "full_name",
                    format!("user {n}: the full name cannot contain ':' or ','"),
                ));
            }
            one_line(&u.description, 256)
                .map_err(|e| Invalid::at("description", format!("user {n}: description {e}")))?;
            one_line(&u.email, 254)
                .map_err(|e| Invalid::at("email", format!("user {n}: email {e}")))?;
            if !u.email.is_empty()
                && (!u.email.contains('@') || u.email.contains([' ', ':', ',', '<', '>']))
            {
                return Err(Invalid::at(
                    "email",
                    format!("user {n}: {:?} is not an email address", u.email),
                ));
            }
            if let Some(d) = &u.expires
                && validate::date_to_days(d).is_none()
            {
                return Err(Invalid::at(
                    "expires",
                    format!("user {n}: expiry {d:?} is not a date (YYYY-MM-DD)"),
                ));
            }
            if u.home.is_empty() {
                u.home = format!("/home/{n}");
            }
            if !u.home.starts_with('/')
                || u.home.contains([':', '\n', '"', '\\'])
                || u.home.chars().any(char::is_control)
                || u.home[1..]
                    .split('/')
                    .any(|c| c.is_empty() || c == "." || c == "..")
            {
                return Err(Invalid::at(
                    "home",
                    format!("user {n}: invalid home directory {:?}", u.home),
                ));
            }
            u.groups = u.groups.iter().map(|g| g.trim().to_string()).collect();
            u.groups.sort();
            u.groups.dedup();
            for g in &u.groups {
                if !group_names.contains(g.as_str()) {
                    return Err(Invalid::at(
                        "groups",
                        format!("user {n}: group {g} is not a local group managed by rynas"),
                    ));
                }
            }
            if u.uid == 0 {
                // an existing local user of that name is adopted
                u.uid = match sys_users.iter().find(|s| s.name == n && in_range(s.uid)) {
                    Some(s) => s.uid,
                    None => next_free_id(&uids).ok_or("no free uid left")?,
                };
                uids.insert(u.uid);
            }
            if !in_range(u.uid) {
                return Err(Invalid::at(
                    "uid",
                    format!(
                        "user {n}: uid {} is outside {LOCAL_ID_MIN}-{LOCAL_ID_MAX}",
                        u.uid
                    ),
                ));
            }
            if let Some(s) = sys_users.iter().find(|s| s.name == n && s.uid != u.uid) {
                return Err(Invalid::at(
                    "name",
                    format!("user {n} already exists with uid {}", s.uid),
                ));
            }
            if let Some(s) = sys_users.iter().find(|s| s.uid == u.uid && s.name != n) {
                return Err(Invalid::at(
                    "uid",
                    format!("uid {} is already used by {}", u.uid, s.name),
                ));
            }
        }
        let mut by_uid = BTreeSet::new();
        for u in &c.users {
            if !by_uid.insert(u.uid) {
                return Err(format!("uid {} is used twice", u.uid).into());
            }
        }
        c.users.sort_by(|a, b| a.name.cmp(&b.name));
        c.groups.sort_by(|a, b| a.name.cmp(&b.name));
        c.remove_homes.sort();
        c.remove_homes.dedup();
        c.remove_homes.retain(|n| !seen.contains(n));
        for n in &c.remove_homes {
            if !account_name(n) {
                return Err(format!("invalid user name {n:?} in remove_homes").into());
            }
        }
        Ok(c)
    }

    /// The system's users and groups in the local range, as a document.
    pub fn import_now(&self) -> Result<UsersConfig, String> {
        let users = self.accounts.users()?;
        let groups = self.accounts.groups()?;
        let samba = self.accounts.samba_users()?;
        Ok(import_from(&users, &groups, samba.as_deref()))
    }
}

/// Builds a document from the system state.
pub fn import_from(
    users: &[SysUser],
    groups: &[SysGroup],
    samba: Option<&[SambaUser]>,
) -> UsersConfig {
    let local_groups: Vec<&SysGroup> = groups
        .iter()
        .filter(|g| in_range(g.gid) && !validate::RESERVED_NAMES.contains(&g.name.as_str()))
        .collect();
    let member = |g: &str, u: &str| {
        groups
            .iter()
            .any(|x| x.name == g && x.members.iter().any(|m| m == u))
    };
    let mut c = UsersConfig::default();
    for g in &local_groups {
        c.groups.push(LocalGroup {
            name: g.name.clone(),
            gid: g.gid,
            description: String::new(),
        });
    }
    for u in users
        .iter()
        .filter(|u| in_range(u.uid) && account_name(&u.name))
    {
        let sm = samba.and_then(|s| s.iter().find(|x| x.name == u.name));
        let shell = if member(SFTP_GROUP, &u.name) {
            ShellAccess::Sftp
        } else if is_login_shell(&u.shell) {
            ShellAccess::Shell
        } else {
            ShellAccess::None
        };
        let disabled = u.expire_days == Some(1) || sm.is_some_and(|s| s.disabled);
        let expires = u.expire_days.filter(|d| *d > 1).map(validate::days_to_date);
        c.users.push(LocalUser {
            name: u.name.clone(),
            uid: u.uid,
            full_name: u.full_name.clone(),
            email: String::new(),
            description: String::new(),
            groups: local_groups
                .iter()
                .filter(|g| g.members.contains(&u.name))
                .map(|g| g.name.clone())
                .collect(),
            admin: member(ADMIN_GROUP, &u.name),
            shell,
            expires,
            disabled,
            smb: sm.is_some(),
            home: u.home.clone(),
        });
    }
    c.users.sort_by(|a, b| a.name.cmp(&b.name));
    c.groups.sort_by(|a, b| a.name.cmp(&b.name));
    c
}

/// What differs between a configured user and its system account (for usermod).
fn user_mod(u: &LocalUser, s: &SysUser) -> Option<AccountOp> {
    let full_name = (u.full_name != s.full_name).then(|| u.full_name.clone());
    let want_login = u.shell == ShellAccess::Shell;
    // a user with shell access keeps the login shell it has (zsh...)
    let shell = (want_login != is_login_shell(&s.shell)).then(|| shell_of(u).to_string());
    let e = expire_of(u);
    let expire = (e.days() != s.expire_days).then_some(e);
    if full_name.is_none() && shell.is_none() && expire.is_none() {
        return None;
    }
    Some(AccountOp::UserMod {
        name: u.name.clone(),
        full_name,
        shell,
        expire,
        home: None,
    })
}

impl Applier for UsersDomain {
    fn key(&self) -> &str {
        USERS_KEY
    }

    fn order(&self) -> i32 {
        10
    }

    fn validate(&self, value: Value) -> Result<Value, Invalid> {
        let c = parse(Some(&value))?;
        let c = self.normalize(c)?;
        serde_json::to_value(c).map_err(|e| e.to_string().into())
    }

    fn render(&self, _value: &Value, _ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        Ok(Vec::new())
    }

    fn reconcile(
        &self,
        from: Option<&Value>,
        to: Option<&Value>,
        ctx: &ApplyCtx,
    ) -> Result<(), String> {
        let Some(to) = to else {
            (ctx.log)("users: no longer managed, accounts left as they are");
            return Ok(());
        };
        let from = parse(from)?;
        let to = parse(Some(to))?;
        let sys_users = self.accounts.users()?;
        let sys_groups = self.accounts.groups()?;
        let group_exists = |n: &str| sys_groups.iter().any(|g| g.name == n);

        // 1. groups: the special ones when needed, then the local ones
        let mut special: BTreeSet<&str> = BTreeSet::new();
        for u in &to.users {
            for g in wanted_groups(u) {
                if [ADMIN_GROUP, SSH_GROUP, SFTP_GROUP].contains(&g.as_str()) {
                    special.insert(match g.as_str() {
                        ADMIN_GROUP => ADMIN_GROUP,
                        SSH_GROUP => SSH_GROUP,
                        _ => SFTP_GROUP,
                    });
                }
            }
        }
        for g in special {
            if !group_exists(g) {
                self.exec(
                    AccountOp::GroupAdd {
                        name: g.into(),
                        gid: None,
                    },
                    ctx,
                )?;
            }
        }
        for g in &to.groups {
            match sys_groups.iter().find(|s| s.name == g.name) {
                None => self.exec(
                    AccountOp::GroupAdd {
                        name: g.name.clone(),
                        gid: Some(g.gid),
                    },
                    ctx,
                )?,
                Some(s) if s.gid != g.gid => {
                    return Err(format!(
                        "group {} exists with gid {}, the configuration says {}",
                        g.name, s.gid, g.gid
                    ));
                }
                Some(_) => {}
            }
        }

        // 2. users: create or bring in line
        let primary_gid = sys_groups
            .iter()
            .find(|g| g.name == PRIMARY_GROUP)
            .map(|g| g.gid)
            .unwrap_or(100);
        for u in &to.users {
            match sys_users.iter().find(|s| s.name == u.name) {
                None => {
                    // a home in the homes folder: made by rynas (a subvolume), before useradd
                    let made = match homes::link_user(&u.home) {
                        Some(n) if n == u.name => {
                            let h = self.homes_host()?;
                            Some(homes::create_home(
                                h,
                                &u.name,
                                u.uid,
                                primary_gid,
                                u.shell == ShellAccess::Shell,
                                ctx.log,
                            )?)
                        }
                        Some(_) => {
                            return Err(format!(
                                "user {}: a home in the homes folder must be named after the user",
                                u.name
                            ));
                        }
                        None => None,
                    };
                    let r = self.exec(
                        AccountOp::UserAdd {
                            name: u.name.clone(),
                            uid: u.uid,
                            group: PRIMARY_GROUP.into(),
                            full_name: u.full_name.clone(),
                            home: u.home.clone(),
                            create_home: made.is_none(),
                            shell: shell_of(u).into(),
                            expire: expire_of(u),
                        },
                        ctx,
                    );
                    if let Err(e) = r {
                        if made.is_some() {
                            let _ = homes::remove_home(self.homes_host()?, &u.home, ctx.log);
                        }
                        return Err(e);
                    }
                }
                Some(s) if s.uid != u.uid => {
                    return Err(format!(
                        "user {} exists with uid {}, the configuration says {}",
                        u.name, s.uid, u.uid
                    ));
                }
                Some(s) => {
                    if let Some(op) = user_mod(u, s) {
                        self.exec(op, ctx)?;
                    }
                    // a moved home (and the rollback of a move): only an explicit change
                    // between the two values, never a mismatch with the system
                    if let Some(prev) = from.users.iter().find(|x| x.name == u.name)
                        && prev.home != u.home
                        && s.home == prev.home
                    {
                        let ok = match &self.homes {
                            Some(h) => h.probe(&u.home).is_dir,
                            None => std::path::Path::new(&u.home).is_dir(),
                        };
                        if !ok {
                            return Err(format!(
                                "user {}: the new home {} does not exist",
                                u.name, u.home
                            ));
                        }
                        self.exec(
                            AccountOp::UserMod {
                                name: u.name.clone(),
                                full_name: None,
                                shell: None,
                                expire: None,
                                home: Some(u.home.clone()),
                            },
                            ctx,
                        )?;
                    }
                }
            }
        }

        // 3. memberships, among the groups rynas manages only. The special groups (admin,
        // ssh, sftp) are touched only for new users and when the admin flag or the shell
        // access changed: an imported account with a login shell is not pulled into
        // rynas-ssh by an unrelated apply (or its rollback).
        let groups_now = self.accounts.groups()?;
        let mut regular: BTreeSet<String> = to.groups.iter().map(|g| g.name.clone()).collect();
        regular.extend(from.groups.iter().map(|g| g.name.clone()));
        let special = |u: &LocalUser| -> BTreeSet<String> {
            wanted_groups(u)
                .into_iter()
                .filter(|g| SPECIAL_GROUPS.contains(&g.as_str()))
                .collect()
        };
        for u in &to.users {
            let prev = from.users.iter().find(|x| x.name == u.name);
            let mut managed = regular.clone();
            if prev.is_none_or(|p| special(p) != special(u)) {
                managed.extend(SPECIAL_GROUPS.iter().map(|g| g.to_string()));
            }
            let want: BTreeSet<String> = wanted_groups(u)
                .into_iter()
                .filter(|g| managed.contains(g))
                .collect();
            let have: BTreeSet<String> = groups_now
                .iter()
                .filter(|g| managed.contains(&g.name) && g.members.contains(&u.name))
                .map(|g| g.name.clone())
                .collect();
            for g in want.difference(&have) {
                self.exec(
                    AccountOp::MemberAdd {
                        group: g.clone(),
                        user: u.name.clone(),
                    },
                    ctx,
                )?;
            }
            for g in have.difference(&want) {
                self.exec(
                    AccountOp::MemberDel {
                        group: g.clone(),
                        user: u.name.clone(),
                    },
                    ctx,
                )?;
            }
        }

        // 4. Samba accounts (only without a password change: removal and flags)
        let samba = self.accounts.samba_users()?;
        if let Some(samba) = &samba {
            for u in &to.users {
                match samba.iter().find(|s| s.name == u.name) {
                    Some(_) if !u.smb => self.exec(
                        AccountOp::SambaDel {
                            name: u.name.clone(),
                        },
                        ctx,
                    )?,
                    Some(s) if s.disabled != u.disabled => self.exec(
                        AccountOp::SambaFlags {
                            name: u.name.clone(),
                            disabled: u.disabled,
                        },
                        ctx,
                    )?,
                    None if u.smb => (ctx.log)(&format!(
                        "{}: no Samba account yet, set the password to create it",
                        u.name
                    )),
                    _ => {}
                }
            }
        } else if to.users.iter().any(|u| u.smb) {
            (ctx.log)("Samba is not installed: SMB access takes effect once it is");
        }

        // 5. deletions of what the previous value had
        let keep: BTreeSet<&str> = to.users.iter().map(|u| u.name.as_str()).collect();
        for u in from
            .users
            .iter()
            .filter(|u| !keep.contains(u.name.as_str()))
        {
            if !sys_users.iter().any(|s| s.name == u.name) {
                continue;
            }
            if samba
                .as_ref()
                .is_some_and(|s| s.iter().any(|x| x.name == u.name))
            {
                self.exec(
                    AccountOp::SambaDel {
                        name: u.name.clone(),
                    },
                    ctx,
                )?;
            }
            let remove = to.remove_homes.contains(&u.name);
            let sys_home = sys_users
                .iter()
                .find(|s| s.name == u.name)
                .map(|s| s.home.clone())
                .unwrap_or_default();
            // a home in the homes folder is a subvolume: rynas deletes it after userdel
            let in_folder = remove && homes::link_user(&sys_home) == Some(u.name.as_str());
            self.exec(
                AccountOp::UserDel {
                    name: u.name.clone(),
                    remove_home: remove && !in_folder,
                },
                ctx,
            )?;
            if in_folder {
                homes::remove_home(self.homes_host()?, &sys_home, ctx.log)?;
            }
        }
        let keep: BTreeSet<&str> = to.groups.iter().map(|g| g.name.as_str()).collect();
        for g in from
            .groups
            .iter()
            .filter(|g| !keep.contains(g.name.as_str()))
        {
            if group_exists(&g.name) {
                self.exec(
                    AccountOp::GroupDel {
                        name: g.name.clone(),
                    },
                    ctx,
                )?;
            }
        }
        Ok(())
    }

    fn health(&self, value: Option<&Value>, _ctx: &ApplyCtx) -> Result<(), String> {
        let c = parse(value)?;
        let users = self.accounts.users()?;
        let groups = self.accounts.groups()?;
        for u in &c.users {
            if !users.iter().any(|s| s.name == u.name && s.uid == u.uid) {
                return Err(format!(
                    "user {} (uid {}) is not on the system",
                    u.name, u.uid
                ));
            }
        }
        for g in &c.groups {
            if !groups.iter().any(|s| s.name == g.name && s.gid == g.gid) {
                return Err(format!(
                    "group {} (gid {}) is not on the system",
                    g.name, g.gid
                ));
            }
        }
        Ok(())
    }

    fn import(&self, _ctx: &ApplyCtx) -> Result<Option<Value>, String> {
        let c = self.import_now()?;
        serde_json::to_value(c).map(Some).map_err(|e| e.to_string())
    }

    fn state_drift(
        &self,
        applied: Option<&Value>,
        _ctx: &ApplyCtx,
    ) -> Result<Vec<StateDrift>, String> {
        let users = self.accounts.users()?;
        let groups = self.accounts.groups()?;
        let samba = self.accounts.samba_users()?;
        let mut out = Vec::new();
        let mut push = |object: String, kind: StateDriftKind, code, name: &str, id: Option<u32>| {
            let args = StateDriftArgs {
                name: Some(name.to_string()),
                id,
                ..Default::default()
            };
            out.push(crate::drift(USERS_KEY, object, kind, code, args))
        };
        if let Some(samba) = &samba {
            for s in samba {
                if !users.iter().any(|u| u.name == s.name) {
                    push(
                        format!("user:{}", s.name),
                        StateDriftKind::MissingInPasswd,
                        StateDriftCode::SambaWithoutUser,
                        &s.name,
                        None,
                    );
                }
            }
        }
        let Some(applied) = applied else {
            return Ok(out);
        };
        let c = parse(Some(applied))?;
        let configured: BTreeMap<&str, &LocalUser> =
            c.users.iter().map(|u| (u.name.as_str(), u)).collect();
        for s in users.iter().filter(|s| in_range(s.uid)) {
            if !configured.contains_key(s.name.as_str()) {
                push(
                    format!("user:{}", s.name),
                    StateDriftKind::Unmanaged,
                    StateDriftCode::UserUnmanaged,
                    &s.name,
                    Some(s.uid),
                );
            }
        }
        for u in &c.users {
            let Some(s) = users.iter().find(|s| s.name == u.name) else {
                push(
                    format!("user:{}", u.name),
                    StateDriftKind::Missing,
                    StateDriftCode::UserMissing,
                    &u.name,
                    None,
                );
                continue;
            };
            if user_mod(u, s).is_some() || s.uid != u.uid {
                push(
                    format!("user:{}", u.name),
                    StateDriftKind::Mismatch,
                    StateDriftCode::UserMismatch,
                    &u.name,
                    None,
                );
            }
            if u.smb
                && let Some(samba) = &samba
                && !samba.iter().any(|x| x.name == u.name)
            {
                push(
                    format!("user:{}", u.name),
                    StateDriftKind::MissingInSamba,
                    StateDriftCode::UserWithoutSamba,
                    &u.name,
                    None,
                );
            }
        }
        let gnames: BTreeSet<&str> = c.groups.iter().map(|g| g.name.as_str()).collect();
        for g in &c.groups {
            if !groups.iter().any(|s| s.name == g.name) {
                push(
                    format!("group:{}", g.name),
                    StateDriftKind::Missing,
                    StateDriftCode::GroupMissing,
                    &g.name,
                    None,
                );
            }
        }
        for g in groups.iter().filter(|g| in_range(g.gid)) {
            if !gnames.contains(g.name.as_str()) {
                push(
                    format!("group:{}", g.name),
                    StateDriftKind::Unmanaged,
                    StateDriftCode::GroupUnmanaged,
                    &g.name,
                    Some(g.gid),
                );
            }
        }
        Ok(out)
    }
}

/// Password rules (phase 1): 8 to 256 characters, no control characters, not the user name.
pub fn check_password(name: &str, p: &str) -> Result<(), String> {
    let n = p.chars().count();
    if n < 8 {
        return Err("the password must have at least 8 characters".into());
    }
    if n > 256 {
        return Err("the password can have at most 256 characters".into());
    }
    if p.chars().any(char::is_control) {
        return Err("the password cannot contain control characters".into());
    }
    if p == name {
        return Err("the password cannot be the user name".into());
    }
    Ok(())
}

/// Sets the Linux password (`chpasswd`, yescrypt) and, with `smb`, the Samba password
/// (`pdbedit -a` for a new Samba account, `smbpasswd` for an existing one) as one
/// transaction: if the Samba step fails the previous shadow hash is put back.
pub fn set_password(
    accounts: &dyn Accounts,
    name: &str,
    password: &Secret,
    smb: bool,
    disabled: bool,
    log: &dyn Fn(&str),
) -> Result<(), String> {
    check_password(name, password.expose())?;
    let users = accounts.users()?;
    match users.iter().find(|u| u.name == name) {
        None => return Err(format!("user {name} does not exist on the system")),
        Some(u) if !in_range(u.uid) => {
            return Err(format!("{name} is not a local user managed by rynas"));
        }
        Some(_) => {}
    }
    let exec = |op: AccountOp| {
        log(&op.describe());
        accounts.exec(&op)
    };
    let previous = accounts.shadow_hash(name)?;
    exec(AccountOp::SetShadowPassword {
        name: name.into(),
        password: password.clone(),
    })?;
    if !smb {
        return Ok(());
    }
    let Some(samba) = accounts.samba_users()? else {
        log("Samba is not installed: only the Linux password was set");
        return Ok(());
    };
    let r = if samba.iter().any(|s| s.name == name) {
        exec(AccountOp::SambaSetPassword {
            name: name.into(),
            password: password.clone(),
        })
    } else {
        exec(AccountOp::SambaAdd {
            name: name.into(),
            password: password.clone(),
        })
        .and_then(|_| {
            if disabled {
                exec(AccountOp::SambaFlags {
                    name: name.into(),
                    disabled: true,
                })
            } else {
                Ok(())
            }
        })
    };
    if let Err(e) = r {
        log(&format!("Samba password failed: {e}"));
        if let Some(h) = previous {
            match exec(AccountOp::RestoreShadowHash {
                name: name.into(),
                hash: h,
            }) {
                Ok(()) => {}
                Err(e2) => log(&format!("restoring the previous password failed: {e2}")),
            }
        }
        return Err(format!(
            "the Samba password could not be set ({e}); the Linux password was not changed"
        ));
    }
    Ok(())
}
