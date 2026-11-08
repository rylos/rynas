//! User homes: the `homes` configuration key and the home move.
//!
//! A home lives either on the system disk (`/home/<user>`, the Debian default) or in the homes
//! folder of a volume, `<volume>/homes/<user>` (one btrfs subvolume per user when the volume
//! is btrfs, a plain folder otherwise). Each user can have their home on a different volume.
//! Homes on a volume are named in passwd through a stable path, `/srv/rynas/homes/<user>`, a
//! **symbolic link per user** to the real folder inside [`HOMES_LINK`], a root-owned folder
//! (0711) on the system disk. Moving a home to another volume replaces its link, not passwd; a
//! dangling link (volume not mounted) makes logins fail instead of writing into the system disk
//! below an empty mount point. See `docs/research/user-homes.md`.
//!
//! - [`HomesDomain`] (`homes`, order 5, before `users`): the folder of links, the homes folder
//!   of the default volume (root, 0711: users can reach their own home but not list the
//!   others), and the migration of the first layout (the whole `/srv/rynas/homes` was one link
//!   to `<volume>/homes`) into per-user links, at start and at every apply. Turning the
//!   service off changes nothing on disk: homes already there keep working.
//! - New users get their home created by the `users` domain ([`create_home`]) on the volume of
//!   their [`LocalUser::home_volume`](rynas_api_types::LocalUser), deleted users lose it with
//!   `remove_home` ([`remove_home`]).
//! - Existing homes never move by themselves: [`plan_move`] says what moving a home to a
//!   destination ([`Dest`]: a volume or the system disk) does and what prevents it,
//!   [`copy_home`] copies it (rsync with hard links, ACLs and xattrs, a second pass and a
//!   verification), then rynasd switches the link and/or the passwd home and keeps the old
//!   folder renamed ([`kept_path`]) until the move is confirmed or undone ([`MoveStore`]).
//!
//! Filesystem access goes through [`HomesHost`]: [`HostHomes`] on the real system,
//! [`MemoryHomes`] for tests and `rynasd --dry-system`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rynas_api_types::{
    HOME_SYSTEM, HOMES_FOLDER, HOMES_KEY, HOMES_LINK, HomeLocation, HomeMoveBlocker,
    HomeMoveBlockerCode as B, HomeMovePlan, HomeMoveRecord, HomesConfig, HomesVolume, LOCAL_ID_MAX,
    LOCAL_ID_MIN, SYSTEM_HOMES,
};
use rynas_config::{Applier, ApplyCtx, Invalid, ManagedFile};
use rynas_tasks::Cmd;
use serde_json::Value;

use crate::accounts::{Accounts, SysUser};
use crate::host::PathInfo;

/// Mode of a homes folder and of the folder of links: owner root, others may traverse (to
/// reach their own home) but not list it.
pub const FOLDER_MODE: u32 = 0o711;
/// Mode of a home: the owner only.
pub const HOME_MODE: u32 = 0o700;
/// Suffix of an old home kept after a move (user names cannot contain '.').
pub const KEPT_SUFFIX: &str = ".rynas-old";
/// Time spent measuring a home for a plan.
pub const PLAN_SIZE_BUDGET: Duration = Duration::from_millis(1500);
/// Time for measuring the sizes of all the homes of one plan request (measured in
/// parallel); what is not measured by then is a lower bound (`size_partial`).
pub const PLAN_TOTAL_BUDGET: Duration = Duration::from_secs(3);
/// Time for measuring a home in the move task, before its copy: a size still partial
/// after it is unknown and the home does not move.
pub const MOVE_SIZE_BUDGET: Duration = Duration::from_secs(20 * 60);
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

/// The volume of `<volume>/homes/<user>`.
pub fn volume_of_folder(folder: &str, user: &str) -> Option<String> {
    folder
        .strip_suffix(&format!("/{HOMES_FOLDER}/{user}"))
        .filter(|v| !v.is_empty())
        .map(String::from)
}

/// Where an old home is kept after a move.
pub fn kept_path(from: &str) -> String {
    format!("{}{KEPT_SUFFIX}", from.trim_end_matches('/'))
}

/// A volume path without its trailing `/`.
pub fn trim_volume(v: &str) -> String {
    let v = v.trim();
    if v.len() > 1 {
        v.trim_end_matches('/').to_string()
    } else {
        v.to_string()
    }
}

fn in_range(uid: u32) -> bool {
    (LOCAL_ID_MIN..=LOCAL_ID_MAX).contains(&uid)
}

fn parent_of(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some(("", _)) => "/",
        Some((p, _)) => p,
        None => "",
    }
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
    /// The symbolic links directly in `dir`: `(name, target)`.
    fn links(&self, dir: &str) -> Vec<(String, String)>;
    /// Local mounted filesystems.
    fn mounts(&self) -> Vec<Mount>;
    /// Free bytes on the filesystem of `path`.
    fn avail(&self, path: &str) -> Option<u64>;
    /// Space used below `path` (one filesystem), bounded in time: `(bytes, partial)`.
    fn tree_size(&self, path: &str, budget: Duration) -> Option<(u64, bool)>;
    fn tree_counts(&self, path: &str) -> Option<TreeCounts>;
    /// Owner uid of a folder and whether it is empty.
    fn owner_empty(&self, path: &str) -> Option<(u32, bool)>;
    /// Owner uid and permission bits of `path` itself (not followed).
    fn owner_mode(&self, path: &str) -> Option<(u32, u32)>;
    /// Bytes referenced by the btrfs subvolume at `path` from its quota group, when quotas
    /// are on (no walk of the files); `None` otherwise.
    fn qgroup_size(&self, _path: &str) -> Option<u64> {
        None
    }
    /// Processes running with this uid (real, effective, saved or filesystem).
    fn processes(&self, uid: u32) -> Vec<u32>;
    /// Processes running with this gid (real, effective, saved, file system or
    /// supplementary).
    fn group_processes(&self, _gid: u32) -> Vec<u32> {
        Vec::new()
    }
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
    /// Removes the symbolic link `path` (never what it points at).
    fn unlink(&self, path: &str, log: &dyn Fn(&str)) -> Result<(), String>;
    /// Swaps two paths in one step (`renameat2(RENAME_EXCHANGE)`).
    fn exchange(&self, a: &str, b: &str, log: &dyn Fn(&str)) -> Result<(), String>;
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

/// System paths that cannot hold a homes folder.
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

/// Filesystems that can hold a homes folder, with the rynas volume labels.
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

/// Why `volume` cannot hold a homes folder (`None`: it can).
pub fn volume_problem(mounts: &[Mount], volume: &str) -> Option<String> {
    if !volume.starts_with('/')
        || volume.chars().any(char::is_control)
        || volume.contains(['\n', ':', '"', '\\'])
        || volume[1..]
            .split('/')
            .any(|x| x.is_empty() || x == "." || x == "..")
    {
        return Some(format!("invalid volume path {volume:?}"));
    }
    if system_path(volume) {
        return Some(format!("{volume} is a system folder: choose a data volume"));
    }
    let Some(m) = mounts.iter().find(|m| m.path == volume) else {
        return Some(format!("{volume} is not a mounted volume"));
    };
    if !HOME_FS.contains(&m.fs_type.as_str()) {
        return Some(format!(
            "{volume} is {}: homes need btrfs, ext4, xfs, f2fs or zfs",
            m.fs_type
        ));
    }
    None
}

pub fn parse(v: Option<&Value>) -> Result<HomesConfig, String> {
    match v {
        Some(v) => {
            serde_json::from_value(v.clone()).map_err(|e| format!("invalid homes document: {e}"))
        }
        None => Ok(HomesConfig::default()),
    }
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

    pub fn normalize(&self, mut c: HomesConfig) -> Result<HomesConfig, Invalid> {
        c.volume = trim_volume(&c.volume);
        if c.volume.is_empty() {
            if c.enabled {
                return Err(Invalid::at(
                    "volume",
                    "choose the volume of the homes folder",
                ));
            }
            return Ok(c);
        }
        if !c.enabled {
            // off: any path that is one (it is only remembered)
            return match volume_problem(&[], &c.volume) {
                Some(e) if e.starts_with("invalid") => Err(Invalid::at("volume", e)),
                _ => Ok(c),
            };
        }
        // homes already on another volume stay there: changing the default moves nothing
        if let Some(e) = volume_problem(&self.host.mounts(), &c.volume) {
            return Err(Invalid::at("volume", e));
        }
        Ok(c)
    }

    /// The first layout (`/srv/rynas/homes` one link to `<volume>/homes`) becomes a folder of
    /// per-user links: built aside, then swapped in one step, so passwd homes never stop
    /// resolving. `true` when something changed. Idempotent.
    pub fn migrate(&self, log: &dyn Fn(&str)) -> Result<bool, String> {
        let h = self.host.as_ref();
        let Some(target) = h.read_link(HOMES_LINK) else {
            return Ok(false);
        };
        let target = target.trim_end_matches('/').to_string();
        let users = self.accounts.users()?;
        let tmp = format!("{HOMES_LINK}.rynas-new");
        // a migration stopped half way: its folder holds only links
        if h.read_link(&tmp).is_some() {
            h.unlink(&tmp, log)?;
        } else if h.probe(&tmp).exists {
            h.remove(&tmp, false, log)?;
        }
        log(&format!(
            "{HOMES_LINK} points at {target}: one link per home instead"
        ));
        h.make_dir(&tmp, false, 0, 0, FOLDER_MODE, log)?;
        for u in users
            .iter()
            .filter(|u| link_user(&u.home) == Some(u.name.as_str()))
        {
            h.set_link(
                &format!("{tmp}/{}", u.name),
                &format!("{target}/{}", u.name),
                log,
            )?;
        }
        h.exchange(&tmp, HOMES_LINK, log)?;
        h.unlink(&tmp, log)?;
        Ok(true)
    }

    fn ensure(&self, c: &HomesConfig, log: &dyn Fn(&str)) -> Result<(), String> {
        self.migrate(log)?;
        ensure_links_dir(self.host.as_ref(), log)?;
        ensure_folder(self.host.as_ref(), &c.volume, log).map(|_| ())
    }
}

/// The folder of links, root 0711 (created when missing).
pub fn ensure_links_dir(host: &dyn HomesHost, log: &dyn Fn(&str)) -> Result<(), String> {
    if host.read_link(HOMES_LINK).is_some() {
        return Err(format!(
            "{HOMES_LINK} is still a symbolic link (first layout): apply the user homes setting"
        ));
    }
    let p = host.probe(HOMES_LINK);
    if p.exists {
        if !p.is_dir {
            return Err(format!("{HOMES_LINK} exists and is not a folder"));
        }
        return Ok(());
    }
    let parent = parent_of(HOMES_LINK);
    if !host.probe(parent).exists {
        host.make_dir(parent, false, 0, 0, 0o755, log)?;
    }
    host.make_dir(HOMES_LINK, false, 0, 0, FOLDER_MODE, log)
}

/// `<volume>/homes` (root 0711, a subvolume on btrfs), created when missing; the volume must be
/// mounted (an empty mount point is on the system disk).
pub fn ensure_folder(
    host: &dyn HomesHost,
    volume: &str,
    log: &dyn Fn(&str),
) -> Result<String, String> {
    let folder = folder_of(volume);
    let vol = host.probe(volume);
    if !vol.exists || !vol.is_dir || !host.mounts().iter().any(|m| m.path == volume) {
        return Err(format!("{volume} is not mounted"));
    }
    let f = host.probe(&folder);
    if !f.exists {
        if host.read_link(&folder).is_some() {
            return Err(format!("{folder} exists and is not a folder"));
        }
        host.make_dir(&folder, vol.fs_type == "btrfs", 0, 0, FOLDER_MODE, log)?;
    } else if !f.is_dir || f.symlink {
        return Err(format!("{folder} exists and is not a folder"));
    } else if let Some((uid, mode)) = host.owner_mode(&folder)
        && (uid != 0 || mode & 0o022 != 0)
    {
        // users could rename or replace the homes of others in it
        return Err(format!(
            "{folder} exists but is not owned by root or can be written by others (owner {uid}, mode {mode:o}): make it root's, mode 0711, or choose another volume"
        ));
    }
    Ok(folder)
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
            self.migrate(ctx.log)?;
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
        if self.host.read_link(HOMES_LINK).is_some() || !self.host.probe(HOMES_LINK).is_dir {
            return Err(format!("{HOMES_LINK} is not a folder"));
        }
        let folder = folder_of(&c.volume);
        if !self.host.probe(&folder).is_dir {
            return Err(format!("{folder} is not reachable"));
        }
        Ok(())
    }

    fn import(&self, _ctx: &ApplyCtx) -> Result<Option<Value>, String> {
        // the first layout names its volume; per-user links name none: off
        let c = match self.host.read_link(HOMES_LINK) {
            Some(t) => match t
                .trim_end_matches('/')
                .strip_suffix(&format!("/{HOMES_FOLDER}"))
            {
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

/// The real folder behind `/srv/rynas/homes/<user>`: its link, or (first layout) inside the
/// folder the whole `/srv/rynas/homes` points at.
pub fn real_home(host: &dyn HomesHost, user: &str) -> Result<String, String> {
    if let Some(t) = host.read_link(&home_of(user)) {
        return Ok(t);
    }
    if let Some(f) = host.read_link(HOMES_LINK) {
        return Ok(format!("{}/{user}", f.trim_end_matches('/')));
    }
    Err(format!(
        "{} does not exist: the home of {user} is not set up",
        home_of(user)
    ))
}

/// The real folder of a user's passwd home.
pub fn folder_of_home(host: &dyn HomesHost, u: &SysUser) -> String {
    if link_user(&u.home) == Some(u.name.as_str()) {
        real_home(host, &u.name).unwrap_or_else(|_| u.home.clone())
    } else {
        u.home.clone()
    }
}

/// Points `/srv/rynas/homes/<user>` at `folder`.
pub fn point_link(
    host: &dyn HomesHost,
    user: &str,
    folder: &str,
    log: &dyn Fn(&str),
) -> Result<(), String> {
    ensure_links_dir(host, log)?;
    host.set_link(&home_of(user), folder, log)
}

/// Creates the home of a new user in the homes folder of `volume` (`None`: the folder of the
/// first layout's link) and its link: a subvolume on btrfs, owner only, `/etc/skel` copied for
/// users with a shell (the others never read dotfiles).
pub fn create_home(
    host: &dyn HomesHost,
    user: &str,
    volume: Option<&str>,
    uid: u32,
    gid: u32,
    skel: bool,
    log: &dyn Fn(&str),
) -> Result<String, String> {
    let link = home_of(user);
    let (folder, linked) = match volume {
        Some(v) => {
            ensure_links_dir(host, log)?;
            (ensure_folder(host, v, log)?, true)
        }
        None => match host.read_link(HOMES_LINK) {
            Some(f) => (f.trim_end_matches('/').to_string(), false),
            None => {
                return Err(format!("user {user}: the volume of the home is not known"));
            }
        },
    };
    let real = format!("{folder}/{user}");
    if linked {
        match host.read_link(&link) {
            Some(t) if t != real => {
                return Err(format!(
                    "{link} exists already and points at {t} (the home of a deleted user?): remove it first"
                ));
            }
            Some(_) => {}
            None if host.probe(&link).exists => {
                return Err(format!("{link} exists and is not a symbolic link"));
            }
            None => {}
        }
    }
    let f = host.probe(&folder);
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
            if linked {
                host.set_link(&link, &real, log)?;
            }
            return Ok(real);
        }
        return Err(format!(
            "{real} exists already (the home of a deleted user?): remove or rename it first"
        ));
    }
    let subvolume = f.fs_type == "btrfs";
    host.make_dir(&real, subvolume, uid, gid, HOME_MODE, log)?;
    if skel && let Err(e) = host.copy_skel(&real, uid, gid, log) {
        log(&format!("{real}: skeleton files not copied: {e}"));
    }
    if linked && let Err(e) = host.set_link(&link, &real, log) {
        let _ = host.remove(&real, subvolume, log);
        return Err(e);
    }
    Ok(real)
}

/// Deletes the home of a deleted user when it is in a homes folder (its subvolume) and its
/// link; `false` when `home` is not there (userdel -r handles it).
pub fn remove_home(host: &dyn HomesHost, home: &str, log: &dyn Fn(&str)) -> Result<bool, String> {
    let Some(user) = link_user(home) else {
        return Ok(false);
    };
    let real = real_home(host, user)?;
    let i = host.probe(&real);
    if !i.exists {
        log(&format!("{real} does not exist any more"));
    } else if i.symlink {
        return Err(format!("{real} is a symbolic link: not deleted"));
    } else {
        host.remove(&real, i.subvolume, log)?;
        log(&format!("{real} deleted"));
    }
    if host.read_link(&home_of(user)).is_some() {
        host.unlink(&home_of(user), log)?;
    }
    Ok(true)
}

/// Where a user's home is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HomeInfo {
    pub location: HomeLocation,
    /// The real folder.
    pub folder: String,
    /// The volume of a home in a homes folder.
    pub volume: String,
    pub subvolume: bool,
}

pub fn home_info(host: &dyn HomesHost, u: &SysUser) -> HomeInfo {
    let linked = link_user(&u.home) == Some(u.name.as_str());
    let folder = folder_of_home(host, u);
    let volume = if linked {
        volume_of_folder(&folder, &u.name).unwrap_or_default()
    } else {
        String::new()
    };
    let i = host.probe(&folder);
    let location = if !i.exists || !i.is_dir {
        HomeLocation::Missing
    } else if linked {
        HomeLocation::Volume
    } else if u.home.starts_with(&format!("{SYSTEM_HOMES}/")) {
        HomeLocation::System
    } else {
        HomeLocation::Other
    };
    HomeInfo {
        location,
        subvolume: i.exists && i.subvolume,
        folder,
        volume,
    }
}

/// The folders holding homes: the homes folder of every volume a link points into (and the
/// first layout's).
pub fn used_folders(host: &dyn HomesHost) -> Vec<String> {
    let mut v: Vec<String> = host
        .links(HOMES_LINK)
        .into_iter()
        .map(|(_, t)| parent_of(t.trim_end_matches('/')).to_string())
        .filter(|p| p.ends_with(&format!("/{HOMES_FOLDER}")))
        .collect();
    v.extend(host.read_link(HOMES_LINK));
    v.sort();
    v.dedup();
    v
}

/// The homes folders holding the old homes of moves waiting for confirmation: in use (the
/// undo puts the old home back there) even when no link points into them any more.
pub fn waiting_folders(records: &[HomeMoveRecord]) -> Vec<String> {
    let mut v: Vec<String> = records
        .iter()
        .flat_map(|r| [r.kept.as_str(), r.from_folder.as_str()])
        .map(|p| parent_of(p.trim_end_matches('/')).to_string())
        .filter(|p| p.ends_with(&format!("/{HOMES_FOLDER}")))
        .collect();
    v.sort();
    v.dedup();
    v
}

// ---------------------------------------------------------------------------------------------
// moves
// ---------------------------------------------------------------------------------------------

/// Where a home goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dest {
    /// The homes folder of this volume (mount point).
    Volume(String),
    /// `/home/<user>`.
    System,
}

impl Dest {
    /// The `to` of a request: a mount point, `system`, or empty (the default volume).
    pub fn parse(to: &str, default_volume: &str) -> Dest {
        let to = to.trim();
        if to == HOME_SYSTEM {
            return Dest::System;
        }
        Dest::Volume(trim_volume(if to.is_empty() { default_volume } else { to }))
    }

    /// The destination as the API names it.
    pub fn name(&self) -> String {
        match self {
            Dest::Volume(v) => v.clone(),
            Dest::System => HOME_SYSTEM.into(),
        }
    }

    /// The passwd home of `user` there.
    pub fn passwd_home(&self, user: &str) -> String {
        match self {
            Dest::Volume(_) => home_of(user),
            Dest::System => format!("{SYSTEM_HOMES}/{user}"),
        }
    }

    /// The real folder of `user`'s home there.
    pub fn folder(&self, user: &str) -> String {
        match self {
            Dest::Volume(v) => format!("{}/{user}", folder_of(v)),
            Dest::System => format!("{SYSTEM_HOMES}/{user}"),
        }
    }

    /// `home_volume` of a user whose home is there.
    pub fn home_volume(&self) -> String {
        match self {
            Dest::Volume(v) => v.clone(),
            Dest::System => String::new(),
        }
    }
}

/// What [`plan_move`] needs to know about the rest of the configuration.
pub struct MoveContext<'a> {
    /// The service is on in the applied configuration (needed to move to a volume).
    pub service_on: bool,
    /// Mount points that can hold a homes folder.
    pub volumes: &'a [String],
    /// Paths of the shares (applied and staged).
    pub shares: &'a [(String, String)],
    /// A move of this user waits for confirmation.
    pub waiting: bool,
    /// Time for measuring the size of the home ([`PLAN_SIZE_BUDGET`], less when many homes
    /// are planned together, [`MOVE_SIZE_BUDGET`] in the task).
    pub size_budget: Duration,
}

/// Free space needed at the destination for `bytes` of homes: 10 % and 64 MiB of headroom
/// (metadata, the volume's own needs).
pub fn space_needed(bytes: u64) -> u64 {
    bytes + bytes / 10 + (64 << 20)
}

/// Space check of several homes going to the same destination (each fitting alone is not
/// enough): `Err((needed, available))` when they do not fit together, or when the sizes are
/// lower bounds that already do not fit.
pub fn batch_fits<'a>(plans: impl IntoIterator<Item = &'a HomeMovePlan>) -> Result<(), (u64, u64)> {
    let mut total = 0u64;
    let mut avail = None;
    for p in plans {
        total = total.saturating_add(p.size_bytes);
        avail = Some(avail.map_or(p.avail_bytes, |a: u64| a.min(p.avail_bytes)));
    }
    let Some(avail) = avail else {
        return Ok(());
    };
    let need = space_needed(total);
    if need > avail {
        return Err((need, avail));
    }
    Ok(())
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

/// What moving `user`'s home to `dest` does and what prevents it now.
pub fn plan_move(
    host: &dyn HomesHost,
    user: &SysUser,
    dest: &Dest,
    cx: &MoveContext,
) -> HomeMovePlan {
    let source = folder_of_home(host, user);
    let linked = link_user(&user.home) == Some(user.name.as_str());
    let mut p = HomeMovePlan {
        user: user.name.clone(),
        from: user.home.clone(),
        source: source.clone(),
        to: dest.passwd_home(&user.name),
        location: dest.name(),
        target: dest.folder(&user.name),
        ..Default::default()
    };
    let mut b = Vec::new();
    if !in_range(user.uid) {
        b.push(blocker(B::NotLocal, user.uid.to_string()));
    }
    if cx.waiting {
        b.push(blocker(B::MoveWaiting, ""));
    }
    let base = match dest {
        Dest::Volume(v) => {
            if !cx.service_on {
                b.push(blocker(B::ServiceOff, ""));
            }
            if !cx.volumes.iter().any(|x| x == v) {
                b.push(blocker(B::TargetInvalid, v.clone()));
            }
            v.clone()
        }
        Dest::System if host.probe(SYSTEM_HOMES).is_dir => SYSTEM_HOMES.to_string(),
        Dest::System => "/".to_string(),
    };
    let bi = host.probe(&base);
    p.subvolume = bi.exists && bi.fs_type == "btrfs";
    p.avail_bytes = host.avail(&base).unwrap_or(0);
    let here = source.trim_end_matches('/') == p.target;
    if here {
        b.push(blocker(B::AlreadyMoved, ""));
    } else if host.probe(&p.target).exists || host.read_link(&p.target).is_some() {
        b.push(blocker(B::TargetExists, p.target.clone()));
    }
    let src = host.probe(&source);
    if !src.exists || !src.is_dir {
        b.push(blocker(B::SourceMissing, user.home.clone()));
    } else if !here {
        if src.symlink || host.read_link(&source).is_some() || source == "/" {
            b.push(blocker(B::SourceNotPlain, user.home.clone()));
        } else if let Some(m) = host.mounts().iter().find(|m| within(&m.path, &source)) {
            b.push(blocker(B::SourceNotPlain, m.path.clone()));
        }
        // a share inside the home; a share holding the home too, unless the home is in a
        // homes folder (a share of the whole volume)
        if let Some((name, _)) = cx
            .shares
            .iter()
            .find(|(_, path)| within(path, &source) || (!linked && within(&source, path)))
        {
            b.push(blocker(B::ShareInside, name.clone()));
        }
        // a subvolume home with quotas on: its quota group, at once; else a bounded walk. A
        // size that cannot be measured at all is unknown (partial)
        let quota = if src.subvolume {
            host.qgroup_size(&source).map(|b| (b, false))
        } else {
            None
        };
        match quota.or_else(|| host.tree_size(&source, cx.size_budget)) {
            Some((bytes, partial)) => {
                p.size_bytes = bytes;
                p.size_partial = partial;
            }
            None => p.size_partial = true,
        }
        p.ssh_keys = host.probe(&format!("{source}/.ssh/authorized_keys")).exists;
        let need = space_needed(p.size_bytes);
        if bi.exists && p.avail_bytes < need {
            b.push(blocker(B::NoSpace, need.to_string()));
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
                B::AlreadyMoved => "already there",
                B::SourceMissing => "the home does not exist",
                B::SourceNotPlain => "the home is a link or has mounts",
                B::TargetExists => "the target exists",
                B::TargetInvalid => "not a volume that can hold homes",
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

/// The parent of the target: the folder of links and the volume's homes folder, or `/home`.
fn prepare_target(
    host: &dyn HomesHost,
    plan: &HomeMovePlan,
    log: &dyn Fn(&str),
) -> Result<(), String> {
    if link_user(&plan.to).is_some() {
        ensure_links_dir(host, log)?;
        ensure_folder(host, &plan.location, log)?;
    } else if !host.probe(SYSTEM_HOMES).exists {
        host.make_dir(SYSTEM_HOMES, false, 0, 0, 0o755, log)?;
    }
    Ok(())
}

/// The root-only folder a home is copied into before it goes to `target`:
/// `<parent>/.<user>.rynas-copy` (root 0700).
pub fn staging_of(target: &str, user: &str) -> String {
    format!("{}/.{user}.rynas-copy", parent_of(target))
}

/// Removes a staging folder and the copy in it.
fn remove_staging(
    host: &dyn HomesHost,
    staging: &str,
    copy: &str,
    log: &dyn Fn(&str),
) -> Result<(), String> {
    let s = host.probe(staging);
    let link = s.symlink || host.read_link(staging).is_some();
    if !s.exists && !link {
        return Ok(());
    }
    if link || !s.is_dir {
        return Err(format!("{staging} exists and is not a folder: not removed"));
    }
    let c = host.probe(copy);
    if c.exists && !c.symlink {
        host.remove(copy, c.subvolume, log)?;
    }
    host.remove(staging, false, log)
}

/// Copies a home to its destination: the copy is made in a root-only staging folder next to
/// the target (subvolume on btrfs), rsync pass, live check, second pass with deletions,
/// verification (a dry run must find nothing left and the counts must match); only then is
/// it given to the user (owner-only mode) and renamed to the target. Until then nobody but
/// root can reach it: the user cannot plant links in the tree root's rsync writes into. On
/// failure the copy is removed. Neither the link nor the passwd home is touched.
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
    prepare_target(host, plan, log)?;
    let staging = staging_of(&plan.target, &plan.user);
    let copy = format!("{staging}/{}", plan.user);
    // a staging folder left by an interrupted move
    remove_staging(host, &staging, &copy, log)?;
    host.make_dir(&staging, false, 0, 0, 0o700, log)?;
    if let Err(e) = host.make_dir(&copy, plan.subvolume, 0, 0, HOME_MODE, log) {
        let _ = host.remove(&staging, false, log);
        return Err(e);
    }
    let run = || -> Result<(), String> {
        log(&format!(
            "{}: copying {} to {}",
            plan.user, plan.source, plan.target
        ));
        host.sync(&plan.source, &copy, false, log, cancelled)?;
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
        host.sync(&plan.source, &copy, true, log, cancelled)?;
        progress(85);
        let left = host.differences(&plan.source, &copy)?;
        if !left.is_empty() {
            return Err(format!(
                "{}: the copy differs from the home ({} entries, first: {})",
                plan.user,
                left.len(),
                left[0]
            ));
        }
        let (a, b) = (host.tree_counts(&plan.source), host.tree_counts(&copy));
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
        if host.probe(&plan.target).exists || host.read_link(&plan.target).is_some() {
            return Err(format!("{}: {} exists now", plan.user, plan.target));
        }
        host.set_owner_mode(&copy, user.uid, user.gid, HOME_MODE, log)?;
        host.rename(&copy, &plan.target, log)
    };
    if let Err(e) = run() {
        log(&format!("{}: removing the incomplete copy", plan.user));
        if let Err(e2) = remove_staging(host, &staging, &copy, log) {
            log(&format!("{staging}: {e2}"));
        }
        return Err(e);
    }
    if let Err(e) = host.remove(&staging, false, log) {
        log(&format!("{staging}: {e}"));
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

/// The real folders before and after a move (the first records named only passwd homes, and
/// moved only from a plain folder to the homes folder).
pub fn record_folders(host: &dyn HomesHost, r: &HomeMoveRecord) -> (String, String) {
    let from = if r.from_folder.is_empty() {
        r.from.clone()
    } else {
        r.from_folder.clone()
    };
    let to = if !r.to_folder.is_empty() {
        r.to_folder.clone()
    } else if link_user(&r.to).is_some() {
        real_home(host, &r.user).unwrap_or_else(|_| r.to.clone())
    } else {
        r.to.clone()
    };
    (from, to)
}

/// Puts the old folder back in place, and its link for a home on a volume; the caller then
/// switches the passwd home back if it changed and moves the copy aside ([`set_aside`]).
pub fn undo_move(
    host: &dyn HomesHost,
    r: &HomeMoveRecord,
    from_folder: &str,
    log: &dyn Fn(&str),
) -> Result<(), String> {
    if host.probe(from_folder).exists {
        return Err(format!("{from_folder} exists again: nothing undone"));
    }
    if !host.probe(&r.kept).exists {
        return Err(format!(
            "{} does not exist any more: nothing undone",
            r.kept
        ));
    }
    host.rename(&r.kept, from_folder, log)?;
    if link_user(&r.from).is_some() {
        point_link(host, &r.user, from_folder, log)?;
    }
    Ok(())
}

/// Moves the copy of an undone move aside, next to it, as `.<user>.undone-<ms>` (kept: it may
/// hold newer files).
pub fn set_aside(
    host: &dyn HomesHost,
    user: &str,
    folder: &str,
    now_ms: u64,
    log: &dyn Fn(&str),
) -> Result<Option<String>, String> {
    if !host.probe(folder).exists || host.read_link(folder).is_some() {
        return Ok(None);
    }
    let aside = format!("{}/.{user}.undone-{now_ms}", parent_of(folder));
    host.rename(folder, &aside, log)?;
    Ok(Some(aside))
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
    /// Held across read-modify-write ([`MoveStore::put`], [`MoveStore::remove`]): moves,
    /// confirmations and undos of different users end in parallel tasks.
    edit: Mutex<()>,
}

impl MoveStore {
    pub fn memory() -> Self {
        Self::default()
    }

    pub fn in_dir(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            file: Some(state_dir.into().join(MOVES_FILE)),
            ..Default::default()
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
        let _edit = self.edit.lock().unwrap();
        let mut v = self.list();
        v.retain(|x| x.user != r.user);
        v.push(r);
        self.save(v)
    }

    pub fn remove(&self, user: &str) -> Result<(), String> {
        let _edit = self.edit.lock().unwrap();
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

/// Processes (not rynasd) whose `/proc/<pid>/status` lists `id` on one of the `fields` lines.
fn proc_status_with(fields: &[&str], id: u32) -> Vec<u32> {
    let me = std::process::id();
    let id = id.to_string();
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
        let hit = st.lines().any(|l| {
            fields.iter().any(|f| {
                l.strip_prefix(f)
                    .is_some_and(|ids| ids.split_whitespace().any(|x| x == id))
            })
        });
        if hit {
            out.push(pid);
        }
    }
    out.sort_unstable();
    out
}

impl HomesHost for HostHomes {
    fn probe(&self, path: &str) -> PathInfo {
        crate::host::ShareHost::probe(&crate::host::HostShares, path)
    }

    fn read_link(&self, path: &str) -> Option<String> {
        std::fs::read_link(path)
            .ok()
            .and_then(|p| p.into_os_string().into_string().ok())
    }

    fn links(&self, dir: &str) -> Vec<(String, String)> {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut v: Vec<(String, String)> = rd
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_symlink()))
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let target = std::fs::read_link(e.path())
                    .ok()?
                    .into_os_string()
                    .into_string()
                    .ok()?;
                Some((name, target))
            })
            .collect();
        v.sort();
        v
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

    fn owner_mode(&self, path: &str) -> Option<(u32, u32)> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::symlink_metadata(path).ok()?;
        Some((m.uid(), m.mode() & 0o7777))
    }

    fn qgroup_size(&self, path: &str) -> Option<u64> {
        crate::host::ShareHost::qgroup_size(&crate::host::HostShares, path)
    }

    fn processes(&self, uid: u32) -> Vec<u32> {
        proc_status_with(&["Uid:"], uid)
    }

    fn group_processes(&self, gid: u32) -> Vec<u32> {
        proc_status_with(&["Gid:", "Groups:"], gid)
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

    fn unlink(&self, path: &str, log: &dyn Fn(&str)) -> Result<(), String> {
        let m = std::fs::symlink_metadata(path).map_err(|e| format!("{path}: {e}"))?;
        if !m.file_type().is_symlink() {
            return Err(format!("{path} is not a symbolic link: not removed"));
        }
        log(&format!("remove link {path}"));
        std::fs::remove_file(path).map_err(|e| format!("{path}: {e}"))
    }

    fn exchange(&self, a: &str, b: &str, log: &dyn Fn(&str)) -> Result<(), String> {
        use nix::fcntl::{AT_FDCWD, RenameFlags, renameat2};
        log(&format!("exchange {a} ⇄ {b}"));
        renameat2(AT_FDCWD, a, AT_FDCWD, b, RenameFlags::RENAME_EXCHANGE)
            .map_err(|e| format!("{a} ⇄ {b}: {e}"))
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
    /// Processes by gid (`group_processes`).
    pub group_processes: BTreeMap<u32, Vec<u32>>,
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
    /// Folders whose size is measured only partly (the budget runs out).
    pub partial_sizes: Vec<String>,
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

/// Removes `root` and everything below it.
fn take_tree(entries: &mut BTreeMap<String, MemEntry>, root: &str) -> Vec<(String, MemEntry)> {
    let prefix = format!("{root}/");
    let keys: Vec<String> = entries
        .keys()
        .filter(|k| *k == root || k.starts_with(&prefix))
        .cloned()
        .collect();
    keys.into_iter()
        .filter_map(|k| entries.remove(&k).map(|e| (k, e)))
        .collect()
}

/// Puts a tree taken from `from` back below `to`.
fn put_tree(
    entries: &mut BTreeMap<String, MemEntry>,
    from: &str,
    to: &str,
    tree: Vec<(String, MemEntry)>,
) {
    for (k, e) in tree {
        entries.insert(format!("{to}{}", &k[from.len()..]), e);
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
                nocow: None,
                root_only: false,
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

    fn links(&self, dir: &str) -> Vec<(String, String)> {
        let s = self.state();
        let v: Vec<(String, String)> = s
            .entries
            .iter()
            .filter_map(|(p, e)| {
                let name = p.strip_prefix(dir)?.strip_prefix('/')?;
                if name.contains('/') {
                    return None;
                }
                Some((name.to_string(), e.link.clone()?))
            })
            .collect();
        if v.is_empty() && self.probe_host && !s.entries.contains_key(dir) {
            drop(s);
            return HostHomes.links(dir);
        }
        v
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

    fn tree_size(&self, path: &str, budget: Duration) -> Option<(u64, bool)> {
        let real = self.resolve(path);
        let s = self.state();
        match s.entries.get(&real) {
            Some(e) => Some((e.bytes, s.partial_sizes.contains(&real))),
            None if self.probe_host => {
                drop(s);
                HostHomes.tree_size(path, budget)
            }
            None => None,
        }
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

    fn owner_mode(&self, path: &str) -> Option<(u32, u32)> {
        let s = self.state();
        match s.entries.get(path) {
            Some(e) if e.link.is_some() => Some((0, 0o777)),
            Some(e) => Some((e.uid, e.mode)),
            None if self.probe_host => {
                drop(s);
                HostHomes.owner_mode(path)
            }
            None => None,
        }
    }

    fn processes(&self, uid: u32) -> Vec<u32> {
        self.state()
            .processes
            .get(&uid)
            .cloned()
            .unwrap_or_default()
    }

    fn group_processes(&self, gid: u32) -> Vec<u32> {
        self.state()
            .group_processes
            .get(&gid)
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
        if !s.entries.contains_key(&rf) {
            return Err(format!("{from}: no such folder"));
        }
        let moved = take_tree(&mut s.entries, &rf);
        put_tree(&mut s.entries, &rf, &rt, moved);
        Ok(())
    }

    fn unlink(&self, path: &str, log: &dyn Fn(&str)) -> Result<(), String> {
        let line = format!("unlink {path}");
        log(&line);
        self.write("unlink", line)?;
        let mut s = self.state();
        match s.entries.get(path) {
            Some(e) if e.link.is_some() => {
                s.entries.remove(path);
                Ok(())
            }
            _ => Err(format!("{path} is not a symbolic link: not removed")),
        }
    }

    fn exchange(&self, a: &str, b: &str, log: &dyn Fn(&str)) -> Result<(), String> {
        let line = format!("exchange {a} <-> {b}");
        log(&line);
        self.write("exchange", line)?;
        let mut s = self.state();
        if !s.entries.contains_key(a) || !s.entries.contains_key(b) {
            return Err(format!("{a} <-> {b}: both must exist"));
        }
        let ta = take_tree(&mut s.entries, a);
        let tb = take_tree(&mut s.entries, b);
        put_tree(&mut s.entries, a, b, ta);
        put_tree(&mut s.entries, b, a, tb);
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
        take_tree(&mut self.state().entries, &real);
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

    fn on(v: &str) -> HomesConfig {
        HomesConfig {
            enabled: true,
            volume: v.into(),
        }
    }

    fn setup() -> (Arc<MemoryHomes>, Arc<MemoryAccounts>, HomesDomain) {
        let h = Arc::new(MemoryHomes::new());
        h.add_volume("/volume1", "btrfs", 1 << 40);
        h.add_volume("/volume2", "btrfs", 1 << 40);
        h.add_volume("/data", "ext4", 1 << 30);
        h.add_volume("/", "ext4", 1 << 30);
        h.add_dir("/home", 0, 0);
        let acc = Arc::new(MemoryAccounts::new());
        let d = HomesDomain::new(h.clone(), acc.clone());
        (h, acc, d)
    }

    #[test]
    fn paths() {
        assert_eq!(home_of("alice"), "/srv/rynas/homes/alice");
        assert_eq!(folder_of("/volume1/"), "/volume1/homes");
        assert_eq!(link_user("/srv/rynas/homes/alice"), Some("alice"));
        assert_eq!(link_user("/srv/rynas/homes/alice/x"), None);
        assert_eq!(link_user("/srv/rynas/homes/"), None);
        assert_eq!(link_user("/srv/rynas/homesx/alice"), None);
        assert_eq!(link_user("/home/alice"), None);
        assert_eq!(kept_path("/home/marco/"), "/home/marco.rynas-old");
        assert_eq!(
            volume_of_folder("/volume1/homes/alice", "alice").as_deref(),
            Some("/volume1")
        );
        assert_eq!(volume_of_folder("/home/alice", "alice"), None);
        assert_eq!(
            Dest::parse("", "/volume1/"),
            Dest::Volume("/volume1".into())
        );
        assert_eq!(Dest::parse("system", "/volume1"), Dest::System);
        assert_eq!(Dest::System.folder("bob"), "/home/bob");
        assert_eq!(Dest::Volume("/v2".into()).folder("bob"), "/v2/homes/bob");
        assert_eq!(
            Dest::Volume("/v2".into()).passwd_home("bob"),
            home_of("bob")
        );
    }

    #[test]
    fn volume_choices_leave_out_the_system() {
        let m = |path: &str, fs: &str| Mount {
            path: path.into(),
            fs_type: fs.into(),
            ..Default::default()
        };
        let mounts = vec![
            m("/", "ext4"),
            m("/boot/efi", "vfat"),
            m("/var/lib/docker", "btrfs"),
            m("/srv/data", "xfs"),
            m("/volume1", "btrfs"),
            m("/media/usb", "ext4"),
        ];
        let c = candidates(&mounts, &[("volume1".into(), "/volume1".into())]);
        let paths: Vec<&str> = c.iter().map(|v| v.path.as_str()).collect();
        assert_eq!(paths, ["/volume1", "/srv/data"], "rynas volumes first");
        assert_eq!(c[0].label.as_deref(), Some("volume1"));
    }

    #[test]
    fn settings_are_checked() {
        let (_, _, d) = setup();
        let ok = |v: &str| d.normalize(on(v));
        assert_eq!(ok("/volume1/").unwrap().volume, "/volume1");
        assert!(ok("").is_err());
        assert!(ok("/").is_err(), "the system disk");
        assert!(ok("/nowhere").is_err(), "not mounted");
        assert!(ok("/volume1/../etc").is_err());
        assert!(ok("relative").is_err());
        assert!(ok("/data").is_ok(), "any mounted local filesystem");
        // off: anything goes except garbage
        let off = |v: &str| {
            d.normalize(HomesConfig {
                enabled: false,
                volume: v.into(),
            })
        };
        assert!(off("/nowhere").is_ok());
        assert!(off("bad").is_err());
    }

    #[test]
    fn the_default_volume_changes_with_homes_on_the_old_one() {
        let (h, acc, d) = setup();
        d.ensure(&on("/volume1"), &quiet).unwrap();
        acc.state()
            .users
            .push(user("alice", 1001, "/srv/rynas/homes/alice"));
        create_home(
            h.as_ref(),
            "alice",
            Some("/volume1"),
            1001,
            100,
            false,
            &quiet,
        )
        .unwrap();
        // alice's home stays on volume1, new homes go to volume2
        assert!(d.normalize(on("/volume2")).is_ok());
        d.ensure(&on("/volume2"), &quiet).unwrap();
        assert!(h.probe("/volume2/homes").is_dir);
        assert_eq!(
            h.read_link("/srv/rynas/homes/alice").as_deref(),
            Some("/volume1/homes/alice")
        );
        assert_eq!(
            used_folders(h.as_ref()),
            ["/volume1/homes"],
            "the folders that hold homes"
        );
    }

    #[test]
    fn enabling_creates_the_folder_of_links_and_the_homes_folder_once() {
        let (h, _, d) = setup();
        d.ensure(&on("/volume1"), &quiet).unwrap();
        d.ensure(&on("/volume1"), &quiet).unwrap();
        assert_eq!(
            h.state().log,
            [
                "mkdir /srv/rynas 0:0 755",
                "mkdir /srv/rynas/homes 0:0 711",
                "subvolume /volume1/homes 0:0 711",
            ]
        );
        let p = h.probe(HOMES_LINK);
        assert!(p.is_dir && !p.symlink);
        // on ext4 a plain folder
        d.ensure(&on("/data"), &quiet).unwrap();
        assert!(
            h.state()
                .log
                .contains(&"mkdir /data/homes 0:0 711".to_string())
        );
        // an empty mount point is not a volume
        h.add_dir("/volume9", 0, 0);
        assert!(ensure_folder(h.as_ref(), "/volume9", &quiet).is_err());
        // an existing homes folder others can write into (or not root's) is not used
        h.add_volume("/volume3", "ext4", 1 << 40);
        h.add_dir("/volume3/homes", 0, 0);
        h.state().entries.get_mut("/volume3/homes").unwrap().mode = 0o777;
        let e = ensure_folder(h.as_ref(), "/volume3", &quiet).unwrap_err();
        assert!(e.contains("not owned by root"), "{e}");
        h.state().entries.get_mut("/volume3/homes").unwrap().mode = 0o755;
        h.state().entries.get_mut("/volume3/homes").unwrap().uid = 1000;
        assert!(ensure_folder(h.as_ref(), "/volume3", &quiet).is_err());
        h.state().entries.get_mut("/volume3/homes").unwrap().uid = 0;
        assert_eq!(
            ensure_folder(h.as_ref(), "/volume3", &quiet).unwrap(),
            "/volume3/homes"
        );
    }

    #[test]
    fn the_first_layout_becomes_per_user_links() {
        let (h, acc, d) = setup();
        // as the first version left it: the whole folder is a link to volume1/homes
        h.add_dir("/srv/rynas", 0, 0);
        h.add_dir("/volume1/homes", 0, 0);
        h.add_dir("/volume1/homes/alice", 1001, 10);
        h.add_dir("/volume1/homes/marco", 1000, 10);
        h.add_dir("/volume1/homes/marco/.ssh", 1000, 1);
        h.set_link(HOMES_LINK, "/volume1/homes", &quiet).unwrap();
        acc.state().users.extend([
            user("alice", 1001, "/srv/rynas/homes/alice"),
            user("marco", 1000, "/srv/rynas/homes/marco"),
            user("bob", 1002, "/home/bob"),
        ]);
        assert_eq!(
            real_home(h.as_ref(), "alice").unwrap(),
            "/volume1/homes/alice"
        );
        h.state().log.clear();
        assert!(d.migrate(&quiet).unwrap());
        assert_eq!(
            h.state().log,
            [
                "mkdir /srv/rynas/homes.rynas-new 0:0 711",
                "link /srv/rynas/homes.rynas-new/alice -> /volume1/homes/alice",
                "link /srv/rynas/homes.rynas-new/marco -> /volume1/homes/marco",
                "exchange /srv/rynas/homes.rynas-new <-> /srv/rynas/homes",
                "unlink /srv/rynas/homes.rynas-new",
            ]
        );
        assert!(h.read_link(HOMES_LINK).is_none() && h.probe(HOMES_LINK).is_dir);
        assert!(
            h.probe("/srv/rynas/homes/marco/.ssh").exists,
            "homes resolve"
        );
        assert!(!h.probe("/srv/rynas/homes/bob").exists);
        assert!(!h.probe("/srv/rynas/homes.rynas-new").exists);
        // idempotent
        h.state().log.clear();
        assert!(!d.migrate(&quiet).unwrap());
        d.ensure(&on("/volume1"), &quiet).unwrap();
        assert!(h.state().log.is_empty(), "{:?}", h.state().log);
        // a migration stopped half way is redone
        let (h, acc, d) = setup();
        h.add_dir("/volume1/homes/alice", 1001, 10);
        h.set_link(HOMES_LINK, "/volume1/homes", &quiet).unwrap();
        h.add_dir("/srv/rynas/homes.rynas-new", 0, 0);
        acc.state()
            .users
            .push(user("alice", 1001, "/srv/rynas/homes/alice"));
        h.state().fail.push("exchange".into());
        assert!(d.migrate(&quiet).is_err());
        assert_eq!(h.read_link(HOMES_LINK).as_deref(), Some("/volume1/homes"));
        h.state().fail.clear();
        assert!(d.migrate(&quiet).unwrap());
        assert!(h.probe("/srv/rynas/homes/alice").is_dir);
    }

    #[test]
    fn homes_of_new_users_are_subvolumes_on_their_volume() {
        let (h, _, d) = setup();
        d.ensure(&on("/volume1"), &quiet).unwrap();
        let real = create_home(
            h.as_ref(),
            "alice",
            Some("/volume2"),
            1001,
            100,
            false,
            &quiet,
        )
        .unwrap();
        assert_eq!(real, "/volume2/homes/alice");
        assert_eq!(
            h.read_link("/srv/rynas/homes/alice").as_deref(),
            Some("/volume2/homes/alice")
        );
        let p = h.probe("/srv/rynas/homes/alice");
        assert!(p.exists && p.subvolume);
        assert_eq!(h.state().entries["/volume2/homes/alice"].mode, 0o700);
        assert_eq!(h.state().entries["/volume2/homes"].mode, 0o711);
        assert!(
            create_home(
                h.as_ref(),
                "alice",
                Some("/volume2"),
                1002,
                100,
                false,
                &quiet
            )
            .is_err(),
            "another user's folder is not reused"
        );
        assert!(
            create_home(
                h.as_ref(),
                "alice",
                Some("/volume2"),
                1001,
                100,
                false,
                &quiet
            )
            .is_ok(),
            "an empty folder of the same uid is (a rolled back apply)"
        );
        assert!(
            create_home(
                h.as_ref(),
                "alice",
                Some("/volume1"),
                1001,
                100,
                false,
                &quiet
            )
            .is_err(),
            "the link of an existing home elsewhere is not replaced"
        );
        h.add_dir("/volume2/homes/alice/notes", 1001, 1);
        assert!(
            create_home(
                h.as_ref(),
                "alice",
                Some("/volume2"),
                1001,
                100,
                false,
                &quiet
            )
            .is_err(),
            "a folder with files is not reused"
        );
        h.state().entries.remove("/volume2/homes/alice/notes");
        let i = home_info(h.as_ref(), &user("alice", 1001, "/srv/rynas/homes/alice"));
        assert_eq!(
            i,
            HomeInfo {
                location: HomeLocation::Volume,
                folder: "/volume2/homes/alice".into(),
                volume: "/volume2".into(),
                subvolume: true,
            }
        );
        assert!(remove_home(h.as_ref(), "/srv/rynas/homes/alice", &quiet).unwrap());
        let log = h.state().log.clone();
        assert!(log.contains(&"subvolume-delete /volume2/homes/alice".to_string()));
        assert!(log.contains(&"unlink /srv/rynas/homes/alice".to_string()));
        assert!(h.read_link("/srv/rynas/homes/alice").is_none());
        assert!(!remove_home(h.as_ref(), "/home/bob", &quiet).unwrap());
        // a volume that is not mounted
        assert!(
            create_home(
                h.as_ref(),
                "carl",
                Some("/volume7"),
                1003,
                100,
                false,
                &quiet
            )
            .is_err()
        );
    }

    const VOLS: &[String] = &[];

    fn vols() -> Vec<String> {
        vec!["/volume1".into(), "/volume2".into(), "/data".into()]
    }

    fn cx(v: &[String]) -> MoveContext<'_> {
        MoveContext {
            service_on: true,
            volumes: v,
            shares: &[],
            waiting: false,
            size_budget: PLAN_SIZE_BUDGET,
        }
    }

    fn ready() -> (Arc<MemoryHomes>, SysUser) {
        let (h, _, d) = setup();
        d.ensure(&on("/volume1"), &quiet).unwrap();
        h.add_dir("/home/marco", 1000, 5 << 30);
        h.add_dir("/home/marco/.ssh/authorized_keys", 1000, 1);
        h.state().log.clear();
        (h, user("marco", 1000, "/home/marco"))
    }

    fn v1() -> Dest {
        Dest::Volume("/volume1".into())
    }

    #[test]
    fn a_move_plan_says_what_happens() {
        let (h, u) = ready();
        let v = vols();
        let p = plan_move(h.as_ref(), &u, &v1(), &cx(&v));
        assert!(p.allowed, "{:?}", p.blockers);
        assert_eq!(p.to, "/srv/rynas/homes/marco");
        assert_eq!(p.source, "/home/marco");
        assert_eq!(p.target, "/volume1/homes/marco");
        assert_eq!(p.location, "/volume1");
        assert!(p.subvolume && p.ssh_keys);
        assert_eq!(p.size_bytes, 5 << 30);
        assert!(h.state().log.is_empty(), "a plan changes nothing");
        // to the system disk where it is: nothing to do
        let p = plan_move(h.as_ref(), &u, &Dest::System, &cx(&v));
        assert_eq!(p.blockers[0].code, B::AlreadyMoved);
        // to ext4: a plain folder
        let p = plan_move(h.as_ref(), &u, &Dest::Volume("/data".into()), &cx(&v));
        assert!(!p.subvolume);
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
            &Dest::Volume("/volume5".into()),
            &MoveContext {
                service_on: false,
                volumes: VOLS,
                shares: &shares,
                waiting: true,
                size_budget: PLAN_SIZE_BUDGET,
            },
        );
        let codes: Vec<B> = p.blockers.iter().map(|b| b.code).collect();
        assert!(!p.allowed);
        for c in [
            B::ServiceOff,
            B::TargetInvalid,
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
        // no space, target exists, mounts inside
        let (h, u) = ready();
        let v = vols();
        h.state().mounts[0].avail_bytes = 1 << 30;
        h.add_dir("/volume1/homes/marco", 1000, 0);
        h.state().mounts.push(Mount {
            path: "/home/marco/nfs".into(),
            fs_type: "ext4".into(),
            ..Default::default()
        });
        let codes: Vec<B> = plan_move(h.as_ref(), &u, &v1(), &cx(&v))
            .blockers
            .iter()
            .map(|b| b.code)
            .collect();
        assert!(codes.contains(&B::NoSpace) && codes.contains(&B::TargetExists));
        assert!(codes.contains(&B::SourceNotPlain), "{codes:?}");
        let gone = user("ghost", 1003, "/home/ghost");
        assert!(
            plan_move(h.as_ref(), &gone, &v1(), &cx(&v))
                .blockers
                .iter()
                .any(|b| b.code == B::SourceMissing)
        );
        let sys = user("daemon", 1, "/usr/sbin");
        assert!(
            plan_move(h.as_ref(), &sys, &v1(), &cx(&v))
                .blockers
                .iter()
                .any(|b| b.code == B::NotLocal)
        );
        // the system disk needs no service
        let p = plan_move(
            h.as_ref(),
            &user("ghost", 1003, "/home/ghost"),
            &Dest::System,
            &MoveContext {
                service_on: false,
                ..cx(&v)
            },
        );
        assert!(!p.blockers.iter().any(|b| b.code == B::ServiceOff));
    }

    #[test]
    fn a_move_copies_verifies_keeps_and_can_be_undone() {
        let (h, u) = ready();
        let v = vols();
        let p = plan_move(h.as_ref(), &u, &v1(), &cx(&v));
        copy_home(h.as_ref(), &p, &u, &quiet, &|_| {}, &|| false).unwrap();
        assert_eq!(
            h.state().log,
            [
                // root's until verified: the user cannot reach the tree rsync writes into
                "mkdir /volume1/homes/.marco.rynas-copy 0:0 700",
                "subvolume /volume1/homes/.marco.rynas-copy/marco 0:0 700",
                "rsync /home/marco/ /volume1/homes/.marco.rynas-copy/marco/",
                "rsync --delete /home/marco/ /volume1/homes/.marco.rynas-copy/marco/",
                "owner /volume1/homes/.marco.rynas-copy/marco 1000:100 700",
                "rename /volume1/homes/.marco.rynas-copy/marco -> /volume1/homes/marco",
                "rm /volume1/homes/.marco.rynas-copy",
            ]
        );
        let t = h.state().entries["/volume1/homes/marco"].clone();
        assert!(t.subvolume && (t.uid, t.mode) == (1000, 0o700));
        assert!(!h.probe("/volume1/homes/.marco.rynas-copy").exists);
        point_link(h.as_ref(), "marco", &p.target, &quiet).unwrap();
        let kept = keep_old(h.as_ref(), "/home/marco", 7, &quiet).unwrap();
        assert_eq!(kept, "/home/marco.rynas-old");
        assert!(h.probe("/home/marco.rynas-old/.ssh/authorized_keys").exists);
        let r = HomeMoveRecord {
            user: "marco".into(),
            from: "/home/marco".into(),
            to: p.to.clone(),
            kept: kept.clone(),
            moved_ms: 7,
            from_folder: p.source.clone(),
            to_folder: p.target.clone(),
        };
        let (from, to) = record_folders(h.as_ref(), &r);
        undo_move(h.as_ref(), &r, &from, &quiet).unwrap();
        let aside = set_aside(h.as_ref(), "marco", &to, 9, &quiet).unwrap();
        assert_eq!(aside.as_deref(), Some("/volume1/homes/.marco.undone-9"));
        assert!(h.probe("/home/marco").exists);
        assert!(!h.probe("/volume1/homes/marco").exists);
        // a record of the first version (no folders) undoes the same way
        let first = HomeMoveRecord {
            from_folder: String::new(),
            to_folder: String::new(),
            ..r.clone()
        };
        assert_eq!(
            record_folders(h.as_ref(), &first),
            ("/home/marco".into(), "/volume1/homes/marco".into()),
            "the link still names the copy: rynasd removes it after the passwd switch"
        );
        // a second keep while the first is there gets a unique name
        h.add_dir("/home/marco.rynas-old", 1000, 0);
        assert_eq!(
            keep_old(h.as_ref(), "/home/marco", 11, &quiet).unwrap(),
            "/home/marco.rynas-old-11"
        );
    }

    #[test]
    fn a_home_moves_between_volumes_and_back_to_the_system_disk() {
        let (h, _, d) = setup();
        d.ensure(&on("/volume1"), &quiet).unwrap();
        create_home(
            h.as_ref(),
            "alice",
            Some("/volume1"),
            1001,
            100,
            false,
            &quiet,
        )
        .unwrap();
        let u = user("alice", 1001, "/srv/rynas/homes/alice");
        let v = vols();
        // volume1 → volume2: the passwd home stays, the link changes
        let p = plan_move(h.as_ref(), &u, &Dest::Volume("/volume2".into()), &cx(&v));
        assert!(p.allowed, "{:?}", p.blockers);
        assert_eq!(
            (p.from.as_str(), p.to.as_str()),
            (u.home.as_str(), u.home.as_str())
        );
        assert_eq!(p.source, "/volume1/homes/alice");
        assert_eq!(p.target, "/volume2/homes/alice");
        copy_home(h.as_ref(), &p, &u, &quiet, &|_| {}, &|| false).unwrap();
        assert!(h.probe("/volume2/homes").is_dir, "the homes folder is made");
        point_link(h.as_ref(), "alice", &p.target, &quiet).unwrap();
        let kept = keep_old(h.as_ref(), &p.source, 5, &quiet).unwrap();
        assert_eq!(kept, "/volume1/homes/alice.rynas-old");
        assert_eq!(home_info(h.as_ref(), &u).volume, "/volume2");
        // already there
        let p2 = plan_move(h.as_ref(), &u, &Dest::Volume("/volume2".into()), &cx(&v));
        assert!(p2.blockers.iter().any(|b| b.code == B::AlreadyMoved));
        // undo: the link goes back to volume1
        let r = HomeMoveRecord {
            user: "alice".into(),
            from: u.home.clone(),
            to: u.home.clone(),
            kept,
            moved_ms: 5,
            from_folder: p.source.clone(),
            to_folder: p.target.clone(),
        };
        undo_move(h.as_ref(), &r, &p.source, &quiet).unwrap();
        assert_eq!(
            h.read_link("/srv/rynas/homes/alice").as_deref(),
            Some("/volume1/homes/alice")
        );
        set_aside(h.as_ref(), "alice", &p.target, 6, &quiet).unwrap();
        assert!(h.probe("/volume2/homes/.alice.undone-6").exists);
        // to the system disk: /home/alice, the passwd home changes
        let p = plan_move(h.as_ref(), &u, &Dest::System, &cx(&v));
        assert!(p.allowed, "{:?}", p.blockers);
        assert_eq!(p.to, "/home/alice");
        assert_eq!(p.target, "/home/alice");
        assert_eq!(p.location, "system");
        assert!(!p.subvolume, "the system disk is ext4 here");
        copy_home(h.as_ref(), &p, &u, &quiet, &|_| {}, &|| false).unwrap();
        assert!(h.probe("/home/alice").is_dir);
    }

    #[test]
    fn a_failed_copy_is_removed_and_the_home_untouched() {
        let (h, u) = ready();
        let v = vols();
        let p = plan_move(h.as_ref(), &u, &v1(), &cx(&v));
        h.state().differences = vec![">f.st...... notes.txt".into()];
        let e = copy_home(h.as_ref(), &p, &u, &quiet, &|_| {}, &|| false).unwrap_err();
        assert!(e.contains("differs"), "{e}");
        assert!(!h.probe("/volume1/homes/marco").exists);
        assert!(!h.probe("/volume1/homes/.marco.rynas-copy").exists);
        assert!(h.probe("/home/marco").exists);
        // a staging folder left by an interrupted move is cleared first
        h.state().differences.clear();
        h.add_dir("/volume1/homes/.marco.rynas-copy", 0, 0);
        h.add_dir("/volume1/homes/.marco.rynas-copy/marco", 1000, 0);
        copy_home(h.as_ref(), &p, &u, &quiet, &|_| {}, &|| false).unwrap();
        assert!(h.probe("/volume1/homes/marco").exists);
        assert!(!h.probe("/volume1/homes/.marco.rynas-copy").exists);
        // the user logs in during the copy: same
        let (h, u) = ready();
        let p = plan_move(h.as_ref(), &u, &v1(), &cx(&v));
        h.state().processes.insert(1000, vec![1]);
        assert!(copy_home(h.as_ref(), &p, &u, &quiet, &|_| {}, &|| false).is_err());
        assert!(!h.probe("/volume1/homes/marco").exists);
        // a blocked plan copies nothing
        let mut p2 = p.clone();
        p2.allowed = false;
        assert!(copy_home(h.as_ref(), &p2, &u, &quiet, &|_| {}, &|| false).is_err());
    }

    #[test]
    fn sizes_partial_or_unknown_and_batches_that_do_not_fit() {
        let (h, u) = ready();
        let v = vols();
        // a walk cut by the budget is a lower bound
        h.state().partial_sizes.push("/home/marco".into());
        let p = plan_move(h.as_ref(), &u, &v1(), &cx(&v));
        assert!(p.size_partial && p.size_bytes == 5 << 30);
        // a size that cannot be read at all is unknown, not zero
        h.state().entries.get_mut("/home/marco").unwrap().bytes = 0;
        let g = MemoryHomes::new();
        assert_eq!(g.tree_size("/nowhere", PLAN_SIZE_BUDGET), None);
        // together: each fits alone, not all of them
        let one = HomeMovePlan {
            size_bytes: 40 << 30,
            avail_bytes: 100 << 30,
            ..Default::default()
        };
        assert!(batch_fits([&one]).is_ok());
        assert!(batch_fits([&one, &one]).is_ok());
        let (need, avail) = batch_fits([&one, &one, &one]).unwrap_err();
        assert_eq!((need, avail), (space_needed(120 << 30), 100 << 30));
        assert!(batch_fits(std::iter::empty::<&HomeMovePlan>()).is_ok());
    }

    #[test]
    fn folders_of_waiting_moves_are_in_use() {
        let r = |kept: &str, from: &str| HomeMoveRecord {
            kept: kept.into(),
            from_folder: from.into(),
            ..Default::default()
        };
        assert_eq!(
            waiting_folders(&[
                r("/volume1/homes/alice.rynas-old", "/volume1/homes/alice"),
                r("/home/marco.rynas-old", "/home/marco"),
                r("/volume2/homes/bob.rynas-old-7", ""),
            ]),
            ["/volume1/homes", "/volume2/homes"]
        );
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
            ..Default::default()
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
        // a record of the first version reads
        let r: HomeMoveRecord = serde_json::from_str(
            r#"{"user":"m","from":"/home/m","to":"/srv/rynas/homes/m","kept":"/home/m.rynas-old","moved_ms":1}"#,
        )
        .unwrap();
        assert!(r.from_folder.is_empty() && r.to_folder.is_empty());
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

    #[test]
    fn host_links_and_exchange_on_a_real_folder() {
        let t = tempfile::tempdir().unwrap();
        let p = |s: &str| t.path().join(s).to_string_lossy().into_owned();
        std::fs::create_dir(p("dir")).unwrap();
        std::fs::create_dir(p("real")).unwrap();
        std::os::unix::fs::symlink(p("real"), p("link")).unwrap();
        HostHomes
            .set_link(&p("dir/alice"), &p("real"), &quiet)
            .unwrap();
        assert_eq!(
            HostHomes.links(&p("dir")),
            [("alice".to_string(), p("real"))]
        );
        HostHomes.exchange(&p("dir"), &p("link"), &quiet).unwrap();
        assert!(
            std::fs::symlink_metadata(p("dir"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(std::fs::symlink_metadata(p("link")).unwrap().is_dir());
        assert!(
            HostHomes.unlink(&p("link"), &quiet).is_err(),
            "a folder is not unlinked"
        );
        HostHomes.unlink(&p("dir"), &quiet).unwrap();
        assert!(
            std::path::Path::new(&p("real")).is_dir(),
            "the target stays"
        );
    }
}
