//! Stack folders on disk, the source of truth (rule 1 of `docs/phase2/docker-apps.md`):
//! `/<vol>/docker/stacks/<name>/compose.yaml` + `.env` (+ any file the stack needs), one
//! btrfs subvolume per stack, owned by `docker-apps`. Every folder-based manager (Dockhand,
//! Dockge, Arcane, Komodo) works on the same files.
//!
//! [`StackFs::create_stack`] is the one entry point that makes a stack from given files; the
//! app catalogue renders an app into files and calls it.
//!
//! Compose runs as `docker compose -p <stack> --project-directory <dir> -f <dir>/<file>`
//! ([`compose_cmd`]); `down` never removes volumes.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rynas_api_types::docker::{
    BindCheck, StackFile, StackFiles, StackValidation, valid_stack_name,
};
use rynas_tasks::Cmd;
use serde_json::Value;

use crate::{Layout, Runner, safefs, under};

/// Compose file names, in compose's own order of preference.
pub const COMPOSE_FILES: &[&str] = &[
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];
/// The file a new stack gets.
pub const COMPOSE_FILE: &str = "compose.yaml";
pub const ENV_FILE: &str = ".env";
/// Largest compose or `.env` file accepted (the API body limit is 1 MiB anyway).
pub const MAX_FILE: usize = 512 << 10;
/// Most extra files of a new stack.
pub const MAX_EXTRA_FILES: usize = 64;

const BTRFS_SUPER_MAGIC: i64 = 0x9123_683E;

/// Why a stack operation was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackError {
    /// A request value is wrong: the field and an English message.
    Invalid(&'static str, String),
    AlreadyExists(String),
    NotFound(String),
    Failed(String),
}

impl std::fmt::Display for StackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StackError::Invalid(field, m) => write!(f, "{field}: {m}"),
            StackError::AlreadyExists(m) | StackError::NotFound(m) | StackError::Failed(m) => {
                f.write_str(m)
            }
        }
    }
}

/// Checks a stack name.
pub fn check_name(name: &str) -> Result<(), StackError> {
    if valid_stack_name(name) {
        Ok(())
    } else {
        Err(StackError::Invalid(
            "name",
            "1 to 63 characters among a-z, 0-9, - and _, starting with a letter or a digit".into(),
        ))
    }
}

/// Checks the path of an extra file: relative, no `..`, no empty or hidden-control parts, not
/// one of the files with their own field.
pub fn check_rel_path(p: &str) -> Result<(), String> {
    if p.is_empty() || p.len() > 255 || p.starts_with('/') || p.contains('\0') || p.contains('\\') {
        return Err(format!("invalid path {p:?}"));
    }
    for part in p.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.chars().any(char::is_control) {
            return Err(format!("invalid path {p:?}"));
        }
    }
    if COMPOSE_FILES.contains(&p) || p == ENV_FILE {
        return Err(format!("{p} has its own field"));
    }
    Ok(())
}

/// The compose file of a stack folder (a regular file: a symlink does not count).
pub fn find_compose(dir: &Path) -> Option<&'static str> {
    COMPOSE_FILES
        .iter()
        .copied()
        .find(|f| std::fs::symlink_metadata(dir.join(f)).is_ok_and(|m| m.is_file()))
}

/// Is `path` on btrfs?
pub fn is_btrfs(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: plain old data filled by the kernel
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid C string and buffer
    unsafe { libc::statfs(c.as_ptr(), &mut s) == 0 && s.f_type as i64 == BTRFS_SUPER_MAGIC }
}

/// Is `path` the root of a btrfs subvolume?
pub fn is_subvolume(path: &Path) -> bool {
    rynas_btrfs::ioctl::open_dir(path)
        .map(|f| rynas_btrfs::ioctl::is_subvolume(&f))
        .unwrap_or(false)
}

// ------------------------------------------------------------------ compose commands

/// The compose actions rynas runs (nothing else is ever passed to compose).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeAction {
    Up,
    Pull,
    Down,
    Restart,
    Stop,
    Start,
    /// `config --format json`: validation and the interpolated model.
    Config,
}

impl ComposeAction {
    pub fn as_str(self) -> &'static str {
        match self {
            ComposeAction::Up => "up",
            ComposeAction::Pull => "pull",
            ComposeAction::Down => "down",
            ComposeAction::Restart => "restart",
            ComposeAction::Stop => "stop",
            ComposeAction::Start => "start",
            ComposeAction::Config => "config",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "up" => Self::Up,
            "pull" => Self::Pull,
            "down" => Self::Down,
            "restart" => Self::Restart,
            "stop" => Self::Stop,
            "start" => Self::Start,
            _ => return None,
        })
    }

    /// Timeout of the command: image downloads can take long on a NAS line.
    pub fn timeout(self) -> Duration {
        match self {
            ComposeAction::Up | ComposeAction::Pull => Duration::from_secs(30 * 60),
            ComposeAction::Down
            | ComposeAction::Restart
            | ComposeAction::Stop
            | ComposeAction::Start => Duration::from_secs(10 * 60),
            ComposeAction::Config => Duration::from_secs(30),
        }
    }

    fn args(self) -> &'static [&'static str] {
        match self {
            ComposeAction::Up => &["up", "--detach", "--wait=false"],
            ComposeAction::Pull => &["pull", "--quiet"],
            // never --volumes: named volumes hold data
            ComposeAction::Down => &["down"],
            ComposeAction::Restart => &["restart"],
            ComposeAction::Stop => &["stop"],
            ComposeAction::Start => &["start"],
            ComposeAction::Config => &["config", "--format", "json"],
        }
    }
}

/// `docker compose -p <project> --project-directory <dir> -f <dir>/<file> <action…>`.
/// `env_file`: an `.env` in another place than the project directory (validation of files
/// not written yet); `None`: compose reads `<dir>/.env` itself.
pub fn compose_cmd(
    project: &str,
    dir: &str,
    compose_file: &str,
    env_file: Option<&str>,
    action: ComposeAction,
) -> Cmd {
    let file = if compose_file.starts_with('/') {
        compose_file.to_string()
    } else {
        format!("{dir}/{compose_file}")
    };
    let env: Vec<String> = env_file.map(str::to_string).into_iter().collect();
    compose_cmd_files(project, dir, &[file], &env, &[], action)
}

/// The same with several `-f` files (absolute), `--env-file` files and `--profile` names
/// (adopted stacks, §5.7).
pub fn compose_cmd_files(
    project: &str,
    dir: &str,
    files: &[String],
    env_files: &[String],
    profiles: &[String],
    action: ComposeAction,
) -> Cmd {
    let mut c = Cmd::new("docker").args(["compose", "-p", project, "--project-directory", dir]);
    for f in files {
        c = c.args(["-f", f]);
    }
    for e in env_files {
        c = c.args(["--env-file", e]);
    }
    for p in profiles {
        c = c.args(["--profile", p]);
    }
    c.args(action.args())
        .env_clear()
        // no colours, no progress bars in the task log
        .env("COMPOSE_ANSI", "never")
        .env("COMPOSE_PROGRESS", "plain")
        .env("COMPOSE_STATUS_STDOUT", "1")
        .timeout(action.timeout())
}

// ------------------------------------------------------------------ the compose model

/// A published port of a service.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Published {
    pub protocol: String,
    pub port: u16,
}

impl std::fmt::Display for Published {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.protocol, self.port)
    }
}

/// A bind mount of a service in the interpolated model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bind {
    pub service: String,
    pub source: String,
    pub target: String,
    /// compose creates the folder when missing (short syntax, or `create_host_path`).
    pub create: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Service {
    pub name: String,
    pub image: Option<String>,
    pub ports: Vec<Published>,
    pub binds: Vec<Bind>,
}

/// The parts of `docker compose config --format json` rynas reads.
pub fn parse_config(json: &str) -> Result<Vec<Service>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("compose config: {e}"))?;
    let services = v
        .get("services")
        .and_then(Value::as_object)
        .ok_or("compose config: no services")?;
    let mut out = Vec::new();
    for (name, s) in services {
        let mut svc = Service {
            name: name.clone(),
            image: s.get("image").and_then(Value::as_str).map(str::to_string),
            ..Default::default()
        };
        for p in s
            .get("ports")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let port = match p.get("published") {
                Some(Value::String(s)) => s.split('-').next().and_then(|x| x.parse().ok()),
                Some(Value::Number(n)) => n.as_u64().and_then(|n| u16::try_from(n).ok()),
                _ => None,
            };
            if let Some(port) = port {
                svc.ports.push(Published {
                    protocol: p
                        .get("protocol")
                        .and_then(Value::as_str)
                        .unwrap_or("tcp")
                        .to_string(),
                    port,
                });
            }
        }
        for m in s
            .get("volumes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if m.get("type").and_then(Value::as_str) != Some("bind") {
                continue;
            }
            let (Some(src), Some(dst)) = (
                m.get("source").and_then(Value::as_str),
                m.get("target").and_then(Value::as_str),
            ) else {
                continue;
            };
            let create = m
                .get("bind")
                .and_then(|b| b.get("create_host_path"))
                .and_then(Value::as_bool)
                .unwrap_or(true);
            svc.binds.push(Bind {
                service: name.clone(),
                source: src.to_string(),
                target: dst.to_string(),
                create,
            });
        }
        out.push(svc);
    }
    Ok(out)
}

/// Is `path` below the folder `dir` (or the folder itself)? Plain path comparison after
/// removing `.` parts; `..` counts as outside.
pub fn is_below(path: &str, dir: &str) -> bool {
    if path.split('/').any(|p| p == "..") {
        return false;
    }
    let dir = dir.trim_end_matches('/');
    path == dir || path.starts_with(&format!("{dir}/"))
}

/// What the UI shows about a compose model: services, images, ports (and the ones other
/// containers use: `used_ports`), binds (existence through `exists`, outside `volume`).
pub fn validation(
    services: &[Service],
    volume: &str,
    used_ports: &BTreeSet<String>,
    exists: &dyn Fn(&str) -> bool,
) -> StackValidation {
    let mut ports: BTreeSet<String> = BTreeSet::new();
    let mut images: BTreeSet<String> = BTreeSet::new();
    let mut binds = Vec::new();
    for s in services {
        if let Some(i) = &s.image {
            images.insert(i.clone());
        }
        for p in &s.ports {
            ports.insert(p.to_string());
        }
        for b in &s.binds {
            binds.push(BindCheck {
                service: b.service.clone(),
                source: b.source.clone(),
                target: b.target.clone(),
                exists: exists(&b.source),
                outside_volume: !is_below(&b.source, volume),
            });
        }
    }
    let port_conflicts = ports
        .iter()
        .filter(|p| used_ports.contains(*p))
        .cloned()
        .collect();
    StackValidation {
        ok: true,
        error: None,
        services: services.iter().map(|s| s.name.clone()).collect(),
        images: images.into_iter().collect(),
        ports: ports.into_iter().collect(),
        port_conflicts,
        binds,
    }
}

/// The last meaningful line of compose's error output.
pub fn compose_error(stderr: &str) -> String {
    stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("docker compose config failed")
        .to_string()
}

// ------------------------------------------------------------------ the folders

/// Mode and owner of the stack folder and its two files (review 2026-11-07, H1): root owns
/// them, so that no container uid can plant a symlink where rynasd writes as root. Compose
/// runs as root and reads them; containers never need to.
/// - the folder: `0750 root:docker-apps`;
/// - `compose.yaml`: `0640 root:docker-apps`;
/// - `.env` (generated secrets): `0600 root:root`.
///
/// Extra files of a new stack and bind folders belong to `docker-apps` (the apps' data).
pub const STACK_DIR_MODE: u32 = 0o750;
pub const COMPOSE_MODE: u32 = 0o640;
pub const ENV_MODE: u32 = 0o600;

/// Stack folders below `root` (`/` in production, the dry root under `--dry-system`).
pub struct StackFs<'a> {
    pub root: &'a Path,
    pub runner: &'a dyn Runner,
    /// Make btrfs subvolumes where the filesystem is btrfs (off under `--dry-system`).
    pub subvolumes: bool,
    /// `docker-apps` (uid, gid): owner of bind folders and extra files, group of the stack
    /// folder and its compose file; `None`: leave every owner as made (tests, dry system).
    pub owner: Option<(u32, u32)>,
}

impl StackFs<'_> {
    /// The real location of a system path.
    pub fn real(&self, abs: &str) -> PathBuf {
        under(self.root, abs)
    }

    /// `root:docker-apps` (folder, compose file).
    fn root_group(&self) -> safefs::Owner {
        self.owner.map(|(_, g)| (0, g))
    }

    /// `root:root` (`.env`).
    fn root_only(&self) -> safefs::Owner {
        self.owner.map(|_| (0, 0))
    }

    /// Opens a stack folder as a descriptor (no symlink anywhere below the root) and gives
    /// it the root owner and mode (stacks made before 2026-11-07 belonged to docker-apps).
    fn open_stack(&self, layout: &Layout, name: &str) -> Result<std::fs::File, String> {
        let abs = layout.stack(name);
        let d = safefs::open_dir(self.root, &abs).map_err(|e| format!("{abs}: {e}"))?;
        if self.owner.is_some() {
            safefs::set_owner_mode(&d, STACK_DIR_MODE, self.root_group())
                .map_err(|e| format!("{abs}: {e}"))?;
        }
        Ok(d)
    }

    /// Creates `abs` as a subvolume (on btrfs, when allowed) or a folder, unless it exists.
    /// `Ok(true)` when it was created.
    pub fn make_subvolume(&self, abs: &str, log: &dyn Fn(&str)) -> Result<bool, String> {
        let p = self.real(abs);
        if p.exists() {
            return Ok(false);
        }
        let parent = p.parent().ok_or("invalid path")?;
        if !parent.is_dir() {
            return Err(format!("{} does not exist", parent.display()));
        }
        if self.subvolumes && is_btrfs(parent) {
            let c = Cmd::new("btrfs")
                .args(["subvolume", "create"])
                .arg(&p)
                .env_clear()
                .timeout(Duration::from_secs(60));
            log(&format!("$ {}", c.display()));
            let o = self.runner.run(&c)?;
            if !o.success() {
                return Err(format!(
                    "btrfs subvolume create {abs}: {}",
                    compose_error(&o.stderr)
                ));
            }
        } else {
            log(&format!("mkdir {abs}"));
            std::fs::create_dir(&p).map_err(|e| format!("mkdir {abs}: {e}"))?;
        }
        Ok(true)
    }

    /// `/<vol>/docker`, `/<vol>/docker/stacks` and the snapshots folder.
    pub fn ensure_stacks_dir(&self, layout: &Layout, log: &dyn Fn(&str)) -> Result<(), String> {
        if !self.real(&layout.volume).is_dir() {
            return Err(format!("volume {} is not mounted", layout.volume));
        }
        self.make_subvolume(&layout.docker(), log)?;
        if self.make_subvolume(&layout.stacks(), log)? {
            let p = self.real(&layout.stacks());
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755));
        }
        let snaps = self.real(&layout.snapshots());
        if !snaps.exists() {
            std::fs::create_dir(&snaps).map_err(|e| format!("mkdir {}: {e}", snaps.display()))?;
            let _ = std::fs::set_permissions(&snaps, std::fs::Permissions::from_mode(0o700));
        }
        Ok(())
    }

    /// Writes `content` to `rel` below the open stack folder: descriptor-based, no symlink
    /// followed, owner and mode set before the rename ([`safefs::write_rel`]). New folders
    /// on the way belong to `docker-apps` (`0755`).
    fn write_file(
        &self,
        dir: &std::fs::File,
        rel: &str,
        content: &str,
        mode: u32,
        owner: safefs::Owner,
    ) -> Result<(), String> {
        safefs::write_rel(dir, rel, content.as_bytes(), mode, 0o755, owner)
            .map_err(|e| format!("{rel}: {e}"))
    }

    /// Creates a new stack from files: the subvolume (or folder) `/<vol>/docker/stacks/<name>`
    /// with `compose.yaml`, `.env` and `files` (modes and owners: [`STACK_DIR_MODE`]).
    /// Refused when the folder exists (`AlreadyExists`). Nothing is started.
    pub fn create_stack(
        &self,
        layout: &Layout,
        name: &str,
        compose: &str,
        env: &str,
        files: &[StackFile],
        log: &dyn Fn(&str),
    ) -> Result<String, StackError> {
        check_name(name)?;
        check_sizes(compose, env)?;
        if files.len() > MAX_EXTRA_FILES {
            return Err(StackError::Invalid(
                "files",
                format!("at most {MAX_EXTRA_FILES} files"),
            ));
        }
        for f in files {
            check_rel_path(&f.path).map_err(|m| StackError::Invalid("files", m))?;
            if f.content.len() > MAX_FILE {
                return Err(StackError::Invalid(
                    "files",
                    format!("{} is too large", f.path),
                ));
            }
            if f.mode.is_some_and(|m| m > 0o777) {
                return Err(StackError::Invalid(
                    "files",
                    format!("{}: invalid mode", f.path),
                ));
            }
        }
        let abs = layout.stack(name);
        if self.real(&abs).exists() {
            return Err(StackError::AlreadyExists(format!(
                "stack folder {abs} exists"
            )));
        }
        self.ensure_stacks_dir(layout, log)
            .map_err(StackError::Failed)?;
        self.make_subvolume(&abs, log).map_err(StackError::Failed)?;
        let write = || -> Result<(), String> {
            let dir = safefs::open_dir(self.root, &abs).map_err(|e| format!("{abs}: {e}"))?;
            safefs::set_owner_mode(&dir, STACK_DIR_MODE, self.root_group())
                .map_err(|e| format!("{abs}: {e}"))?;
            self.write_file(&dir, COMPOSE_FILE, compose, COMPOSE_MODE, self.root_group())?;
            self.write_file(&dir, ENV_FILE, env, ENV_MODE, self.root_only())?;
            for f in files {
                self.write_file(&dir, &f.path, &f.content, f.mode.unwrap_or(0o644), self.owner)?;
            }
            Ok(())
        };
        if let Err(e) = write() {
            log(&format!("writing the stack failed, removing {abs}: {e}"));
            let _ = self.remove_stack_dir(layout, name, log);
            return Err(StackError::Failed(e));
        }
        log(&format!("stack {name} created in {abs}"));
        Ok(abs)
    }

    /// Replaces the compose file and `.env` of an existing stack (same file name).
    pub fn write_files(
        &self,
        layout: &Layout,
        name: &str,
        compose: &str,
        env: &str,
    ) -> Result<(), StackError> {
        check_sizes(compose, env)?;
        let file = find_compose(&self.real(&layout.stack(name)))
            .ok_or_else(|| StackError::NotFound(format!("stack {name} has no compose file")))?;
        let dir = self.open_stack(layout, name).map_err(StackError::Failed)?;
        // modes no looser than rynas's (a stricter one the administrator chose stays)
        let mode = |f: &str, max: u32| safefs::mode_at(&dir, f).map_or(max, |m| m & max);
        self.write_file(&dir, file, compose, mode(file, COMPOSE_MODE), self.root_group())
            .and_then(|_| {
                self.write_file(&dir, ENV_FILE, env, mode(ENV_FILE, ENV_MODE), self.root_only())
            })
            .map_err(StackError::Failed)
    }

    /// The compose file and `.env` of a stack.
    pub fn read_files(&self, layout: &Layout, name: &str) -> Result<StackFiles, StackError> {
        let path = self.real(&layout.stack(name));
        let file = find_compose(&path)
            .ok_or_else(|| StackError::NotFound(format!("stack {name} has no compose file")))?;
        // no symlink followed: a planted `.env -> /etc/shadow` is never read as root
        let dir = safefs::open_dir(self.root, &layout.stack(name))
            .map_err(|e| StackError::Failed(format!("{}: {e}", layout.stack(name))))?;
        let read = |f: &str| safefs::read_at(&dir, f, MAX_FILE as u64);
        Ok(StackFiles {
            compose: read(file).map_err(|e| StackError::Failed(format!("{file}: {e}")))?,
            env: read(ENV_FILE).unwrap_or_default(),
            compose_file: file.to_string(),
            ..Default::default()
        })
    }

    /// Deletes a stack folder (a subvolume through the ioctl, else recursively) and its
    /// snapshots.
    pub fn remove_stack_dir(
        &self,
        layout: &Layout,
        name: &str,
        log: &dyn Fn(&str),
    ) -> Result<(), String> {
        check_name(name).map_err(|e| e.to_string())?;
        let snaps = self.real(&layout.stack_snapshots(name));
        if snaps.is_dir() {
            for e in std::fs::read_dir(&snaps)
                .map_err(|e| e.to_string())?
                .flatten()
            {
                let p = e.path();
                if p.is_dir() {
                    remove_tree(&p, log)?;
                } else {
                    let _ = std::fs::remove_file(&p);
                }
            }
            let _ = std::fs::remove_dir(&snaps);
        }
        let dir = self.real(&layout.stack(name));
        if dir.exists() {
            remove_tree(&dir, log)?;
        }
        Ok(())
    }

    /// Creates the missing bind folders below `volume` (owned by `docker-apps`), so that
    /// Docker does not make them as root. Folders outside the volume are never created, and
    /// a symlink on the way refuses the folder ([`create_below`]). Returns the folders
    /// created.
    pub fn create_bind_dirs(
        &self,
        binds: &[Bind],
        volume: &str,
        log: &dyn Fn(&str),
    ) -> Result<Vec<String>, String> {
        let mut made = Vec::new();
        let mut seen = BTreeSet::new();
        for b in binds {
            if !b.create || !is_below(&b.source, volume) || !seen.insert(b.source.clone()) {
                continue;
            }
            if create_below(self.root, volume, &b.source, 0o755, self.owner)?.is_empty() {
                continue;
            }
            log(&format!(
                "created bind folder {} (owner {})",
                b.source,
                crate::APPS_USER
            ));
            made.push(b.source.clone());
        }
        Ok(made)
    }
}

/// Creates the folder `abs` (and its missing parents) below the existing folder `base`, both
/// absolute paths below `root`: every component is opened with `O_NOFOLLOW` (a symlink on
/// the way is an error, never followed), each new folder gets `mode` and `owner` on its
/// descriptor. A folder that already exists (even through a symlink the administrator made)
/// is left alone. Returns the folders made, outermost first.
pub fn create_below(
    root: &Path,
    base: &str,
    abs: &str,
    mode: u32,
    owner: safefs::Owner,
) -> Result<Vec<String>, String> {
    if !is_below(abs, base) {
        return Err(format!("{abs} is not below {base}"));
    }
    if under(root, abs).is_dir() {
        return Ok(Vec::new());
    }
    let rest = abs[base.trim_end_matches('/').len()..].trim_start_matches('/');
    let mut cur = safefs::open_dir(root, base).map_err(|e| format!("{base}: {e}"))?;
    let mut path = base.trim_end_matches('/').to_string();
    let mut made = Vec::new();
    for c in safefs::components(rest).map_err(|e| e.to_string())? {
        path = format!("{path}/{c}");
        let (d, new) = safefs::ensure_dir_at(&cur, c, mode, owner)
            .map_err(|e| format!("mkdir {path}: {e}"))?;
        if new {
            made.push(path.clone());
        }
        cur = d;
    }
    Ok(made)
}

/// Deletes a subvolume with the ioctl, or a plain folder tree.
pub fn remove_tree(p: &Path, log: &dyn Fn(&str)) -> Result<(), String> {
    if is_subvolume(p) {
        let (Some(parent), Some(name)) = (p.parent(), p.file_name()) else {
            return Err("invalid path".into());
        };
        log(&format!("deleting subvolume {}", p.display()));
        let dir = rynas_btrfs::ioctl::open_dir(parent).map_err(|e| e.to_string())?;
        // read-only snapshots too: root may delete them
        match rynas_btrfs::ioctl::destroy_subvolume(&dir, &name.to_string_lossy()) {
            Ok(()) => return Ok(()),
            Err(e) => log(&format!(
                "subvolume delete failed ({e}), removing the files"
            )),
        }
    }
    log(&format!("removing {}", p.display()));
    std::fs::remove_dir_all(p).map_err(|e| format!("rm {}: {e}", p.display()))
}

fn check_sizes(compose: &str, env: &str) -> Result<(), StackError> {
    if compose.trim().is_empty() {
        return Err(StackError::Invalid(
            "compose",
            "the compose file is empty".into(),
        ));
    }
    if compose.len() > MAX_FILE {
        return Err(StackError::Invalid(
            "compose",
            "the compose file is too large".into(),
        ));
    }
    if env.len() > MAX_FILE {
        return Err(StackError::Invalid(
            "env",
            "the .env file is too large".into(),
        ));
    }
    if compose.contains('\0') || env.contains('\0') {
        return Err(StackError::Invalid("compose", "binary content".into()));
    }
    Ok(())
}

/// Published host ports in use by other projects (`tcp/8080`), from containers' ports.
pub fn used_ports<'a>(
    ports: impl IntoIterator<Item = (&'a Option<String>, &'a rynas_api_types::docker::PortMapping)>,
    except_stack: Option<&str>,
) -> BTreeSet<String> {
    ports
        .into_iter()
        .filter(|(stack, _)| except_stack.is_none() || stack.as_deref() != except_stack)
        .filter_map(|(_, p)| Some(format!("{}/{}", p.protocol, p.public_port?)))
        .collect()
}

/// Groups bind sources by service (for logs).
pub fn binds_by_service(services: &[Service]) -> BTreeMap<String, Vec<String>> {
    services
        .iter()
        .map(|s| {
            (
                s.name.clone(),
                s.binds.iter().map(|b| b.source.clone()).collect(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::tests::Fake;
    use std::sync::Mutex;

    #[test]
    fn rel_paths() {
        for ok in ["config/app.ini", "a", "secrets/db.key"] {
            assert!(check_rel_path(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../b",
            "a//b",
            "./a",
            "compose.yaml",
            ".env",
            "a\\b",
        ] {
            assert!(check_rel_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn compose_lines() {
        let c = compose_cmd(
            "immich",
            "/vol1/docker/stacks/immich",
            "compose.yaml",
            None,
            ComposeAction::Up,
        );
        assert_eq!(
            c.display(),
            "docker compose -p immich --project-directory /vol1/docker/stacks/immich -f /vol1/docker/stacks/immich/compose.yaml up --detach --wait=false"
        );
        let c = compose_cmd("x", "/d", "docker-compose.yml", None, ComposeAction::Down);
        assert!(
            c.display().ends_with("-f /d/docker-compose.yml down"),
            "{}",
            c.display()
        );
        assert!(!c.display().contains("-v") && !c.display().contains("--volumes"));
        let c = compose_cmd(
            "x",
            "/d",
            "/tmp/v/compose.yaml",
            Some("/tmp/v/.env"),
            ComposeAction::Config,
        );
        assert!(
            c.display()
                .contains("-f /tmp/v/compose.yaml --env-file /tmp/v/.env config --format json")
        );
        assert_eq!(ComposeAction::parse("pull"), Some(ComposeAction::Pull));
        assert_eq!(ComposeAction::parse("exec"), None);
    }

    const CONFIG: &str = r#"{
      "name": "immich",
      "services": {
        "server": {
          "image": "ghcr.io/immich-app/immich-server:v2.1.0",
          "ports": [{"mode": "ingress", "target": 2283, "published": "2283", "protocol": "tcp"}],
          "volumes": [
            {"type": "bind", "source": "/vol1/docker/stacks/immich/library", "target": "/data", "bind": {"create_host_path": true}},
            {"type": "bind", "source": "/etc/localtime", "target": "/etc/localtime", "read_only": true, "bind": {}},
            {"type": "volume", "source": "model-cache", "target": "/cache"}
          ]
        },
        "db": {"image": "postgres:16", "volumes": [{"type": "bind", "source": "/vol1/photos", "target": "/x", "bind": {"create_host_path": false}}]},
        "udp": {"image": "x", "ports": [{"target": 53, "published": 5353, "protocol": "udp"}]}
      }
    }"#;

    #[test]
    fn model_and_validation() {
        let s = parse_config(CONFIG).unwrap();
        assert_eq!(s.len(), 3);
        let server = s.iter().find(|x| x.name == "server").unwrap();
        assert_eq!(server.binds.len(), 2);
        assert_eq!(
            server.ports,
            vec![Published {
                protocol: "tcp".into(),
                port: 2283
            }]
        );
        let used: BTreeSet<String> = ["udp/5353".to_string()].into();
        let v = validation(&s, "/vol1", &used, &|p| p == "/etc/localtime");
        assert_eq!(v.services, ["db", "server", "udp"]);
        assert_eq!(v.ports, ["tcp/2283", "udp/5353"]);
        assert_eq!(v.port_conflicts, ["udp/5353"]);
        let lt = v
            .binds
            .iter()
            .find(|b| b.source == "/etc/localtime")
            .unwrap();
        assert!(lt.exists && lt.outside_volume);
        let lib = v.binds.iter().find(|b| b.target == "/data").unwrap();
        assert!(!lib.exists && !lib.outside_volume);
        assert!(parse_config("{}").is_err());
        assert!(
            is_below("/vol1/a", "/vol1")
                && !is_below("/vol10/a", "/vol1")
                && !is_below("/vol1/../etc", "/vol1")
        );
    }

    fn fs<'a>(root: &'a Path, fake: &'a Fake) -> StackFs<'a> {
        StackFs {
            root,
            runner: fake,
            subvolumes: false,
            owner: None,
        }
    }

    #[test]
    fn create_read_write_remove() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("vol1")).unwrap();
        let fake = Fake {
            rules: vec![],
            calls: Mutex::new(vec![]),
        };
        let s = fs(d.path(), &fake);
        let l = Layout::of_label("vol1");
        let files = vec![StackFile {
            path: "config/app.ini".into(),
            content: "a=1\n".into(),
            mode: Some(0o600),
        }];
        let p = s
            .create_stack(
                &l,
                "app",
                "services:\n  a:\n    image: x\n",
                "PUID=1000\n",
                &files,
                &|_| {},
            )
            .unwrap();
        assert_eq!(p, "/vol1/docker/stacks/app");
        let dir = d.path().join("vol1/docker/stacks/app");
        let mode = |f: &str| std::fs::metadata(dir.join(f)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(".env"), 0o600);
        assert_eq!(mode("config/app.ini"), 0o600);
        assert!(d.path().join("vol1/docker/.snapshots").is_dir());
        assert_eq!(
            s.create_stack(&l, "app", "x: 1\n", "", &[], &|_| {}),
            Err(StackError::AlreadyExists(
                "stack folder /vol1/docker/stacks/app exists".into()
            ))
        );
        let f = s.read_files(&l, "app").unwrap();
        assert_eq!(
            (f.compose_file.as_str(), f.env.as_str()),
            ("compose.yaml", "PUID=1000\n")
        );
        s.write_files(&l, "app", "services: {}\n", "A=2\n").unwrap();
        assert_eq!(s.read_files(&l, "app").unwrap().env, "A=2\n");
        assert_eq!(mode(".env"), 0o600);
        assert!(matches!(
            s.create_stack(&l, "Bad", "x", "", &[], &|_| {}),
            Err(StackError::Invalid("name", _))
        ));
        assert!(matches!(
            s.create_stack(&l, "b", " ", "", &[], &|_| {}),
            Err(StackError::Invalid("compose", _))
        ));
        assert!(matches!(
            s.create_stack(
                &l,
                "c",
                "x",
                "",
                &[StackFile {
                    path: "../x".into(),
                    ..Default::default()
                }],
                &|_| {}
            ),
            Err(StackError::Invalid("files", _))
        ));
        s.remove_stack_dir(&l, "app", &|_| {}).unwrap();
        assert!(!dir.exists());
        // an unmounted volume
        let l2 = Layout::of_label("vol9");
        assert!(matches!(
            s.create_stack(&l2, "x", "a: 1", "", &[], &|_| {}),
            Err(StackError::Failed(_))
        ));
    }

    #[test]
    fn bind_folders_inside_the_volume_only() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("vol1")).unwrap();
        let fake = Fake {
            rules: vec![],
            calls: Mutex::new(vec![]),
        };
        let s = fs(d.path(), &fake);
        let b = |src: &str, create: bool| Bind {
            service: "a".into(),
            source: src.into(),
            target: "/t".into(),
            create,
        };
        let made = s
            .create_bind_dirs(
                &[
                    b("/vol1/docker/stacks/a/data/db", true),
                    b("/etc/x", true),
                    b("/vol1/nocreate", false),
                    b("/vol1/docker/stacks/a/data/db", true),
                ],
                "/vol1",
                &|_| {},
            )
            .unwrap();
        assert_eq!(made, ["/vol1/docker/stacks/a/data/db"]);
        assert!(d.path().join("vol1/docker/stacks/a/data/db").is_dir());
        assert!(!d.path().join("etc/x").exists() && !d.path().join("vol1/nocreate").exists());
    }

    /// Review 2026-11-07, H1: a container uid plants symlinks in a stack folder; rynasd's
    /// root writes and bind folders must refuse them and never touch the targets.
    #[test]
    fn planted_symlinks_are_never_followed() {
        use std::os::unix::fs::symlink;
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("vol1")).unwrap();
        let victim = d.path().join("shadow");
        std::fs::write(&victim, "root:secret\n").unwrap();
        let elsewhere = d.path().join("etc");
        std::fs::create_dir(&elsewhere).unwrap();
        let fake = Fake {
            rules: vec![],
            calls: Mutex::new(vec![]),
        };
        let s = fs(d.path(), &fake);
        let l = Layout::of_label("vol1");
        s.create_stack(&l, "app", "services: {}\n", "A=1\n", &[], &|_| {})
            .unwrap();
        let dir = d.path().join("vol1/docker/stacks/app");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), STACK_DIR_MODE);
        assert_eq!(mode(&dir.join("compose.yaml")), COMPOSE_MODE);
        assert_eq!(mode(&dir.join(".env")), ENV_MODE);
        // the temporary names point at the victim
        symlink(&victim, dir.join("..env.rynas-tmp")).unwrap();
        symlink(&victim, dir.join(".compose.yaml.rynas-tmp")).unwrap();
        s.write_files(&l, "app", "services: {x: {}}\n", "A=2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "root:secret\n");
        assert_eq!(std::fs::read_to_string(dir.join(".env")).unwrap(), "A=2\n");
        // `.env` itself replaced by a link: never read as root, replaced on the next write
        std::fs::remove_file(dir.join(".env")).unwrap();
        symlink(&victim, dir.join(".env")).unwrap();
        assert_eq!(s.read_files(&l, "app").unwrap().env, "");
        s.write_files(&l, "app", "services: {}\n", "A=3\n").unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "root:secret\n");
        assert!(!std::fs::symlink_metadata(dir.join(".env")).unwrap().is_symlink());
        // a bind subfolder that is a link: refused, nothing made at the target
        symlink(&elsewhere, dir.join("config")).unwrap();
        let b = Bind {
            service: "a".into(),
            source: "/vol1/docker/stacks/app/config/sub".into(),
            target: "/config".into(),
            create: true,
        };
        assert!(s.create_bind_dirs(&[b], "/vol1", &|_| {}).is_err());
        assert!(!elsewhere.join("sub").exists());
        // a whole stack folder replaced by a link: writes refused
        std::fs::create_dir(d.path().join("vol1/docker/stacks/evil-target")).unwrap();
        symlink(&elsewhere, d.path().join("vol1/docker/stacks/evil")).unwrap();
        std::fs::write(elsewhere.join("compose.yaml"), "services: {}\n").unwrap();
        assert!(s.write_files(&l, "evil", "services: {}\n", "X=1\n").is_err());
        assert!(!elsewhere.join(".env").exists());
    }

    #[test]
    fn subvolume_command_when_btrfs() {
        // only the command line: the test folder may not be btrfs
        let d = tempfile::tempdir().unwrap();
        let fake = Fake {
            rules: vec![("btrfs subvolume create", 0, "")],
            calls: Mutex::new(vec![]),
        };
        let s = StackFs {
            root: d.path(),
            runner: &fake,
            subvolumes: true,
            owner: None,
        };
        let r = s.make_subvolume("/x", &|_| {});
        if is_btrfs(d.path()) {
            // the fake did not create it: reported as created anyway
            assert_eq!(r, Ok(true));
            assert!(fake.calls.lock().unwrap()[0].starts_with("btrfs subvolume create "));
        } else {
            assert_eq!(r, Ok(true));
            assert!(d.path().join("x").is_dir());
        }
    }
}
