//! User homes: the `homes` configuration key and the home move.
//!
//! With the service on, the homes live in `<volume>/homes/<user>` (one btrfs subvolume per
//! user when the volume is btrfs, a plain folder otherwise) and the passwd entries name them
//! through a stable path, [`HOMES_LINK`] (`/srv/rynas/homes`), a symbolic link to
//! `<volume>/homes`. The link is the only thing that changes when the folder goes to another
//! volume; a dangling link (volume not mounted) makes logins fail instead of writing into the
//! system disk below an empty mount point. See `docs/research/user-homes.md`.
//!
//! - [`HomesDomain`] (`homes`, order 5, before `users`): creates the homes folder (root,
//!   0711: users can reach their own home but not list the others) and the link. Turning the
//!   service off changes nothing on disk: homes already there keep working.
//! - New users get their home created by the `users` domain ([`create_home`]), deleted users
//!   lose it with `remove_home` ([`remove_home`]).
//! - Existing homes never move by themselves: [`plan_move`] says what a move does and what
//!   prevents it, [`copy_home`] copies a home (rsync with hard links, ACLs and xattrs, a
//!   second pass and a verification), then rynasd switches the passwd home and keeps the old
//!   folder renamed ([`kept_path`]) until the move is confirmed or undone ([`MoveStore`]).
//!
//! Filesystem access goes through [`HomesHost`]: [`HostHomes`] on the real system,
//! [`MemoryHomes`] for tests and `rynasd --dry-system`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rynas_api_types::{
    HOMES_FOLDER, HOMES_KEY, HOMES_LINK, HomeLocation, HomeMoveBlocker, HomeMoveBlockerCode as B,
    HomeMovePlan, HomeMoveRecord, HomesConfig, HomesVolume, LOCAL_ID_MAX, LOCAL_ID_MIN,
};
use rynas_config::{Applier, ApplyCtx, Invalid, ManagedFile};
use rynas_tasks::Cmd;
use serde_json::Value;

use crate::accounts::{Accounts, SysUser};
use crate::host::PathInfo;

/// Mode of the homes folder: owner root, others may traverse (to reach their own home) but
/// not list it.
pub const FOLDER_MODE: u32 = 0o711;
/// Mode of a home: the owner only.
pub const HOME_MODE: u32 = 0o700;
/// Suffix of an old home kept after a move (user names cannot contain '.').
pub const KEPT_SUFFIX: &str = ".rynas-old";
/// Time spent measuring a home for a plan.
pub const PLAN_SIZE_BUDGET: Duration = Duration::from_millis(1500);
/// A long copy is logged as it goes; it ends at this timeout at the latest.
const COPY_TIMEOUT: Duration = Duration::from_secs(48 * 3600);
const CMD_TIMEOUT: Duration = Duration::from_secs(120);

/// `/srv/rynas/homes/<user>`.
pub fn home_of(user: &str) -> String {
    format!("{HOMES_LINK}/{user}")
}

/// `<volume>/homes`.
pub fn folder_of(volume: &str) -> String {
    format!("{}/{HOMES_FOLDER}", volume.trim_end_matches('/'))
}

/// The user name when `home` is `/srv/rynas/homes/<name>`.
pub fn link_user(home: &str) -> Option<&str> {
    let rest = home.strip_prefix(HOMES_LINK)?.strip_prefix('/')?;
    (!rest.is_empty() && !rest.contains('/') && rest != "." && rest != "..").then_some(rest)
}

/// Where an old home is kept after a move.
pub fn kept_path(from: &str) -> String {
    format!("{}{KEPT_SUFFIX}", from.trim_end_matches('/'))
}

fn in_range(uid: u32) -> bool {
    (LOCAL_ID_MIN..=LOCAL_ID_MAX).contains(&uid)
}

/// A mounted filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Mount {
    pub path: String,
    pub fs_type: String,
    pub size_bytes: u64,
    pub avail_bytes: u64,
}

/// Files, folders, links and bytes of a tree (the verification of a copy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TreeCounts {
    pub files: u64,
    pub dirs: u64,
    pub links: u64,
    pub bytes: u64,
}

/// What the homes code needs from the system.
pub trait HomesHost: Send + Sync {
    /// Facts about a path (symbolic links followed; `symlink` says whether `path` is one).
    fn probe(&self, path: &str) -> PathInfo;
    /// Target of a symbolic link.
    fn read_link(&self, path: &str) -> Option<String>;
    /// Local mounted filesystems.
    fn mounts(&self) -> Vec<Mount>;
    /// Free bytes on the filesystem of `path`.
    fn avail(&self, path: &str) -> Option<u64>;
    /// Space used below `path` (one filesystem), bounded in time: `(bytes, partial)`.
    fn tree_size(&self, path: &str, budget: Duration) -> Option<(u64, bool)>;
    fn tree_counts(&self, path: &str) -> Option<TreeCounts>;
    /// Owner uid of a folder and whether it is empty.
    fn owner_empty(&self, path: &str) -> Option<(u32, bool)>;
    /// Processes running with this uid (real, effective, saved or filesystem).
    fn processes(&self, uid: u32) -> Vec<u32>;
    /// Open SMB sessions of a user; `None` when it cannot be known (no Samba).
    fn smb_sessions(&self, user: &str, uid: u32) -> Option<usize>;
    fn has_program(&self, name: &str) -> bool;

    /// Creates `path` (a btrfs subvolume when `subvolume`) with this owner and mode.
    fn make_dir(
        &self,
        path: &str,
        subvolume: bool,
        uid: u32,
        gid: u32,
        mode: u32,
        log: &dyn Fn(&str),
    ) -> Result<(), String>;
    /// Copies `/etc/skel` into a new home, owned by the user.
    fn copy_skel(&self, home: &str, uid: u32, gid: u32, log: &dyn Fn(&str)) -> Result<(), String>;
    /// Points the symbolic link `link` at `target` (atomically replaced).
    fn set_link(&self, link: &str, target: &str, log: &dyn Fn(&str)) -> Result<(), String>;
    fn rename(&self, from: &str, to: &str, log: &dyn Fn(&str)) -> Result<(), String>;
    fn set_owner_mode(
        &self,
        path: &str,
        uid: u32,
        gid: u32,
        mode: u32,
        log: &dyn Fn(&str),
    ) -> Result<(), String>;
    /// Deletes a tree (`btrfs subvolume delete` for a subvolume).
    fn remove(&self, path: &str, subvolume: bool, log: &dyn Fn(&str)) -> Result<(), String>;
    /// One rsync pass `from/` → `to/` (hard links, ACLs, xattrs, numeric ids, one
    /// filesystem); `delete` removes what is gone from the source. Stops on `cancelled`.
    fn sync(
        &self,
        from: &str,
        to: &str,
        delete: bool,
        log: &dyn Fn(&str),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String>;
    /// What a further pass would still change (rsync dry run), the root folder left out.
    fn differences(&self, from: &str, to: &str) -> Result<Vec<String>, String>;
}

// ---------------------------------------------------------------------------------------------
// settings
// ---------------------------------------------------------------------------------------------

/// System paths that cannot hold the homes folder.
const SYSTEM_MOUNTS: [&str; 14] = [
    "/",
    "/boot",
    "/efi",
    "/usr",
    "/var",
    "/etc",
    "/tmp",
    "/run",
    "/proc",
    "/sys",
    "/dev",
    "/srv/rynas",
    "/media",
    "/mnt/removable",
];
/// Filesystems that can hold homes (local, POSIX permissions and ACLs).
const HOME_FS: [&str; 5] = ["btrfs", "ext4", "xfs", "f2fs", "zfs"];

fn system_path(p: &str) -> bool {
    SYSTEM_MOUNTS
        .iter()
        .any(|s| p == *s || (*s != "/" && p.starts_with(&format!("{s}/"))))
        || p.starts_with("/boot/")
        || p.starts_with("/var/")
        || p.starts_with("/run/")
}

/// Filesystems that can hold the homes folder, with the rynas volume labels.
pub fn candidates(mounts: &[Mount], rynas: &[(String, String)]) -> Vec<HomesVolume> {
    let mut v: Vec<HomesVolume> = mounts
        .iter()
        .filter(|m| HOME_FS.contains(&m.fs_type.as_str()) && !system_path(&m.path))
        .map(|m| HomesVolume {
            path: m.path.clone(),
            label: rynas
                .iter()
                .find(|(_, mp)| mp.trim_end_matches('/') == m.path)
                .map(|(l, _)| l.clone()),
            fs_type: m.fs_type.clone(),
            size_bytes: m.size_bytes,
            avail_bytes: m.avail_bytes,
        })
        .collect();
    v.sort_by(|a, b| (a.label.is_none(), &a.path).cmp(&(b.label.is_none(), &b.path)));
    v.dedup_by(|a, b| a.path == b.path);
    v
}

pub fn parse(v: Option<&Value>) -> Result<HomesConfig, String> {
    match v {
        Some(v) => {
            serde_json::from_value(v.clone()).map_err(|e| format!("invalid homes document: {e}"))
        }
        None => Ok(HomesConfig::default()),
    }
}

/// Users whose passwd home is in the homes folder.
pub fn users_on_link(users: &[SysUser]) -> Vec<String> {
    users
        .iter()
        .filter(|u| link_user(&u.home).is_some())
        .map(|u| u.name.clone())
        .collect()
}

pub struct HomesDomain {
    host: Arc<dyn HomesHost>,
    accounts: Arc<dyn Accounts>,
}

impl HomesDomain {
    pub fn new(host: Arc<dyn HomesHost>, accounts: Arc<dyn Accounts>) -> Self {
        Self { host, accounts }
    }

    pub fn host(&self) -> &Arc<dyn HomesHost> {
        &self.host
    }

    /// The folder the link points at now.
    pub fn current_folder(&self) -> Option<String> {
        self.host.read_link(HOMES_LINK)
    }

    pub fn normalize(&self, mut c: HomesConfig) -> Result<HomesConfig, Invalid> {
        c.volume = c.volume.trim().to_string();
        if c.volume.len() > 1 {
            c.volume = c.volume.trim_end_matches('/').to_string();
        }
        if c.volume.is_empty() {
            if c.enabled {
                return Err(Invalid::at(
                    "volume",
                    "choose the volume of the homes folder",
                ));
            }
            return Ok(c);
        }
        if !c.volume.starts_with('/')
            || c.volume.chars().any(char::is_control)
            || c.volume.contains(['\n', ':', '"', '\\'])
            || c.volume[1..]
                .split('/')
                .any(|x| x.is_empty() || x == "." || x == "..")
        {
            return Err(Invalid::at(
                "volume",
                format!("invalid volume path {:?}", c.volume),
            ));
        }
        if !c.enabled {
            return Ok(c);
        }
        if system_path(&c.volume) {
            return Err(Invalid::at(
                "volume",
                format!("{} is a system folder: choose a data volume", c.volume),
            ));
        }
        let mounts = self.host.mounts();
        let Some(m) = mounts.iter().find(|m| m.path == c.volume) else {
            return Err(Invalid::at(
                "volume",
                format!("{} is not a mounted volume", c.volume),
            ));
        };
        if !HOME_FS.contains(&m.fs_type.as_str()) {
            return Err(Invalid::at(
                "volume",
                format!(
                    "{} is {}: homes need btrfs, ext4, xfs, f2fs or zfs",
                    c.volume, m.fs_type
                ),
            ));
        }
        // another folder while homes are in the current one: their files would stay behind
        let folder = folder_of(&c.volume);
        if let Some(cur) = self.current_folder()
            && cur != folder
        {
            let users = self.accounts.users()?;
            let on = users_on_link(&users);
            if !on.is_empty() {
                return Err(Invalid::at(
                    "volume",
                    format!(
                        "the homes of {} are in {cur}: the homes folder cannot change volume while it holds homes",
                        on.join(", ")
                    ),
                ));
            }
        }
        Ok(c)
    }

    fn ensure(&self, c: &HomesConfig, log: &dyn Fn(&str)) -> Result<(), String> {
        let folder = folder_of(&c.volume);
        let vol = self.host.probe(&c.volume);
        if !vol.exists || !vol.is_dir {
            return Err(format!("{} is not mounted", c.volume));
        }
        let f = self.host.probe(&folder);
        if !f.exists {
            self.host
                .make_dir(&folder, vol.fs_type == "btrfs", 0, 0, FOLDER_MODE, log)?;
        } else if !f.is_dir || f.symlink {
            return Err(format!("{folder} exists and is not a folder"));
        }
        match self.host.read_link(HOMES_LINK) {
            Some(t) if t == folder => {}
            Some(t) => {
                log(&format!("{HOMES_LINK}: {t} → {folder}"));
                self.host.set_link(HOMES_LINK, &folder, log)?;
            }
            None => {
                if self.host.probe(HOMES_LINK).exists {
                    return Err(format!(
                        "{HOMES_LINK} exists and is not a symbolic link: rynas does not replace it"
                    ));
                }
                let parent = HOMES_LINK.rsplit_once('/').map(|p| p.0).unwrap_or("/srv");
                if !self.host.probe(parent).exists {
                    self.host.make_dir(parent, false, 0, 0, 0o755, log)?;
                }
                self.host.set_link(HOMES_LINK, &folder, log)?;
            }
        }
        Ok(())
    }
}

impl Applier for HomesDomain {
    fn key(&self) -> &str {
        HOMES_KEY
    }

    fn order(&self) -> i32 {
        5
    }

    fn validate(&self, value: Value) -> Result<Value, Invalid> {
        let c = self.normalize(parse(Some(&value))?)?;
        serde_json::to_value(c).map_err(|e| e.to_string().into())
    }

    fn render(&self, _value: &Value, _ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        Ok(Vec::new())
    }

    fn reconcile(
        &self,
        _from: Option<&Value>,
        to: Option<&Value>,
        ctx: &ApplyCtx,
    ) -> Result<(), String> {
        let c = parse(to)?;
        if !c.enabled {
            (ctx.log)("user homes off: new users get /home/<user>, existing homes stay");
            return Ok(());
        }
        self.ensure(&c, ctx.log)
    }

    fn health(&self, value: Option<&Value>, _ctx: &ApplyCtx) -> Result<(), String> {
        let c = parse(value)?;
        if !c.enabled {
            return Ok(());
        }
        let folder = folder_of(&c.volume);
        if self.host.read_link(HOMES_LINK).as_deref() != Some(folder.as_str()) {
            return Err(format!("{HOMES_LINK} does not point at {folder}"));
        }
        if !self.host.probe(HOMES_LINK).is_dir {
            return Err(format!("{folder} is not reachable"));
        }
        Ok(())
    }

    fn import(&self, _ctx: &ApplyCtx) -> Result<Option<Value>, String> {
        let c = match self.host.read_link(HOMES_LINK) {
            Some(t) => match t.strip_suffix(&format!("/{HOMES_FOLDER}")) {
                Some(v) if !v.is_empty() => HomesConfig {
                    enabled: true,
                    volume: v.to_string(),
                },
                _ => HomesConfig::default(),
            },
            None => HomesConfig::default(),
        };
        serde_json::to_value(c).map(Some).map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------------------------
// homes of users
// ---------------------------------------------------------------------------------------------

/// The real folder of `/srv/rynas/homes/<user>`.
pub fn real_home(host: &dyn HomesHost, user: &str) -> Result<String, String> {
    let folder = host.read_link(HOMES_LINK).ok_or_else(|| {
        format!("user homes are not set up: {HOMES_LINK} does not exist (turn the service on)")
    })?;
    Ok(format!("{folder}/{user}"))
}

/// Creates the home of a new user in the homes folder: a subvolume on btrfs, owner only,
/// `/etc/skel` copied for users with a shell (the others never read dotfiles).
pub fn create_home(
    host: &dyn HomesHost,
    user: &str,
    uid: u32,
    gid: u32,
    skel: bool,
    log: &dyn Fn(&str),
) -> Result<String, String> {
    let real = real_home(host, user)?;
    let folder = real.rsplit_once('/').map(|p| p.0).unwrap_or("");
    let f = host.probe(folder);
    if !f.is_dir {
        return Err(format!("the homes folder {folder} is not reachable"));
    }
    if host.probe(&real).exists {
        // an empty folder of this uid: left by a rolled back apply, reused
        if host.owner_empty(&real) == Some((uid, true)) && !host.probe(&real).symlink {
            log(&format!("{real}: empty home of uid {uid} reused"));
            host.set_owner_mode(&real, uid, gid, HOME_MODE, log)?;
            if skel && let Err(e) = host.copy_skel(&real, uid, gid, log) {
                log(&format!("{real}: skeleton files not copied: {e}"));
            }
            return Ok(real);
        }
        return Err(format!(
            "{real} exists already (the home of a deleted user?): remove or rename it first"
        ));
    }
    host.make_dir(&real, f.fs_type == "btrfs", uid, gid, HOME_MODE, log)?;
    if skel && let Err(e) = host.copy_skel(&real, uid, gid, log) {
        log(&format!("{real}: skeleton files not copied: {e}"));
    }
    Ok(real)
}

/// Deletes the home of a deleted user when it is in the homes folder (its subvolume);
/// `false` when `home` is not there (userdel -r handles it).
pub fn remove_home(host: &dyn HomesHost, home: &str, log: &dyn Fn(&str)) -> Result<bool, String> {
    let Some(user) = link_user(home) else {
        return Ok(false);
    };
    let real = real_home(host, user)?;
    let i = host.probe(&real);
    if !i.exists {
        log(&format!("{real} does not exist any more"));
        return Ok(true);
    }
    if i.symlink {
        return Err(format!("{real} is a symbolic link: not deleted"));
    }
    host.remove(&real, i.subvolume, log)?;
    log(&format!("{real} deleted"));
    Ok(true)
}

/// Where a passwd home is.
pub fn location(host: &dyn HomesHost, home: &str) -> (HomeLocation, bool) {
    let i = host.probe(home);
    if !i.exists || !i.is_dir {
        return (HomeLocation::Missing, false);
    }
    let loc = if link_user(home).is_some() {
        HomeLocation::Volume
    } else if home.starts_with("/home/") {
        HomeLocation::System
    } else {
        HomeLocation::Other
    };
    (loc, i.subvolume)
}

// ---------------------------------------------------------------------------------------------
// moves
// ---------------------------------------------------------------------------------------------

/// What [`plan_move`] needs to know about the rest of the configuration.
pub struct MoveContext<'a> {
    /// The service is on in the applied configuration and its link works.
    pub service_on: bool,
    /// Paths of the shares (applied and staged).
    pub shares: &'a [(String, String)],
    /// A move of this user waits for confirmation.
    pub waiting: bool,
}

fn blocker(code: B, detail: impl Into<String>) -> HomeMoveBlocker {
    HomeMoveBlocker {
        code,
        detail: detail.into(),
    }
}

/// `a` is `b` or inside it.
fn within(a: &str, b: &str) -> bool {
    let b = b.trim_end_matches('/');
    a == b || a.starts_with(&format!("{b}/"))
}

/// What moving `user`'s home to the homes folder does and what prevents it now.
pub fn plan_move(host: &dyn HomesHost, user: &SysUser, cx: &MoveContext) -> HomeMovePlan {
    let to = home_of(&user.name);
    let mut p = HomeMovePlan {
        user: user.name.clone(),
        from: user.home.clone(),
        to: to.clone(),
        ..Default::default()
    };
    let mut b = Vec::new();
    if !in_range(user.uid) {
        b.push(blocker(B::NotLocal, user.uid.to_string()));
    }
    if cx.waiting {
        b.push(blocker(B::MoveWaiting, ""));
    }
    if link_user(&user.home).is_some() {
        b.push(blocker(B::AlreadyMoved, ""));
    }
    let folder = host.read_link(HOMES_LINK);
    if !cx.service_on || folder.is_none() {
        b.push(blocker(B::ServiceOff, ""));
    }
    if let Some(f) = &folder {
        p.target = format!("{f}/{}", user.name);
        let fi = host.probe(f);
        p.subvolume = fi.fs_type == "btrfs";
        p.avail_bytes = host.avail(f).unwrap_or(0);
        if host.probe(&p.target).exists {
            b.push(blocker(B::TargetExists, p.target.clone()));
        }
    }
    let src = host.probe(&user.home);
    if !src.exists || !src.is_dir {
        b.push(blocker(B::SourceMissing, user.home.clone()));
    } else if link_user(&user.home).is_none() {
        let mounts = host.mounts();
        if src.symlink || user.home == "/" {
            b.push(blocker(B::SourceNotPlain, user.home.clone()));
        } else if let Some(m) = mounts.iter().find(|m| within(&m.path, &user.home)) {
            b.push(blocker(B::SourceNotPlain, m.path.clone()));
        }
        if let Some((name, _)) = cx
            .shares
            .iter()
            .find(|(_, path)| within(path, &user.home) || within(&user.home, path))
        {
            b.push(blocker(B::ShareInside, name.clone()));
        }
        if let Some((bytes, partial)) = host.tree_size(&user.home, PLAN_SIZE_BUDGET) {
            p.size_bytes = bytes;
            p.size_partial = partial;
        }
        p.ssh_keys = host
            .probe(&format!("{}/.ssh/authorized_keys", user.home))
            .exists;
        if folder.is_some() {
            // 10 % and 64 MiB of headroom (metadata, the volume's own needs)
            let need = p.size_bytes + p.size_bytes / 10 + (64 << 20);
            if p.avail_bytes < need {
                b.push(blocker(B::NoSpace, need.to_string()));
            }
        }
    }
    b.extend(live_blockers(host, user));
    if !host.has_program("rsync") {
        b.push(blocker(B::NoRsync, ""));
    }
    p.allowed = b.is_empty();
    p.blockers = b;
    p
}

/// Running processes and SMB sessions of the user: checked by the plan, before the copy,
/// and again just before the passwd home changes.
pub fn live_blockers(host: &dyn HomesHost, user: &SysUser) -> Vec<HomeMoveBlocker> {
    let mut b = Vec::new();
    let pids = host.processes(user.uid);
    if !pids.is_empty() {
        let mut list: Vec<String> = pids.iter().take(8).map(u32::to_string).collect();
        if pids.len() > 8 {
            list.push("…".into());
        }
        b.push(blocker(B::Processes, list.join(" ")));
    }
    if let Some(n) = host.smb_sessions(&user.name, user.uid)
        && n > 0
    {
        b.push(blocker(B::SmbSessions, n.to_string()));
    }
    b
}

/// Plain text of the blockers (task logs, errors).
pub fn blockers_text(b: &[HomeMoveBlocker]) -> String {
    b.iter()
        .map(|x| {
            let what = match x.code {
                B::ServiceOff => "the user homes service is off",
                B::NotLocal => "not a local user",
                B::AlreadyMoved => "already in the homes folder",
                B::SourceMissing => "the home does not exist",
                B::SourceNotPlain => "the home is a link or has mounts",
                B::TargetExists => "the target exists",
                B::Processes => "running processes",
                B::SmbSessions => "SMB sessions",
                B::ShareInside => "a share uses the home",
                B::NoSpace => "not enough free space",
                B::NoRsync => "rsync is not installed",
                B::MoveWaiting => "a previous move waits for confirmation",
                B::Other => "other",
            };
            if x.detail.is_empty() {
                what.to_string()
            } else {
                format!("{what} ({})", x.detail)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Copies a home to its place in the homes folder: target created (subvolume on btrfs),
/// rsync pass, live check, second pass with deletions, verification (a dry run must find
/// nothing left and the counts must match), owner-only mode. On failure the copy is removed.
/// The passwd home is not touched.
pub fn copy_home(
    host: &dyn HomesHost,
    plan: &HomeMovePlan,
    user: &SysUser,
    log: &dyn Fn(&str),
    progress: &dyn Fn(u8),
    cancelled: &dyn Fn() -> bool,
) -> Result<(), String> {
    if !plan.allowed {
        return Err(format!(
            "{}: cannot move: {}",
            plan.user,
            blockers_text(&plan.blockers)
        ));
    }
    host.make_dir(
        &plan.target,
        plan.subvolume,
        user.uid,
        user.gid,
        HOME_MODE,
        log,
    )?;
    let run = || -> Result<(), String> {
        log(&format!(
            "{}: copying {} to {}",
            plan.user, plan.from, plan.target
        ));
        host.sync(&plan.from, &plan.target, false, log, cancelled)?;
        progress(70);
        let live = live_blockers(host, user);
        if !live.is_empty() {
            return Err(format!(
                "{}: the user started using the home during the copy: {}",
                plan.user,
                blockers_text(&live)
            ));
        }
        log(&format!("{}: second pass", plan.user));
        host.sync(&plan.from, &plan.target, true, log, cancelled)?;
        progress(85);
        let left = host.differences(&plan.from, &plan.target)?;
        if !left.is_empty() {
            return Err(format!(
                "{}: the copy differs from the home ({} entries, first: {})",
                plan.user,
                left.len(),
                left[0]
            ));
        }
        let (a, b) = (host.tree_counts(&plan.from), host.tree_counts(&plan.target));
        if a != b {
            return Err(format!(
                "{}: the copy has other counts than the home: {a:?} / {b:?}",
                plan.user
            ));
        }
        if let Some(c) = a {
            log(&format!(
                "{}: verified: {} files, {} folders, {} links, {} bytes",
                plan.user, c.files, c.dirs, c.links, c.bytes
            ));
        }
        host.set_owner_mode(&plan.target, user.uid, user.gid, HOME_MODE, log)
    };
    if let Err(e) = run() {
        log(&format!("{}: removing the incomplete copy", plan.user));
        if let Err(e2) = host.remove(&plan.target, plan.subvolume, log) {
            log(&format!("{}: {e2}", plan.target));
        }
        return Err(e);
    }
    Ok(())
}

/// Renames the old home out of the way; its new name (unique).
pub fn keep_old(
    host: &dyn HomesHost,
    from: &str,
    now_ms: u64,
    log: &dyn Fn(&str),
) -> Result<String, String> {
    let mut kept = kept_path(from);
    if host.probe(&kept).exists || host.probe(&kept).symlink {
        kept = format!("{kept}-{now_ms}");
    }
    host.rename(from, &kept, log)?;
    Ok(kept)
}

/// Deletes the old home of a confirmed move. Only a folder named like [`kept_path`] goes.
pub fn finish_move(
    host: &dyn HomesHost,
    r: &HomeMoveRecord,
    log: &dyn Fn(&str),
) -> Result<(), String> {
    if !r.kept.contains(KEPT_SUFFIX) {
        return Err(format!("{} is not a kept home: not deleted", r.kept));
    }
    let i = host.probe(&r.kept);
    if !i.exists {
        log(&format!("{} does not exist any more", r.kept));
        return Ok(());
    }
    if i.symlink {
        return Err(format!("{} is a symbolic link: not deleted", r.kept));
    }
    host.remove(&r.kept, i.subvolume, log)?;
    log(&format!("{} deleted", r.kept));
    Ok(())
}

/// Puts the old home back in place (the caller then switches the passwd home back) and moves
/// the copy aside as `<homes>/.<user>.undone-<ms>` (kept: it may hold newer files).
pub fn undo_move(
    host: &dyn HomesHost,
    r: &HomeMoveRecord,
    now_ms: u64,
    log: &dyn Fn(&str),
) -> Result<Option<String>, String> {
    if host.probe(&r.from).exists {
        return Err(format!("{} exists again: nothing undone", r.from));
    }
    if !host.probe(&r.kept).exists {
        return Err(format!(
            "{} does not exist any more: nothing undone",
            r.kept
        ));
    }
    host.rename(&r.kept, &r.from, log)?;
    let aside = match host.read_link(HOMES_LINK) {
        Some(folder) => {
            let real = format!("{folder}/{}", r.user);
            if host.probe(&real).exists {
                let aside = format!("{folder}/.{}.undone-{now_ms}", r.user);
                host.rename(&real, &aside, log)?;
                Some(aside)
            } else {
                None
            }
        }
        None => None,
    };
    Ok(aside)
}

// ---------------------------------------------------------------------------------------------
// record of the moves waiting for confirmation
// ---------------------------------------------------------------------------------------------

pub const MOVES_FILE: &str = "homes-moves.json";

/// Moves whose old folder is kept: `<state_dir>/homes-moves.json` (0600), or in memory.
#[derive(Default)]
pub struct MoveStore {
    file: Option<PathBuf>,
    mem: Mutex<Vec<HomeMoveRecord>>,
}

impl MoveStore {
    pub fn memory() -> Self {
        Self::default()
    }

    pub fn in_dir(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            file: Some(state_dir.into().join(MOVES_FILE)),
            mem: Mutex::new(Vec::new()),
        }
    }

    pub fn list(&self) -> Vec<HomeMoveRecord> {
        match &self.file {
            Some(f) => std::fs::read(f)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default(),
            None => self.mem.lock().unwrap().clone(),
        }
    }

    pub fn get(&self, user: &str) -> Option<HomeMoveRecord> {
        self.list().into_iter().find(|r| r.user == user)
    }

    fn save(&self, v: Vec<HomeMoveRecord>) -> Result<(), String> {
        let Some(f) = &self.file else {
            *self.mem.lock().unwrap() = v;
            return Ok(());
        };
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = f.with_extension("json.tmp");
        let body = serde_json::to_vec(&v).map_err(|e| e.to_string())?;
        let mut w = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        w.write_all(&body)
            .and_then(|_| w.sync_all())
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, f).map_err(|e| format!("{}: {e}", f.display()))
    }

    pub fn put(&self, r: HomeMoveRecord) -> Result<(), String> {
        let mut v = self.list();
        v.retain(|x| x.user != r.user);
        v.push(r);
        self.save(v)
    }

    pub fn remove(&self, user: &str) -> Result<(), String> {
        let mut v = self.list();
        let before = v.len();
        v.retain(|x| x.user != user);
        if v.len() == before {
            return Ok(());
        }
        self.save(v)
    }
}

// ---------------------------------------------------------------------------------------------
// the real system
// ---------------------------------------------------------------------------------------------

/// The real system (`rynasd --dry-system` uses [`MemoryHomes`] instead).
pub struct HostHomes;

fn run(log: &dyn Fn(&str), cmd: Cmd) -> Result<String, String> {
    let name = cmd.program().to_string_lossy().into_owned();
    log(&format!("$ {}", cmd.display()));
    cmd.env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .timeout(CMD_TIMEOUT)
        .run_blocking()
        .map(|o| o.stdout)
        .map_err(|e| format!("{name}: {e}"))
}

/// `/proc/self/mountinfo` lines → read-write mount points and their filesystem type
/// (read-only mounts, such as bind mounts of system files or snapshots, cannot hold homes).
pub fn parse_mountinfo(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let (left, right) = l.split_once(" - ")?;
            let mut fields = left.split(' ');
            let mp = fields.nth(4)?;
            if fields.next()?.split(',').any(|o| o == "ro") {
                return None;
            }
            let fs = right.split(' ').next()?;
            // octal escapes (\040 for a space)
            let mut out = String::new();
            let b = mp.as_bytes();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'\\'
                    && i + 3 < b.len()
                    && let Ok(v) = u8::from_str_radix(&mp[i + 1..i + 4], 8)
                {
                    out.push(v as char);
                    i += 4;
                    continue;
                }
                out.push(b[i] as char);
                i += 1;
            }
            Some((out, fs.to_string()))
        })
        .collect()
}

/// Sessions of `user` in `smbstatus -b -j` output.
pub fn parse_smbstatus_sessions(json: &str, user: &str, uid: u32) -> Option<usize> {
    let v: Value = serde_json::from_str(json).ok()?;
    let Some(s) = v.get("sessions").and_then(Value::as_object) else {
        return Some(0);
    };
    Some(
        s.values()
            .filter(|x| {
                x.get("username").and_then(Value::as_str) == Some(user)
                    || x.get("uid").and_then(Value::as_u64) == Some(uid as u64)
            })
            .count(),
    )
}

/// rsync dry-run itemized lines that still name a change (the root folder left out).
pub fn parse_itemize(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .filter(|l| {
            let name = l.split_once(' ').map(|p| p.1).unwrap_or("");
            name != "./" && name != "."
        })
        .map(String::from)
        .collect()
}

const RSYNC_BASE: [&str; 3] = ["-aHAXx", "--numeric-ids", "--no-inc-recursive"];

impl HomesHost for HostHomes {
    fn probe(&self, path: &str) -> PathInfo {
        crate::host::ShareHost::probe(&crate::host::HostShares, path)
    }

    fn read_link(&self, path: &str) -> Option<String> {
        std::fs::read_link(path)
            .ok()
            .and_then(|p| p.into_os_string().into_string().ok())
    }

    // the statvfs field types depend on the architecture
    #[allow(clippy::unnecessary_cast)]
    fn mounts(&self) -> Vec<Mount> {
        let text = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        parse_mountinfo(&text)
            .into_iter()
            // a bind-mounted file is no volume
            .filter(|(path, fs)| {
                HOME_FS.contains(&fs.as_str()) && std::path::Path::new(path).is_dir()
            })
            .map(|(path, fs_type)| {
                let (size_bytes, avail_bytes) = nix::sys::statvfs::statvfs(path.as_str())
                    .map(|s| {
                        let f = s.fragment_size() as u64;
                        (s.blocks() as u64 * f, s.blocks_available() as u64 * f)
                    })
                    .unwrap_or((0, 0));
                Mount {
                    path,
                    fs_type,
                    size_bytes,
                    avail_bytes,
                }
            })
            .collect()
    }

    // the statvfs field types depend on the architecture
    #[allow(clippy::unnecessary_cast)]
    fn avail(&self, path: &str) -> Option<u64> {
        nix::sys::statvfs::statvfs(path)
            .ok()
            .map(|s| s.blocks_available() as u64 * s.fragment_size() as u64)
    }

    fn tree_size(&self, path: &str, budget: Duration) -> Option<(u64, bool)> {
        crate::host::tree_size(std::path::Path::new(path), budget)
    }

    fn tree_counts(&self, path: &str) -> Option<TreeCounts> {
        use std::os::unix::fs::MetadataExt;
        let root = std::fs::symlink_metadata(path).ok()?;
        let dev = root.dev();
        let mut c = TreeCounts::default();
        let mut stack = vec![PathBuf::from(path)];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).ok()?.flatten() {
                let m = std::fs::symlink_metadata(e.path()).ok()?;
                if m.dev() != dev {
                    continue;
                }
                if m.is_dir() {
                    c.dirs += 1;
                    stack.push(e.path());
                } else if m.file_type().is_symlink() {
                    c.links += 1;
                } else {
                    c.files += 1;
                    c.bytes += m.len();
                }
            }
        }
        Some(c)
    }

    fn owner_empty(&self, path: &str) -> Option<(u32, bool)> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::symlink_metadata(path).ok()?;
        if !m.is_dir() {
            return None;
        }
        let empty = std::fs::read_dir(path).ok()?.next().is_none();
        Some((m.uid(), empty))
    }

    fn processes(&self, uid: u32) -> Vec<u32> {
        let me = std::process::id();
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir("/proc") else {
            return out;
        };
        for e in rd.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            if pid == me {
                continue;
            }
            let Ok(st) = std::fs::read_to_string(e.path().join("status")) else {
                continue;
            };
            let hit = st
                .lines()
                .find_map(|l| l.strip_prefix("Uid:"))
                .is_some_and(|ids| ids.split_whitespace().any(|x| x == uid.to_string()));
            if hit {
                out.push(pid);
            }
        }
        out.sort_unstable();
        out
    }

    fn smb_sessions(&self, user: &str, uid: u32) -> Option<usize> {
        let o = Cmd::new("smbstatus")
            .args(["-b", "-j"])
            .env_clear()
            .timeout(CMD_TIMEOUT)
            .run_blocking()
            .ok()?;
        parse_smbstatus_sessions(&o.stdout, user, uid)
    }

    fn has_program(&self, name: &str) -> bool {
        ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
            .iter()
            .any(|d| std::path::Path::new(d).join(name).exists())
    }

    fn make_dir(
        &self,
        path: &str,
        subvolume: bool,
        uid: u32,
        gid: u32,
        mode: u32,
        log: &dyn Fn(&str),
    ) -> Result<(), String> {
        if subvolume {
            run(log, Cmd::new("btrfs").args(["subvolume", "create", path]))?;
        } else {
            run(log, Cmd::new("mkdir").args(["--", path]))?;
        }
        self.set_owner_mode(path, uid, gid, mode, log)
    }

    fn copy_skel(&self, home: &str, uid: u32, gid: u32, log: &dyn Fn(&str)) -> Result<(), String> {
        if !std::path::Path::new("/etc/skel").is_dir() {
            return Ok(());
        }
        run(
            log,
            Cmd::new("cp").args(["-a", "--no-preserve=ownership", "/etc/skel/.", home]),
        )?;
        run(
            log,
            Cmd::new("chown").args(["-R", "--no-dereference", &format!("{uid}:{gid}"), home]),
        )
        .map(|_| ())
    }

    fn set_link(&self, link: &str, target: &str, log: &dyn Fn(&str)) -> Result<(), String> {
        let tmp = format!("{link}.rynas-new");
        let _ = std::fs::remove_file(&tmp);
        log(&format!("{link} → {target}"));
        std::os::unix::fs::symlink(target, &tmp).map_err(|e| format!("{tmp}: {e}"))?;
        std::fs::rename(&tmp, link).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("{link}: {e}")
        })
    }

    fn rename(&self, from: &str, to: &str, log: &dyn Fn(&str)) -> Result<(), String> {
        log(&format!("rename {from} → {to}"));
        std::fs::rename(from, to).map_err(|e| format!("{from}: {e}"))
    }

    fn set_owner_mode(
        &self,
        path: &str,
        uid: u32,
        gid: u32,
        mode: u32,
        log: &dyn Fn(&str),
    ) -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt;
        log(&format!("{path}: owner {uid}:{gid}, mode {mode:o}"));
        std::os::unix::fs::lchown(path, Some(uid), Some(gid))
            .map_err(|e| format!("{path}: {e}"))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("{path}: {e}"))
    }

    fn remove(&self, path: &str, subvolume: bool, log: &dyn Fn(&str)) -> Result<(), String> {
        if subvolume {
            run(
                log,
                Cmd::new("btrfs")
                    .args(["subvolume", "delete", "--recursive", path])
                    .timeout(COPY_TIMEOUT),
            )
            .map(|_| ())
        } else {
            run(
                log,
                Cmd::new("rm")
                    .args([
                        "-rf",
                        "--one-file-system",
                        "--preserve-root=all",
                        "--",
                        path,
                    ])
                    .timeout(COPY_TIMEOUT),
            )
            .map(|_| ())
        }
    }

    fn sync(
        &self,
        from: &str,
        to: &str,
        delete: bool,
        log: &dyn Fn(&str),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        let mut args: Vec<String> = RSYNC_BASE.iter().map(|s| s.to_string()).collect();
        args.push("--info=stats1".into());
        if delete {
            args.push("--delete".into());
        }
        args.push(format!("{}/", from.trim_end_matches('/')));
        args.push(format!("{}/", to.trim_end_matches('/')));
        log(&format!("$ rsync {}", args.join(" ")));
        run_cancellable("rsync", &args, log, cancelled)
    }

    fn differences(&self, from: &str, to: &str) -> Result<Vec<String>, String> {
        let mut c = Cmd::new("rsync").args(RSYNC_BASE);
        c = c.args([
            "--dry-run",
            "--delete",
            "--itemize-changes",
            "--out-format=%i %n",
        ]);
        let o = c
            .args([
                format!("{}/", from.trim_end_matches('/')),
                format!("{}/", to.trim_end_matches('/')),
            ])
            .env_clear()
            .timeout(COPY_TIMEOUT)
            .run_blocking()
            .map_err(|e| format!("rsync: {e}"))?;
        Ok(parse_itemize(&o.stdout))
    }
}

/// Runs a long command, its output copied to `log`, killed (process group) on cancel.
fn run_cancellable(
    program: &str,
    args: &[String],
    log: &dyn Fn(&str),
    cancelled: &dyn Fn() -> bool,
) -> Result<(), String> {
    use std::io::BufRead;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("{program}: {e}"))?;
    let pgid = nix::unistd::Pid::from_raw(child.id() as i32);
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut readers = Vec::new();
    for (stream, prefix) in [
        (
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
            "",
        ),
        (
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
            "stderr: ",
        ),
    ] {
        if let Some(s) = stream {
            let tx = tx.clone();
            readers.push(std::thread::spawn(move || {
                for l in std::io::BufReader::new(s).lines().map_while(Result::ok) {
                    let _ = tx.send(format!("{prefix}{l}"));
                }
            }));
        }
    }
    drop(tx);
    let start = std::time::Instant::now();
    let status = loop {
        while let Ok(l) = rx.try_recv() {
            log(&l);
        }
        if let Some(s) = child.try_wait().map_err(|e| format!("{program}: {e}"))? {
            break s;
        }
        if cancelled() || start.elapsed() > COPY_TIMEOUT {
            let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGTERM);
            std::thread::sleep(Duration::from_secs(2));
            let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
            let _ = child.wait();
            return Err(if cancelled() {
                format!("{program}: cancelled")
            } else {
                format!("{program}: timed out")
            });
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    for r in readers {
        let _ = r.join();
    }
    while let Ok(l) = rx.try_recv() {
        log(&l);
    }
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program}: {status}"))
    }
}

// ---------------------------------------------------------------------------------------------
// in memory
// ---------------------------------------------------------------------------------------------

/// A path of [`MemoryHomes`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MemEntry {
    /// Target of a symbolic link.
    pub link: Option<String>,
    pub subvolume: bool,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    /// Bytes used (files below are not modelled).
    pub bytes: u64,
    pub fs_type: String,
}

#[derive(Debug, Default, Clone)]
pub struct MemoryHomesState {
    /// Folders and links by absolute path.
    pub entries: BTreeMap<String, MemEntry>,
    pub mounts: Vec<Mount>,
    pub processes: BTreeMap<u32, Vec<u32>>,
    /// SMB sessions by user; a user missing here has none.
    pub smb: BTreeMap<String, usize>,
    /// `smbstatus` is not available.
    pub no_samba: bool,
    pub missing_programs: Vec<String>,
    /// Every write, in order.
    pub log: Vec<String>,
    /// Operations (`make_dir`, `sync`, `rename`, `remove`, `set_link`...) that fail.
    pub fail: Vec<String>,
    /// What [`HomesHost::differences`] reports.
    pub differences: Vec<String>,
}

/// Folders, links and processes in memory: tests, and `--dry-system` (`probe_host`: paths
/// unknown here are probed on the real system, read-only).
#[derive(Default)]
pub struct MemoryHomes {
    pub probe_host: bool,
    state: Mutex<MemoryHomesState>,
}

impl MemoryHomes {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn probing_host() -> Self {
        Self {
            probe_host: true,
            ..Default::default()
        }
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, MemoryHomesState> {
        self.state.lock().unwrap()
    }

    /// Declares a mounted volume.
    pub fn add_volume(&self, path: &str, fs_type: &str, avail: u64) {
        let mut s = self.state();
        s.mounts.push(Mount {
            path: path.into(),
            fs_type: fs_type.into(),
            size_bytes: avail * 2,
            avail_bytes: avail,
        });
        s.entries.insert(
            path.into(),
            MemEntry {
                subvolume: fs_type == "btrfs",
                mode: 0o755,
                fs_type: fs_type.into(),
                ..Default::default()
            },
        );
    }

    /// Declares a folder (on the filesystem of the longest mount containing it).
    pub fn add_dir(&self, path: &str, uid: u32, bytes: u64) {
        let fs = self.fs_of(path);
        self.state().entries.insert(
            path.into(),
            MemEntry {
                uid,
                gid: 100,
                mode: 0o755,
                bytes,
                fs_type: fs,
                ..Default::default()
            },
        );
    }

    fn fs_of(&self, path: &str) -> String {
        let s = self.state();
        s.mounts
            .iter()
            .filter(|m| within(path, &m.path))
            .max_by_key(|m| m.path.len())
            .map(|m| m.fs_type.clone())
            .unwrap_or_else(|| "ext4".into())
    }

    /// `path` with a symbolic link prefix replaced by its target.
    fn resolve(&self, path: &str) -> String {
        let s = self.state();
        for (p, e) in &s.entries {
            if let Some(t) = &e.link {
                if path == p {
                    return t.clone();
                }
                if let Some(rest) = path.strip_prefix(&format!("{p}/")) {
                    return format!("{t}/{rest}");
                }
            }
        }
        path.to_string()
    }

    fn write(&self, op: &str, line: String) -> Result<(), String> {
        let mut s = self.state();
        tracing::info!("dry system: {line}");
        s.log.push(line);
        if s.fail.iter().any(|f| f == op) {
            return Err(format!("{op}: injected failure"));
        }
        Ok(())
    }
}

impl HomesHost for MemoryHomes {
    fn probe(&self, path: &str) -> PathInfo {
        let symlink = self
            .state()
            .entries
            .get(path)
            .is_some_and(|e| e.link.is_some());
        let real = self.resolve(path);
        if let Some(e) = self.state().entries.get(&real).cloned() {
            return PathInfo {
                exists: true,
                is_dir: e.link.is_none(),
                fs_type: e.fs_type.clone(),
                remote: false,
                subvolume: e.subvolume,
                subvolume_uuid: None,
                symlink,
            };
        }
        if self.probe_host {
            return HostHomes.probe(path);
        }
        PathInfo {
            symlink,
            ..Default::default()
        }
    }

    fn read_link(&self, path: &str) -> Option<String> {
        if let Some(e) = self.state().entries.get(path) {
            return e.link.clone();
        }
        if self.probe_host {
            return HostHomes.read_link(path);
        }
        None
    }

    fn mounts(&self) -> Vec<Mount> {
        let m = self.state().mounts.clone();
        if m.is_empty() && self.probe_host {
            return HostHomes.mounts();
        }
        m
    }

    fn avail(&self, path: &str) -> Option<u64> {
        let real = self.resolve(path);
        let s = self.state();
        let m = s
            .mounts
            .iter()
            .filter(|m| within(&real, &m.path))
            .max_by_key(|m| m.path.len());
        match m {
            Some(m) => Some(m.avail_bytes),
            None if self.probe_host => HostHomes.avail(path),
            None => None,
        }
    }

    fn tree_size(&self, path: &str, _budget: Duration) -> Option<(u64, bool)> {
        let real = self.resolve(path);
        self.state().entries.get(&real).map(|e| (e.bytes, false))
    }

    fn tree_counts(&self, path: &str) -> Option<TreeCounts> {
        let real = self.resolve(path);
        self.state().entries.get(&real).map(|e| TreeCounts {
            files: 1,
            dirs: 0,
            links: 0,
            bytes: e.bytes,
        })
    }

    fn owner_empty(&self, path: &str) -> Option<(u32, bool)> {
        let real = self.resolve(path);
        let s = self.state();
        let e = s.entries.get(&real)?;
        let prefix = format!("{real}/");
        Some((e.uid, !s.entries.keys().any(|k| k.starts_with(&prefix))))
    }

    fn processes(&self, uid: u32) -> Vec<u32> {
        self.state()
            .processes
            .get(&uid)
            .cloned()
            .unwrap_or_default()
    }

    fn smb_sessions(&self, user: &str, _uid: u32) -> Option<usize> {
        let s = self.state();
        if s.no_samba {
            return None;
        }
        Some(s.smb.get(user).copied().unwrap_or(0))
    }

    fn has_program(&self, name: &str) -> bool {
        !self.state().missing_programs.iter().any(|p| p == name)
    }

    fn make_dir(
        &self,
        path: &str,
        subvolume: bool,
        uid: u32,
        gid: u32,
        mode: u32,
        log: &dyn Fn(&str),
    ) -> Result<(), String> {
        let line = format!(
            "{} {path} {uid}:{gid} {mode:o}",
            if subvolume { "subvolume" } else { "mkdir" }
        );
        log(&line);
        self.write("make_dir", line)?;
        let real = self.resolve(path);
        let fs = self.fs_of(&real);
        let mut s = self.state();
        if s.entries.contains_key(&real) {
            return Err(format!("{path}: exists"));
        }
        s.entries.insert(
            real,
            MemEntry {
                subvolume,
                uid,
                gid,
                mode,
                fs_type: fs,
                ..Default::default()
            },
        );
        Ok(())
    }

    fn copy_skel(&self, home: &str, uid: u32, gid: u32, log: &dyn Fn(&str)) -> Result<(), String> {
        let line = format!("skel {home} {uid}:{gid}");
        log(&line);
        self.write("copy_skel", line)
    }

    fn set_link(&self, link: &str, target: &str, log: &dyn Fn(&str)) -> Result<(), String> {
        let line = format!("link {link} -> {target}");
        log(&line);
        self.write("set_link", line)?;
        self.state().entries.insert(
            link.into(),
            MemEntry {
                link: Some(target.into()),
                ..Default::default()
            },
        );
        Ok(())
    }

    fn rename(&self, from: &str, to: &str, log: &dyn Fn(&str)) -> Result<(), String> {
        let line = format!("rename {from} -> {to}");
        log(&line);
        self.write("rename", line)?;
        let (rf, rt) = (self.resolve(from), self.resolve(to));
        let mut s = self.state();
        let e = s
            .entries
            .remove(&rf)
            .ok_or_else(|| format!("{from}: no such folder"))?;
        s.entries.insert(rt, e);
        Ok(())
    }

    fn set_owner_mode(
        &self,
        path: &str,
        uid: u32,
        gid: u32,
        mode: u32,
        log: &dyn Fn(&str),
    ) -> Result<(), String> {
        let line = format!("owner {path} {uid}:{gid} {mode:o}");
        log(&line);
        self.write("set_owner_mode", line)?;
        let real = self.resolve(path);
        if let Some(e) = self.state().entries.get_mut(&real) {
            e.uid = uid;
            e.gid = gid;
            e.mode = mode;
        }
        Ok(())
    }

    fn remove(&self, path: &str, subvolume: bool, log: &dyn Fn(&str)) -> Result<(), String> {
        let line = format!(
            "{} {path}",
            if subvolume { "subvolume-delete" } else { "rm" }
        );
        log(&line);
        self.write("remove", line)?;
        let real = self.resolve(path);
        self.state().entries.remove(&real);
        Ok(())
    }

    fn sync(
        &self,
        from: &str,
        to: &str,
        delete: bool,
        log: &dyn Fn(&str),
        _cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        let line = format!(
            "rsync{} {from}/ {to}/",
            if delete { " --delete" } else { "" }
        );
        log(&line);
        self.write("sync", line)?;
        let (rf, rt) = (self.resolve(from), self.resolve(to));
        let mut s = self.state();
        let bytes = s.entries.get(&rf).map(|e| e.bytes).unwrap_or(0);
        if let Some(e) = s.entries.get_mut(&rt) {
            e.bytes = bytes;
        }
        Ok(())
    }

    fn differences(&self, _from: &str, _to: &str) -> Result<Vec<String>, String> {
        Ok(self.state().differences.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::MemoryAccounts;

    fn quiet(_: &str) {}

    fn user(name: &str, uid: u32, home: &str) -> SysUser {
        SysUser {
            name: name.into(),
            uid,
            gid: 100,
            full_name: String::new(),
            home: home.into(),
            shell: "/bin/bash".into(),
            expire_days: None,
        }
    }

    fn setup() -> (Arc<MemoryHomes>, HomesDomain) {
        let h = Arc::new(MemoryHomes::new());
        h.add_volume("/volume1", "btrfs", 1 << 40);
        h.add_volume("/data", "ext4", 1 << 30);
        h.add_volume("/", "ext4", 1 << 30);
        let acc = Arc::new(MemoryAccounts::new());
        let d = HomesDomain::new(h.clone(), acc);
        (h, d)
    }

    #[test]
    fn paths() {
        assert_eq!(home_of("alice"), "/srv/rynas/homes/alice");
        assert_eq!(folder_of("/volume1/"), "/volume1/homes");
        assert_eq!(link_user("/srv/rynas/homes/alice"), Some("alice"));
        assert_eq!(link_user("/srv/rynas/homes/alice/x"), None);
        assert_eq!(link_user("/srv/rynas/homesx/alice"), None);
        assert_eq!(link_user("/srv/rynas/homes/"), None);
        assert_eq!(link_user("/home/alice"), None);
        assert_eq!(kept_path("/home/marco/"), "/home/marco.rynas-old");
    }

    #[test]
    fn volume_choices_leave_out_the_system() {
        let mounts = vec![
            Mount {
                path: "/".into(),
                fs_type: "ext4".into(),
                ..Default::default()
            },
            Mount {
                path: "/boot/efi".into(),
                fs_type: "vfat".into(),
                ..Default::default()
            },
            Mount {
                path: "/var/lib/docker".into(),
                fs_type: "btrfs".into(),
                ..Default::default()
            },
            Mount {
                path: "/srv/data".into(),
                fs_type: "xfs".into(),
                ..Default::default()
            },
            Mount {
                path: "/volume1".into(),
                fs_type: "btrfs".into(),
                ..Default::default()
            },
            Mount {
                path: "/media/usb".into(),
                fs_type: "ext4".into(),
                ..Default::default()
            },
        ];
        let c = candidates(&mounts, &[("volume1".into(), "/volume1".into())]);
        let paths: Vec<&str> = c.iter().map(|v| v.path.as_str()).collect();
        assert_eq!(paths, ["/volume1", "/srv/data"], "rynas volumes first");
        assert_eq!(c[0].label.as_deref(), Some("volume1"));
    }

    #[test]
    fn settings_are_checked() {
        let (_, d) = setup();
        let ok = |v: &str| {
            d.normalize(HomesConfig {
                enabled: true,
                volume: v.into(),
            })
        };
        assert_eq!(ok("/volume1/").unwrap().volume, "/volume1");
        assert!(ok("").is_err());
        assert!(ok("/").is_err(), "the system disk");
        assert!(ok("/nowhere").is_err(), "not mounted");
        assert!(ok("/volume1/../etc").is_err());
        assert!(ok("relative").is_err());
        assert!(ok("/data").is_ok(), "any mounted local filesystem");
        // off: anything goes except garbage
        assert!(
            d.normalize(HomesConfig {
                enabled: false,
                volume: "/nowhere".into()
            })
            .is_ok()
        );
    }

    #[test]
    fn the_volume_cannot_change_under_homes() {
        let h = Arc::new(MemoryHomes::new());
        h.add_volume("/volume1", "btrfs", 1 << 40);
        h.add_volume("/volume2", "btrfs", 1 << 40);
        let acc = Arc::new(MemoryAccounts::new());
        acc.state()
            .users
            .push(user("alice", 1001, "/srv/rynas/homes/alice"));
        let d = HomesDomain::new(h.clone(), acc.clone());
        let c = |v: &str| HomesConfig {
            enabled: true,
            volume: v.into(),
        };
        d.ensure(&c("/volume1"), &quiet).unwrap();
        assert!(d.normalize(c("/volume1")).is_ok());
        let e = d.normalize(c("/volume2")).unwrap_err();
        assert!(e.message.contains("alice"), "{e:?}");
        acc.state().users.clear();
        assert!(d.normalize(c("/volume2")).is_ok());
    }

    #[test]
    fn enabling_creates_the_folder_and_the_link_once() {
        let (h, d) = setup();
        let c = HomesConfig {
            enabled: true,
            volume: "/volume1".into(),
        };
        d.ensure(&c, &quiet).unwrap();
        d.ensure(&c, &quiet).unwrap();
        let s = h.state();
        assert_eq!(
            s.log,
            [
                "subvolume /volume1/homes 0:0 711",
                "mkdir /srv/rynas 0:0 755",
                "link /srv/rynas/homes -> /volume1/homes",
            ]
        );
        drop(s);
        let p = h.probe(HOMES_LINK);
        assert!(p.symlink && p.is_dir && p.subvolume);
        // on ext4 a plain folder
        let c2 = HomesConfig {
            enabled: true,
            volume: "/data".into(),
        };
        d.ensure(&c2, &quiet).unwrap();
        assert!(
            h.state()
                .log
                .contains(&"mkdir /data/homes 0:0 711".to_string())
        );
        assert_eq!(h.read_link(HOMES_LINK).as_deref(), Some("/data/homes"));
    }

    #[test]
    fn a_real_folder_at_the_link_is_never_replaced() {
        let (h, d) = setup();
        h.add_dir("/srv/rynas/homes", 0, 0);
        let e = d
            .ensure(
                &HomesConfig {
                    enabled: true,
                    volume: "/volume1".into(),
                },
                &quiet,
            )
            .unwrap_err();
        assert!(e.contains("not a symbolic link"), "{e}");
    }

    #[test]
    fn homes_of_new_users_are_subvolumes_owned_by_them() {
        let (h, d) = setup();
        d.ensure(
            &HomesConfig {
                enabled: true,
                volume: "/volume1".into(),
            },
            &quiet,
        )
        .unwrap();
        let real = create_home(h.as_ref(), "alice", 1001, 100, false, &quiet).unwrap();
        assert_eq!(real, "/volume1/homes/alice");
        let p = h.probe("/srv/rynas/homes/alice");
        assert!(p.exists && p.subvolume);
        assert_eq!(h.state().entries["/volume1/homes/alice"].mode, 0o700);
        assert!(
            create_home(h.as_ref(), "alice", 1002, 100, false, &quiet).is_err(),
            "another user's folder is not reused"
        );
        assert!(
            create_home(h.as_ref(), "alice", 1001, 100, false, &quiet).is_ok(),
            "an empty folder of the same uid is (a rolled back apply)"
        );
        h.add_dir("/volume1/homes/alice/notes", 1001, 1);
        assert!(
            create_home(h.as_ref(), "alice", 1001, 100, false, &quiet).is_err(),
            "a folder with files is not reused"
        );
        h.state().entries.remove("/volume1/homes/alice/notes");
        assert_eq!(
            location(h.as_ref(), "/srv/rynas/homes/alice"),
            (HomeLocation::Volume, true)
        );
        assert!(remove_home(h.as_ref(), "/srv/rynas/homes/alice", &quiet).unwrap());
        assert!(
            h.state()
                .log
                .contains(&"subvolume-delete /volume1/homes/alice".to_string())
        );
        assert!(!remove_home(h.as_ref(), "/home/bob", &quiet).unwrap());
    }

    fn ready() -> (Arc<MemoryHomes>, SysUser) {
        let (h, d) = setup();
        d.ensure(
            &HomesConfig {
                enabled: true,
                volume: "/volume1".into(),
            },
            &quiet,
        )
        .unwrap();
        h.add_dir("/home/marco", 1000, 5 << 30);
        h.add_dir("/home/marco/.ssh/authorized_keys", 1000, 1);
        h.state().log.clear();
        (h, user("marco", 1000, "/home/marco"))
    }

    fn cx() -> MoveContext<'static> {
        MoveContext {
            service_on: true,
            shares: &[],
            waiting: false,
        }
    }

    #[test]
    fn a_move_plan_says_what_happens() {
        let (h, u) = ready();
        let p = plan_move(h.as_ref(), &u, &cx());
        assert!(p.allowed, "{:?}", p.blockers);
        assert_eq!(p.to, "/srv/rynas/homes/marco");
        assert_eq!(p.target, "/volume1/homes/marco");
        assert!(p.subvolume && p.ssh_keys);
        assert_eq!(p.size_bytes, 5 << 30);
        assert!(h.state().log.is_empty(), "a plan changes nothing");
    }

    #[test]
    fn a_move_plan_lists_what_blocks() {
        let (h, u) = ready();
        h.state().processes.insert(1000, vec![4242, 4243]);
        h.state().smb.insert("marco".into(), 2);
        h.state().missing_programs.push("rsync".into());
        let shares = [("photos".to_string(), "/home/marco/photos".to_string())];
        let p = plan_move(
            h.as_ref(),
            &u,
            &MoveContext {
                service_on: false,
                shares: &shares,
                waiting: true,
            },
        );
        let codes: Vec<B> = p.blockers.iter().map(|b| b.code).collect();
        assert!(!p.allowed);
        for c in [
            B::ServiceOff,
            B::ShareInside,
            B::Processes,
            B::SmbSessions,
            B::NoRsync,
            B::MoveWaiting,
        ] {
            assert!(codes.contains(&c), "{c:?} in {codes:?}");
        }
        assert_eq!(
            p.blockers
                .iter()
                .find(|b| b.code == B::Processes)
                .unwrap()
                .detail,
            "4242 4243"
        );
        // no space, target exists, already moved, missing, mounts inside
        let (h, u) = ready();
        h.state().mounts[0].avail_bytes = 1 << 30;
        h.add_dir("/volume1/homes/marco", 1000, 0);
        h.state().mounts.push(Mount {
            path: "/home/marco/nfs".into(),
            fs_type: "ext4".into(),
            ..Default::default()
        });
        let codes: Vec<B> = plan_move(h.as_ref(), &u, &cx())
            .blockers
            .iter()
            .map(|b| b.code)
            .collect();
        assert!(codes.contains(&B::NoSpace) && codes.contains(&B::TargetExists));
        assert!(codes.contains(&B::SourceNotPlain), "{codes:?}");
        let moved = user("marco", 1000, "/srv/rynas/homes/marco");
        assert!(
            plan_move(h.as_ref(), &moved, &cx())
                .blockers
                .iter()
                .any(|b| b.code == B::AlreadyMoved)
        );
        let gone = user("ghost", 1003, "/home/ghost");
        assert!(
            plan_move(h.as_ref(), &gone, &cx())
                .blockers
                .iter()
                .any(|b| b.code == B::SourceMissing)
        );
        let sys = user("daemon", 1, "/usr/sbin");
        assert!(
            plan_move(h.as_ref(), &sys, &cx())
                .blockers
                .iter()
                .any(|b| b.code == B::NotLocal)
        );
    }

    #[test]
    fn a_move_copies_verifies_keeps_and_can_be_undone() {
        let (h, u) = ready();
        let p = plan_move(h.as_ref(), &u, &cx());
        copy_home(h.as_ref(), &p, &u, &quiet, &|_| {}, &|| false).unwrap();
        assert_eq!(
            h.state().log,
            [
                "subvolume /volume1/homes/marco 1000:100 700",
                "rsync /home/marco/ /volume1/homes/marco/",
                "rsync --delete /home/marco/ /volume1/homes/marco/",
                "owner /volume1/homes/marco 1000:100 700",
            ]
        );
        let kept = keep_old(h.as_ref(), "/home/marco", 7, &quiet).unwrap();
        assert_eq!(kept, "/home/marco.rynas-old");
        let r = HomeMoveRecord {
            user: "marco".into(),
            from: "/home/marco".into(),
            to: p.to.clone(),
            kept: kept.clone(),
            moved_ms: 7,
        };
        let aside = undo_move(h.as_ref(), &r, 9, &quiet).unwrap();
        assert_eq!(aside.as_deref(), Some("/volume1/homes/.marco.undone-9"));
        assert!(h.probe("/home/marco").exists);
        assert!(!h.probe("/volume1/homes/marco").exists);
        // a second keep while the first is there gets a unique name
        h.add_dir("/home/marco.rynas-old", 1000, 0);
        assert_eq!(
            keep_old(h.as_ref(), "/home/marco", 11, &quiet).unwrap(),
            "/home/marco.rynas-old-11"
        );
    }

    #[test]
    fn a_failed_copy_is_removed_and_the_home_untouched() {
        let (h, u) = ready();
        let p = plan_move(h.as_ref(), &u, &cx());
        h.state().differences = vec![">f.st...... notes.txt".into()];
        let e = copy_home(h.as_ref(), &p, &u, &quiet, &|_| {}, &|| false).unwrap_err();
        assert!(e.contains("differs"), "{e}");
        assert!(!h.probe("/volume1/homes/marco").exists);
        assert!(h.probe("/home/marco").exists);
        // the user logs in during the copy: same
        let (h, u) = ready();
        let p = plan_move(h.as_ref(), &u, &cx());
        h.state().processes.insert(1000, vec![1]);
        assert!(copy_home(h.as_ref(), &p, &u, &quiet, &|_| {}, &|| false).is_err());
        assert!(!h.probe("/volume1/homes/marco").exists);
        // a blocked plan copies nothing
        let mut p2 = p.clone();
        p2.allowed = false;
        assert!(copy_home(h.as_ref(), &p2, &u, &quiet, &|_| {}, &|| false).is_err());
    }

    #[test]
    fn finishing_deletes_only_a_kept_folder() {
        let (h, _) = ready();
        h.add_dir("/home/marco.rynas-old", 1000, 0);
        let mut r = HomeMoveRecord {
            user: "marco".into(),
            from: "/home/marco".into(),
            to: home_of("marco"),
            kept: "/home/marco".into(),
            moved_ms: 1,
        };
        assert!(finish_move(h.as_ref(), &r, &quiet).is_err());
        r.kept = "/home/marco.rynas-old".into();
        finish_move(h.as_ref(), &r, &quiet).unwrap();
        assert!(!h.probe("/home/marco.rynas-old").exists);
    }

    #[test]
    fn the_move_record_survives_restarts() {
        let d = tempfile::tempdir().unwrap();
        let s = MoveStore::in_dir(d.path());
        s.put(HomeMoveRecord {
            user: "marco".into(),
            ..Default::default()
        })
        .unwrap();
        assert!(MoveStore::in_dir(d.path()).get("marco").is_some());
        s.remove("marco").unwrap();
        assert!(MoveStore::in_dir(d.path()).list().is_empty());
    }

    #[test]
    fn parsers() {
        let m = parse_mountinfo(
            "22 1 0:21 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw\n\
             40 22 0:35 /@ /volume\\0401 rw shared:5 - btrfs /dev/md2 rw\n",
        );
        assert_eq!(m[1], ("/volume 1".into(), "btrfs".into()));
        assert!(
            parse_mountinfo("50 22 0:40 /x /opt/bind ro,relatime - ext4 /dev/sda1 rw\n").is_empty(),
            "read-only mounts are left out"
        );
        let j = r#"{"sessions":{"1":{"username":"alice","uid":1001},"2":{"username":"bob","uid":1002},"3":{"username":"x","uid":1001}}}"#;
        assert_eq!(parse_smbstatus_sessions(j, "alice", 1001), Some(2));
        assert_eq!(parse_smbstatus_sessions("{}", "alice", 1001), Some(0));
        assert_eq!(parse_smbstatus_sessions("garbage", "alice", 1001), None);
        assert_eq!(
            parse_itemize(".d..t...... ./\n>f+++++++++ new.txt\n\n"),
            [">f+++++++++ new.txt"]
        );
    }
}
