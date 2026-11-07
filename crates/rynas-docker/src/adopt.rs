//! Adopted stacks (`docs/phase2/docker-apps.md` §5.7): compose projects rynas manages where
//! they already are. An adopted stack is a record in rynasd's state (`adopted.json`): the
//! project name, its directory, its `-f` files, `--env-file` files and profiles, and a hash
//! of each file. Nothing in the project folder is moved or rewritten by adopting; "Release"
//! deletes the record and nothing else.
//!
//! - Candidates come from the containers' compose labels (`working_dir`, `config_files`,
//!   `environment_file`) or from a folder (import): its compose file plus the override
//!   compose itself would merge.
//! - Portainer's compose data is adopted read-only: Portainer still owns those files.
//! - Writes keep the file's owner and mode, are atomic (temporary file + rename, no symlink
//!   followed) and refused when the files changed on disk since the editor read them
//!   ([`files_hash`]).
//! - "Move to the rynas stacks folder" copies the whole folder (data included) and checks
//!   that every bind mount still resolves to the same place ([`check_moved_binds`]).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rynas_api_types::docker::{AdoptWarning, AdoptedInfo, ManagerKind, valid_stack_name};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::stacks::{Bind, COMPOSE_FILES, ENV_FILE, MAX_FILE, is_below};
use crate::{safefs, under};

/// The record file below rynasd's Docker state folder.
pub const REGISTRY_FILE: &str = "adopted.json";
/// Override files compose merges by itself next to the main file.
pub const OVERRIDE_FILES: &[&str] = &[
    "compose.override.yaml",
    "compose.override.yml",
    "docker-compose.override.yaml",
    "docker-compose.override.yml",
];
/// Most `-f` / `--env-file` / profile entries of one stack.
pub const MAX_ENTRIES: usize = 16;

/// Folders never adopted or imported (system trees, and `/` itself).
const SYSTEM_DIRS: &[&str] = &[
    "/bin", "/boot", "/dev", "/etc", "/lib", "/lib64", "/proc", "/run", "/sbin", "/sys", "/usr",
    "/var/lib/rynas",
];

/// One adopted stack as recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Adopted {
    pub dir: String,
    pub files: Vec<String>,
    #[serde(default)]
    pub env_files: Vec<String>,
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub origin: Option<ManagerKind>,
    #[serde(default)]
    pub read_only: bool,
    pub adopted_ms: u64,
    /// The [`files_hash`] when adopted or last written by rynas.
    #[serde(default)]
    pub hash: String,
}

impl Adopted {
    /// The API view; `current`: the files' hash now (`None`: unreadable, not reported).
    pub fn info(&self, current: Option<&str>) -> AdoptedInfo {
        AdoptedInfo {
            dir: self.dir.clone(),
            files: self.files.clone(),
            env_files: self.env_files.clone(),
            profiles: self.profiles.clone(),
            origin: self.origin,
            read_only: self.read_only,
            adopted_ms: self.adopted_ms,
            changed_outside: current.is_some_and(|h| !self.hash.is_empty() && h != self.hash),
        }
    }

    /// The env file the editor shows: the first `--env-file`, else `<dir>/.env`.
    pub fn env_path(&self) -> String {
        self.env_files
            .first()
            .cloned()
            .unwrap_or_else(|| format!("{}/{ENV_FILE}", self.dir.trim_end_matches('/')))
    }

    /// Can the editor write the main compose file and the env file? Not read-only, both
    /// below the project folder.
    pub fn writable(&self) -> bool {
        !self.read_only
            && self.files.first().is_some_and(|f| is_below(f, &self.dir))
            && is_below(&self.env_path(), &self.dir)
    }
}

/// The records, by stack name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Registry {
    #[serde(default)]
    pub stacks: BTreeMap<String, Adopted>,
}

impl Registry {
    /// Reads `<state>/adopted.json` (missing or unreadable: empty, logged by the caller).
    pub fn load(state_dir: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(state_dir.join(REGISTRY_FILE)) {
            Ok(t) => serde_json::from_str(&t).map_err(|e| format!("{REGISTRY_FILE}: {e}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{REGISTRY_FILE}: {e}")),
        }
    }

    /// Writes it atomically (`0600`).
    pub fn save(&self, state_dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(state_dir).map_err(|e| e.to_string())?;
        let dir = safefs::open_start(state_dir).map_err(|e| e.to_string())?;
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        safefs::write_at(&dir, REGISTRY_FILE, text.as_bytes(), 0o600, None)
            .map_err(|e| format!("{REGISTRY_FILE}: {e}"))
    }
}

/// Checks an absolute path given by the API: absolute, no `..` or `.` parts, no control
/// characters, at most 1024 bytes.
pub fn check_abs(p: &str) -> Result<(), String> {
    if !p.starts_with('/') || p.len() > 1024 || p.contains('\0') || p.contains('\\') {
        return Err(format!("{p:?} is not an absolute path"));
    }
    let rest = p.trim_end_matches('/');
    if rest.is_empty() {
        return Err("/ cannot be a stack folder".into());
    }
    for part in rest[1..].split('/') {
        if part.is_empty() || part == "." || part == ".." || part.chars().any(char::is_control) {
            return Err(format!("{p:?} is not a plain absolute path"));
        }
    }
    Ok(())
}

/// Checks a project folder: [`check_abs`] and not a system tree.
pub fn check_dir(p: &str) -> Result<(), String> {
    check_abs(p)?;
    if SYSTEM_DIRS.iter().any(|s| is_below(p, s)) {
        return Err(format!("{p} is a system folder"));
    }
    Ok(())
}

/// `p` without a trailing slash.
pub fn clean(p: &str) -> String {
    let t = p.trim_end_matches('/');
    if t.is_empty() { "/".into() } else { t.to_string() }
}

/// `config_files` of the labels as absolute paths (relative entries resolve against the
/// working directory, as compose saw them).
pub fn resolve_files(working_dir: &str, files: &[String]) -> Vec<String> {
    files
        .iter()
        .map(|f| f.trim())
        .filter(|f| !f.is_empty())
        .map(|f| {
            if f.starts_with('/') {
                f.to_string()
            } else {
                format!("{}/{}", working_dir.trim_end_matches('/'), f.trim_start_matches("./"))
            }
        })
        .collect()
}

/// The compose files compose would use in `dir` without `-f`: the first of
/// [`COMPOSE_FILES`] and its override (absolute paths). `exists`: is it a regular file.
pub fn scan_folder(dir: &str, exists: &dyn Fn(&str) -> bool) -> Vec<String> {
    let d = dir.trim_end_matches('/');
    let mut out = Vec::new();
    if let Some(f) = COMPOSE_FILES
        .iter()
        .map(|f| format!("{d}/{f}"))
        .find(|p| exists(p))
    {
        out.push(f);
        if let Some(o) = OVERRIDE_FILES
            .iter()
            .map(|f| format!("{d}/{f}"))
            .find(|p| exists(p))
        {
            out.push(o);
        }
    }
    out
}

/// The manager whose folder this looks like (paths only).
pub fn origin_of(dir: &str, files: &[String]) -> Option<ManagerKind> {
    let all: Vec<&str> = std::iter::once(dir).chain(files.iter().map(String::as_str)).collect();
    let any = |f: &dyn Fn(&str) -> bool| all.iter().any(|p| f(p));
    if any(&|p| {
        p.starts_with("/data/compose/") || (p.contains("portainer") && p.contains("/compose/"))
    }) {
        Some(ManagerKind::Portainer)
    } else if any(&|p| p.contains("dockhand")) {
        Some(ManagerKind::Dockhand)
    } else if any(&|p| p.contains("dockge") || p.starts_with("/opt/stacks/")) {
        Some(ManagerKind::Dockge)
    } else if any(&|p| p.contains("komodo")) {
        Some(ManagerKind::Komodo)
    } else if any(&|p| p.contains("arcane")) {
        Some(ManagerKind::Arcane)
    } else {
        None
    }
}

/// The host folder of a Portainer compose folder `/data/compose/<id>` (a path inside
/// Portainer's container): `<mountpoint>/compose/<id>` of a volume whose name has
/// `portainer` in it, when that folder exists.
pub fn portainer_host_dir(
    working_dir: &str,
    volumes: &[(String, String)],
    exists: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let rest = working_dir.strip_prefix("/data/")?;
    volumes
        .iter()
        .filter(|(n, _)| n.contains("portainer"))
        .map(|(_, m)| format!("{}/{rest}", m.trim_end_matches('/')))
        .find(|p| exists(p))
}

/// Rebases Portainer's in-container paths (`/data/...`) onto its host folder.
pub fn rebase(paths: &[String], from: &str, to: &str) -> Vec<String> {
    paths
        .iter()
        .map(|p| match p.strip_prefix(from) {
            Some(r) if r.is_empty() || r.starts_with('/') => format!("{to}{r}"),
            _ => p.clone(),
        })
        .collect()
}

/// A stack name from a folder name: lowercase, invalid characters dropped.
pub fn name_from_dir(dir: &str) -> Option<String> {
    let base = dir.trim_end_matches('/').rsplit('/').next()?;
    let n: String = base
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let n = n.trim_start_matches(['-', '_']).chars().take(63).collect::<String>();
    valid_stack_name(&n).then_some(n)
}

/// What a line scan of compose files tells (no YAML parser: the keys rynas warns about).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileFacts {
    /// The last top-level `name:` (compose: the last file wins).
    pub name: Option<String>,
    pub include: bool,
    pub profiles: BTreeSet<String>,
    pub external_networks: BTreeSet<String>,
}

fn unquote(v: &str) -> String {
    let v = v.split(" #").next().unwrap_or("").trim();
    v.trim_matches(['"', '\'']).to_string()
}

/// Scans the texts of the compose files, in `-f` order.
pub fn scan_texts(texts: &[String]) -> FileFacts {
    let mut f = FileFacts::default();
    for t in texts {
        let mut section = "";
        let mut net: Option<String> = None;
        let mut in_profiles: Option<usize> = None;
        for line in t.lines() {
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let indent = line.len() - trimmed.len();
            if indent == 0 {
                in_profiles = None;
                net = None;
                if let Some(v) = trimmed.strip_prefix("name:") {
                    let v = unquote(v);
                    if !v.is_empty() {
                        f.name = Some(v);
                    }
                }
                if trimmed.starts_with("include:") {
                    f.include = true;
                }
                section = trimmed.split(':').next().unwrap_or("");
                continue;
            }
            if let Some(pi) = in_profiles {
                if indent > pi && trimmed.starts_with("- ") {
                    let v = unquote(&trimmed[2..]);
                    if !v.is_empty() {
                        f.profiles.insert(v);
                    }
                    continue;
                }
                in_profiles = None;
            }
            if let Some(v) = trimmed.strip_prefix("profiles:") {
                let v = v.trim();
                if let Some(list) = v.strip_prefix('[') {
                    for p in list.trim_end_matches(']').split(',') {
                        let p = unquote(p);
                        if !p.is_empty() {
                            f.profiles.insert(p);
                        }
                    }
                } else {
                    in_profiles = Some(indent);
                }
                continue;
            }
            if section == "networks" {
                if indent == 2 && trimmed.ends_with(':') {
                    net = Some(trimmed.trim_end_matches(':').trim().to_string());
                } else if let Some(n) = &net
                    && trimmed.replace(' ', "") == "external:true"
                {
                    f.external_networks.insert(n.clone());
                }
            }
        }
    }
    f
}

/// Reads a regular file at an absolute path below `root` (no symlink on the way), at most
/// [`MAX_FILE`] bytes.
pub fn read_abs(root: &Path, abs: &str) -> Result<String, String> {
    let (parent, name) = split(abs)?;
    let d = safefs::open_dir(root, parent).map_err(|e| format!("{parent}: {e}"))?;
    safefs::read_at(&d, name, MAX_FILE as u64).map_err(|e| format!("{abs}: {e}"))
}

/// Is `abs` a regular file below `root` (no symlink on the way)?
pub fn is_file(root: &Path, abs: &str) -> bool {
    let Ok((parent, name)) = split(abs) else {
        return false;
    };
    safefs::open_dir(root, parent)
        .and_then(|d| safefs::open_file_at(&d, name))
        .is_ok()
}

fn split(abs: &str) -> Result<(&str, &str), String> {
    check_abs(abs)?;
    let (p, n) = abs.trim_end_matches('/').rsplit_once('/').ok_or("invalid path")?;
    Ok((if p.is_empty() { "/" } else { p }, n))
}

/// The hash of a stack's files: every compose file, then every env file (missing files
/// count as empty), by path and content.
pub fn files_hash(root: &Path, files: &[String], env_files: &[String]) -> String {
    let mut h = Sha256::new();
    for f in files.iter().chain(env_files) {
        h.update(f.as_bytes());
        h.update([0]);
        h.update(read_abs(root, f).unwrap_or_default().as_bytes());
        h.update([0]);
    }
    hex(&h.finalize())
}

/// The hash the editor sees: compose file name, compose text, env text.
pub fn editor_hash(compose_file: &str, compose: &str, env: &str) -> String {
    let mut h = Sha256::new();
    for p in [compose_file, compose, env] {
        h.update(p.as_bytes());
        h.update([0]);
    }
    hex(&h.finalize())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Writes `content` to the existing or new file `abs` below `root`: atomic, no symlink
/// followed, the old file's owner and mode kept (a new file: `mode`, root).
pub fn write_abs(root: &Path, abs: &str, content: &str, mode: u32) -> Result<(), String> {
    let (parent, name) = split(abs)?;
    let d = safefs::open_dir(root, parent).map_err(|e| format!("{parent}: {e}"))?;
    let (m, owner) = match safefs::open_file_at(&d, name).and_then(|f| safefs::stat_of(&f)) {
        Ok((m, u, g)) => (m, Some((u, g))),
        Err(_) => (mode, None),
    };
    safefs::write_at(&d, name, content.as_bytes(), m, owner).map_err(|e| format!("{abs}: {e}"))
}

/// The warnings of an adoption from what is known about it.
pub struct WarnInput<'a> {
    pub name: &'a str,
    pub dir: &'a str,
    pub files: &'a [String],
    pub env_files: &'a [String],
    pub facts: &'a FileFacts,
    pub origin: Option<ManagerKind>,
    /// Containers of this project name run from another folder.
    pub name_in_use: bool,
    pub missing: bool,
}

pub fn warnings(w: &WarnInput) -> Vec<AdoptWarning> {
    let mut out = Vec::new();
    match w.origin {
        Some(ManagerKind::Portainer) => out.push(AdoptWarning::Portainer),
        Some(_) => out.push(AdoptWarning::OtherManager),
        None => {}
    }
    if w.missing {
        out.push(AdoptWarning::MissingFile);
    }
    if w.facts.name.as_deref().is_some_and(|n| n != w.name) {
        out.push(AdoptWarning::NameDiffers);
    }
    if w.name_in_use {
        out.push(AdoptWarning::NameInUse);
    }
    if !w.facts.external_networks.is_empty() {
        out.push(AdoptWarning::ExternalNetworks);
    }
    if !w.facts.profiles.is_empty() {
        out.push(AdoptWarning::Profiles);
    }
    if w.facts.include {
        out.push(AdoptWarning::Include);
    }
    let env = w
        .env_files
        .first()
        .cloned()
        .unwrap_or_else(|| format!("{}/{ENV_FILE}", w.dir.trim_end_matches('/')));
    if !w.files.first().is_some_and(|f| is_below(f, w.dir)) || !is_below(&env, w.dir) {
        out.push(AdoptWarning::OutsideDir);
    }
    out
}

/// Checks a request's list of compose or env files: at most [`MAX_ENTRIES`], each
/// [`check_abs`] and not in a system tree (like [`check_dir`]: `/etc/shadow` is never read
/// as an env file).
pub fn check_paths(field: &'static str, v: &[String]) -> Result<(), (&'static str, String)> {
    if v.len() > MAX_ENTRIES {
        return Err((field, format!("at most {MAX_ENTRIES} entries")));
    }
    for p in v {
        check_abs(p).map_err(|m| (field, m))?;
        if SYSTEM_DIRS.iter().any(|s| is_below(p, s)) {
            return Err((field, format!("{p} is in a system folder")));
        }
    }
    Ok(())
}

/// Checks profile names (compose: `[a-zA-Z0-9][a-zA-Z0-9_.-]*`).
pub fn check_profiles(v: &[String]) -> Result<(), String> {
    if v.len() > MAX_ENTRIES {
        return Err(format!("at most {MAX_ENTRIES} profiles"));
    }
    for p in v {
        let ok = p.len() <= 64
            && p.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
            && p.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c));
        if !ok {
            return Err(format!("invalid profile {p:?}"));
        }
    }
    Ok(())
}

/// After a folder copy `old_dir` → `new_dir`: every bind of the new model resolves to the
/// same host path as before, or to the copied place for sources inside the old folder.
/// Returns the binds that would point elsewhere (`service: old → new`).
pub fn check_moved_binds(old: &[Bind], new: &[Bind], old_dir: &str, new_dir: &str) -> Vec<String> {
    let mut bad = Vec::new();
    for o in old {
        let expect = if is_below(&o.source, old_dir) {
            format!("{}{}", clean(new_dir), &o.source[clean(old_dir).len()..])
        } else {
            o.source.clone()
        };
        let got = new
            .iter()
            .find(|n| n.service == o.service && n.target == o.target)
            .map(|n| n.source.clone());
        if got.as_deref() != Some(expect.as_str()) {
            bad.push(format!(
                "{}: {} → {}",
                o.service,
                o.source,
                got.unwrap_or_else(|| "(missing)".into())
            ));
        }
    }
    bad
}

/// The record after a move into `new_dir`: paths below the old folder rebased.
pub fn moved(a: &Adopted, new_dir: &str) -> Adopted {
    let old = clean(&a.dir);
    let re = |v: &[String]| rebase(v, &old, &clean(new_dir));
    Adopted {
        dir: clean(new_dir),
        files: re(&a.files),
        env_files: re(&a.env_files),
        read_only: false,
        origin: None,
        ..a.clone()
    }
}

/// After a move: is the stack a plain rynas stack (one standard compose file at the folder
/// root, the default `.env`, no profiles)? Then the record is dropped.
pub fn is_plain(a: &Adopted) -> bool {
    let d = clean(&a.dir);
    a.profiles.is_empty()
        && (a.env_files.is_empty() || a.env_files == [format!("{d}/{ENV_FILE}")])
        && a.files.len() == 1
        && COMPOSE_FILES
            .iter()
            .any(|f| a.files[0] == format!("{d}/{f}"))
}

/// Does the folder exist below `root`?
pub fn dir_exists(root: &Path, abs: &str) -> bool {
    check_abs(abs).is_ok() && safefs::open_dir(root, abs).is_ok() && under(root, abs).is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_names() {
        assert!(check_abs("/volume1/docker/app").is_ok());
        for bad in ["", "rel", "/", "/a/../b", "/a/./b", "/a//b", "/a\\b"] {
            assert!(check_abs(bad).is_err(), "{bad}");
        }
        assert!(check_dir("/etc/compose").is_err() && check_dir("/srv/compose/x").is_ok());
        assert!(check_paths("env_files", &["/srv/app/.env".into()]).is_ok());
        for bad in ["/etc/shadow", "/var/lib/rynas/secret", "/proc/self/environ"] {
            assert_eq!(check_paths("env_files", &[bad.into()]).unwrap_err().0, "env_files", "{bad}");
        }
        assert_eq!(name_from_dir("/volume1/docker/Home Assistant/"), Some("homeassistant".into()));
        assert_eq!(name_from_dir("/x/_web-1"), Some("web-1".into()));
        assert_eq!(name_from_dir("/x/___"), None);
        assert_eq!(
            resolve_files("/srv/app", &["compose.yaml".into(), "/abs/o.yml".into(), "./x.yml".into()]),
            ["/srv/app/compose.yaml", "/abs/o.yml", "/srv/app/x.yml"]
        );
        assert!(check_profiles(&["debug".into(), "a.b-c_1".into()]).is_ok());
        assert!(check_profiles(&["-x".into()]).is_err());
    }

    #[test]
    fn folders_and_origins() {
        let have = ["/s/app/docker-compose.yml", "/s/app/docker-compose.override.yml"];
        let ex = |p: &str| have.contains(&p);
        assert_eq!(scan_folder("/s/app/", &ex), have);
        assert!(scan_folder("/s/none", &ex).is_empty());
        assert_eq!(origin_of("/data/compose/3", &[]), Some(ManagerKind::Portainer));
        assert_eq!(origin_of("/opt/stacks/kuma", &[]), Some(ManagerKind::Dockge));
        assert_eq!(origin_of("/volume1/docker/dockhand/stacks/x", &[]), Some(ManagerKind::Dockhand));
        assert_eq!(origin_of("/volume1/docker/app", &[]), None);
        let vols = vec![
            ("media_cache".to_string(), "/v/volumes/media_cache/_data".to_string()),
            ("portainer_data".to_string(), "/v/volumes/portainer_data/_data".to_string()),
        ];
        let ex2 = |p: &str| p == "/v/volumes/portainer_data/_data/compose/3";
        assert_eq!(
            portainer_host_dir("/data/compose/3", &vols, &ex2).as_deref(),
            Some("/v/volumes/portainer_data/_data/compose/3")
        );
        assert_eq!(portainer_host_dir("/data/compose/4", &vols, &ex2), None);
        assert_eq!(
            rebase(&["/data/compose/3/docker-compose.yml".into(), "/other".into()], "/data/compose/3", "/h/3"),
            ["/h/3/docker-compose.yml", "/other"]
        );
    }

    #[test]
    fn line_scan() {
        let a = "name: \"media\"\nservices:\n  web:\n    image: x\n    profiles: [debug, \"tools\"]\n  job:\n    profiles:\n      - batch\n    image: y\nnetworks:\n  proxy:\n    external: true\n  internal:\n    driver: bridge\n";
        let b = "include:\n  - other.yaml\nname: media2 # comment\n";
        let f = scan_texts(&[a.into(), b.into()]);
        assert_eq!(f.name.as_deref(), Some("media2"));
        assert!(f.include);
        assert_eq!(f.profiles.into_iter().collect::<Vec<_>>(), ["batch", "debug", "tools"]);
        assert_eq!(f.external_networks.into_iter().collect::<Vec<_>>(), ["proxy"]);
        assert_eq!(scan_texts(&["services: {}\n".into()]), FileFacts::default());
    }

    #[test]
    fn warnings_of_an_adoption() {
        let facts = scan_texts(&["name: other\nservices: {}\n".into()]);
        let w = warnings(&WarnInput {
            name: "app",
            dir: "/srv/app",
            files: &["/srv/app/compose.yaml".into()],
            env_files: &["/elsewhere/.env".into()],
            facts: &facts,
            origin: Some(ManagerKind::Dockge),
            name_in_use: true,
            missing: false,
        });
        assert_eq!(
            w,
            [
                AdoptWarning::OtherManager,
                AdoptWarning::NameDiffers,
                AdoptWarning::NameInUse,
                AdoptWarning::OutsideDir
            ]
        );
    }

    #[test]
    fn registry_hashes_and_writes() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir_all(root.join("srv/app")).unwrap();
        std::fs::write(root.join("srv/app/compose.yaml"), "services: {}\n").unwrap();
        let files = vec!["/srv/app/compose.yaml".to_string()];
        let h1 = files_hash(root, &files, &[]);
        let mut a = Adopted {
            dir: "/srv/app".into(),
            files: files.clone(),
            hash: h1.clone(),
            ..Default::default()
        };
        assert!(!a.info(Some(&h1)).changed_outside);
        assert!(a.writable());
        // another manager edits the file
        std::fs::write(root.join("srv/app/compose.yaml"), "services: {a: {}}\n").unwrap();
        let h2 = files_hash(root, &files, &[]);
        assert!(a.info(Some(&h2)).changed_outside);
        // rynas keeps the mode of a file it rewrites
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            root.join("srv/app/compose.yaml"),
            std::fs::Permissions::from_mode(0o664),
        )
        .unwrap();
        write_abs(root, "/srv/app/compose.yaml", "services: {b: {}}\n", 0o640).unwrap();
        let m = std::fs::metadata(root.join("srv/app/compose.yaml")).unwrap().permissions().mode();
        assert_eq!(m & 0o777, 0o664);
        write_abs(root, "/srv/app/.env", "A=1\n", 0o600).unwrap();
        assert_eq!(read_abs(root, &a.env_path()).unwrap(), "A=1\n");
        // a symlinked folder on the way is never followed
        std::os::unix::fs::symlink(root.join("srv/app"), root.join("srv/link")).unwrap();
        assert!(read_abs(root, "/srv/link/compose.yaml").is_err());
        assert!(!is_file(root, "/srv/link/compose.yaml") && is_file(root, "/srv/app/.env"));
        // the registry round trip
        a.read_only = true;
        assert!(!a.writable());
        let mut r = Registry::default();
        r.stacks.insert("app".into(), a.clone());
        let st = root.join("state");
        r.save(&st).unwrap();
        assert_eq!(Registry::load(&st).unwrap(), r);
        assert_eq!(Registry::load(&root.join("none")).unwrap(), Registry::default());
    }

    #[test]
    fn moves() {
        let b = |s: &str, src: &str, t: &str| Bind {
            service: s.into(),
            source: src.into(),
            target: t.into(),
            create: true,
        };
        let old = [
            b("web", "/srv/app/data", "/data"),
            b("web", "/volume1/media", "/media"),
        ];
        let good = [
            b("web", "/v1/docker/stacks/app/data", "/data"),
            b("web", "/volume1/media", "/media"),
        ];
        assert!(check_moved_binds(&old, &good, "/srv/app", "/v1/docker/stacks/app").is_empty());
        // `../shared` resolved differently after the move
        let old2 = [b("web", "/srv/shared", "/s")];
        let new2 = [b("web", "/v1/docker/stacks/shared", "/s")];
        assert_eq!(check_moved_binds(&old2, &new2, "/srv/app", "/v1/docker/stacks/app").len(), 1);
        let a = Adopted {
            dir: "/srv/app/".into(),
            files: vec!["/srv/app/docker-compose.yml".into()],
            env_files: vec!["/srv/app/.env".into()],
            origin: Some(ManagerKind::Dockge),
            ..Default::default()
        };
        let m = moved(&a, "/v1/docker/stacks/app");
        assert_eq!(m.files, ["/v1/docker/stacks/app/docker-compose.yml"]);
        assert!(is_plain(&m) && m.origin.is_none());
        let mut two = m.clone();
        two.files.push("/v1/docker/stacks/app/docker-compose.override.yml".into());
        assert!(!is_plain(&two));
    }
}
