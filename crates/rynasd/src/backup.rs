//! `rynasd export` / `rynasd import`: the `.rynasbak` archive of rynas' state
//! ([`rynas_config::archive`]).
//!
//! What goes in:
//! - `config.db`: every configuration domain, including the Task Scheduler's task
//!   definitions (key `scheduler`, scripts included) and the snapshot jobs (`snapshots`);
//! - `notify.db`: notification targets, rules, silences and the event history (SQLite
//!   backup API, consistent while rynasd runs);
//! - with a passphrase, the encrypted secrets: TOTP secrets (`auth/totp/<user>.json`) and
//!   the notification key (`notify/secret.key`) that seals the targets' passwords and tokens
//!   inside `notify.db`.
//!
//! What stays out: `scheduler.db` (run history with up to 256 KiB of output per run, 5000
//! runs: bulky, machine-specific and rebuilt by the next runs), task logs, generated files
//! (rendered again from the configuration on apply), snapshots themselves.
//!
//! Import stages the configuration as a pending changeset (never applied by itself). TOTP
//! secrets are written only with `restore_secrets` and only where none exists. The
//! notification store is replaced only with `restore_notifications` (rynasd must be
//! stopped: the engine keeps its key in memory), together with its key when the archive
//! has it; the replaced files are kept next to the new ones (`*.before-import-<ms>`).

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use rynas_config::Config;
use rynas_config::archive::{Manifest, Secrets, read_archive, restore_database};
use rynas_scheduler::SCHEDULER_KEY;

use crate::ops::root_task_guard;
use crate::roles::AdminPolicy;

/// Member name of the notification database.
pub const NOTIFY_DB: &str = "notify.db";
/// Secret name of the notification key.
pub const NOTIFY_KEY_SECRET: &str = "notify/secret.key";
const TOTP_PREFIX: &str = "auth/totp/";

fn notify_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("notify")
}

fn totp_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("auth").join("totp")
}

fn io(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

/// What [`export`] wrote.
#[derive(Debug)]
pub struct ExportReport {
    pub manifest: Manifest,
    /// Secrets encrypted in the archive (0 without a passphrase).
    pub secrets: usize,
}

/// The secrets of `state_dir`: TOTP files and the notification key.
pub fn collect_secrets(state_dir: &Path) -> std::io::Result<Secrets> {
    let mut secrets = Secrets::new();
    if let Ok(dir) = std::fs::read_dir(totp_dir(state_dir)) {
        for e in dir.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".json") {
                secrets.insert(format!("{TOTP_PREFIX}{name}"), std::fs::read(e.path())?);
            }
        }
    }
    let key = notify_dir(state_dir).join(rynas_notify::engine::secrets::KEY_FILE);
    if key.exists() {
        secrets.insert(NOTIFY_KEY_SECRET.into(), std::fs::read(key)?);
    }
    Ok(secrets)
}

/// Writes the archive `out` of `cfg` and the state in `state_dir`; secrets only with a
/// passphrase.
pub fn export(
    cfg: &Config,
    state_dir: &Path,
    out: &Path,
    hostname: &str,
    passphrase: Option<&str>,
) -> std::io::Result<ExportReport> {
    let secrets = match passphrase {
        Some(_) => collect_secrets(state_dir)?,
        None => Secrets::new(),
    };
    let notify_db = notify_dir(state_dir).join(NOTIFY_DB);
    let manifest = cfg
        .export_with(
            out,
            hostname,
            &[(NOTIFY_DB, notify_db.as_path())],
            passphrase.map(|p| (p, &secrets)),
        )
        .map_err(io)?;
    Ok(ExportReport {
        secrets: if manifest.secrets { secrets.len() } else { 0 },
        manifest,
    })
}

#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    /// Owner of the pending changeset.
    pub owner: String,
    /// Write archived TOTP secrets that do not exist here yet.
    pub restore_secrets: bool,
    /// Replace the notification store (and its key, when the archive has it).
    pub restore_notifications: bool,
    /// The owner must be an administrator when the archive adds, changes or removes root
    /// user tasks (the system groups by default).
    pub admins: AdminPolicy,
}

/// What happened to the notification store on import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyRestore {
    /// The archive has no notification store.
    NotInArchive,
    /// In the archive, not restored (no `restore_notifications`).
    Skipped,
    /// Restored; `key` says whether the key came with it (otherwise the targets' secrets
    /// cannot be opened and must be entered again). `kept`: the replaced files.
    Restored { key: bool, kept: Vec<PathBuf> },
}

/// What [`import`] did.
#[derive(Debug)]
pub struct ImportReport {
    pub manifest: Manifest,
    pub staged: u32,
    /// TOTP secrets in the archive (readable with the passphrase).
    pub totp_in_archive: usize,
    pub totp_restored: usize,
    pub notifications: NotifyRestore,
}

/// Reads `file` and stages its configuration for `opts.owner`; restores secrets and the
/// notification store as asked.
pub fn import(
    cfg: &Config,
    state_dir: &Path,
    file: &Path,
    passphrase: Option<&str>,
    opts: &ImportOptions,
) -> std::io::Result<ImportReport> {
    let a = read_archive(file, passphrase, state_dir).map_err(io)?;
    // checked before anything is staged or written
    if cfg.keys().iter().any(|k| k == SCHEDULER_KEY)
        && let Some(target) = a.values.get(SCHEDULER_KEY)
    {
        let applied = cfg.applied(SCHEDULER_KEY).map_err(io)?;
        root_task_guard(&opts.admins, &opts.owner, applied.as_ref(), Some(target))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e.2))?;
    }
    let staged = cfg.stage_values(&opts.owner, &a.values).map_err(io)?;
    let totp: Vec<(&String, &Vec<u8>)> = a
        .secrets
        .iter()
        .flatten()
        .filter(|(n, _)| n.starts_with(TOTP_PREFIX))
        .collect();
    let mut totp_restored = 0;
    if opts.restore_secrets && !totp.is_empty() {
        let dir = totp_dir(state_dir);
        std::fs::create_dir_all(&dir)?;
        for (name, data) in &totp {
            let file = &name[TOTP_PREFIX.len()..];
            // names come from the archive: no path components
            if file.is_empty() || file.contains('/') || file.starts_with('.') {
                continue;
            }
            if write_new(&dir.join(file), data)? {
                totp_restored += 1;
            }
        }
    }
    let notifications = match a.databases.get(NOTIFY_DB) {
        None => NotifyRestore::NotInArchive,
        Some(_) if !opts.restore_notifications => NotifyRestore::Skipped,
        Some(db) => {
            let dir = notify_dir(state_dir);
            std::fs::create_dir_all(&dir)?;
            std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
            let mut kept = Vec::new();
            let key = a.secrets.as_ref().and_then(|s| s.get(NOTIFY_KEY_SECRET));
            // the key first: the database's sealed secrets are useless without it
            if let Some(k) = key {
                let path = dir.join(rynas_notify::engine::secrets::KEY_FILE);
                if path.exists() {
                    let old = before_import(&path);
                    std::fs::rename(&path, &old)?;
                    kept.push(old);
                }
                write_new(&path, k)?;
            }
            if let Some(old) = restore_database(db, &dir.join(NOTIFY_DB), state_dir).map_err(io)? {
                kept.push(old);
            }
            NotifyRestore::Restored {
                key: key.is_some(),
                kept,
            }
        }
    };
    Ok(ImportReport {
        manifest: a.manifest,
        staged,
        totp_in_archive: totp.len(),
        totp_restored,
        notifications,
    })
}

fn before_import(p: &Path) -> PathBuf {
    let mut name = p.as_os_str().to_owned();
    name.push(format!(".before-import-{}", rynas_tasks::now_ms()));
    PathBuf::from(name)
}

/// Writes a new 0600 file; `false` when it already exists.
fn write_new(path: &Path, data: &[u8]) -> std::io::Result<bool> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut f) => {
            f.write_all(data)?;
            f.sync_all()?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}
