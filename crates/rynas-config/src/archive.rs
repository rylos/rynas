//! Configuration export/import: `.rynasbak` archives (tar + zstd).
//!
//! Content:
//! - `manifest.json`: format, rynas version, schema version, hostname, time, keys, whether
//!   secrets are included;
//! - `config.db`: a consistent copy of the live database (SQLite backup API, safe with WAL);
//! - `secrets.age` (only with a passphrase): a JSON map `name → bytes`, encrypted with the
//!   age scrypt passphrase recipient. Without a passphrase the archive has no secrets;
//! - other SQLite databases of rynas' own state (e.g. `notify.db`), listed in
//!   `manifest.databases`, each a consistent copy made with the SQLite backup API. They are
//!   not configuration: the caller decides whether to restore them ([`restore_database`]).
//!
//! Generated files are not exported: they are rendered again from the database. An import
//! never applies anything: it stages the archive's values as a pending changeset to review.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use age::secrecy::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::engine::{Config, ConfigError, Result};

pub const FORMAT: u32 = 1;
const MANIFEST: &str = "manifest.json";
const CONFIG_DB: &str = "config.db";
const SECRETS: &str = "secrets.age";
/// Refuse archive members larger than this (corrupt or hostile archives).
const MAX_MEMBER: u64 = 256 << 20;

/// Secret blobs by name, e.g. `auth/totp/alice.json`.
pub type Secrets = BTreeMap<String, Vec<u8>>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub rynas_version: String,
    pub schema_version: u32,
    pub created_ms: u64,
    pub hostname: String,
    pub keys: Vec<String>,
    /// The archive has a `secrets.age` member.
    pub secrets: bool,
    /// Extra database members (`notify.db`...); absent in older archives.
    #[serde(default)]
    pub databases: Vec<String>,
}

/// A read archive.
#[derive(Debug)]
pub struct Archive {
    pub manifest: Manifest,
    /// The `config` table of the archived database.
    pub values: BTreeMap<String, Value>,
    /// Decrypted secrets; `None` when the archive has none or no passphrase was given.
    pub secrets: Option<Secrets>,
    /// Extra databases by member name (raw SQLite files).
    pub databases: BTreeMap<String, Vec<u8>>,
}

/// Names allowed for extra database members: `[a-z0-9_-]+.db`, not `config.db`.
pub fn valid_database_name(n: &str) -> bool {
    n != CONFIG_DB
        && n.len() <= 64
        && n.strip_suffix(".db").is_some_and(|b| {
            !b.is_empty()
                && b.bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
        })
}

/// A consistent copy of the SQLite database at `src` (SQLite backup API: safe while another
/// process writes it, WAL included), as bytes.
fn snapshot_database(src: &Path, tmp_dir: &Path) -> Result<Vec<u8>> {
    let conn = rusqlite::Connection::open_with_flags(
        src,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let tmp = tempfile::NamedTempFile::new_in(tmp_dir)?;
    conn.backup(
        rusqlite::MAIN_DB,
        tmp.path(),
        None::<fn(rusqlite::backup::Progress)>,
    )?;
    Ok(std::fs::read(tmp.path())?)
}

/// Replaces the database at `dst` with an archived copy, through the SQLite backup API (a
/// process holding `dst` open sees the new content, never a torn file). The archived bytes
/// are checked first (`PRAGMA quick_check`); the current database, if any, is kept as
/// `<dst>.before-import-<ms>` (returned).
pub fn restore_database(
    data: &[u8],
    dst: &Path,
    tmp_dir: &Path,
) -> Result<Option<std::path::PathBuf>> {
    let tmp = tempfile::NamedTempFile::new_in(tmp_dir)?;
    std::fs::write(tmp.path(), data)?;
    let src = rusqlite::Connection::open_with_flags(
        tmp.path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let check: String = src
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .map_err(|e| {
            ConfigError::Invalid(format!("{}: not a database: {e}", dst.display()).into())
        })?;
    if check != "ok" {
        return Err(ConfigError::Invalid(
            format!("{}: archived copy is damaged: {check}", dst.display()).into(),
        ));
    }
    let kept = if dst.exists() {
        let mut name = dst.as_os_str().to_owned();
        name.push(format!(".before-import-{}", rynas_tasks::now_ms()));
        let kept = std::path::PathBuf::from(name);
        let cur = rusqlite::Connection::open(dst)?;
        cur.backup(
            rusqlite::MAIN_DB,
            &kept,
            None::<fn(rusqlite::backup::Progress)>,
        )?;
        std::fs::set_permissions(&kept, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        Some(kept)
    } else {
        None
    };
    let mut cur = rusqlite::Connection::open(dst)?;
    cur.restore(
        rusqlite::MAIN_DB,
        tmp.path(),
        None::<fn(rusqlite::backup::Progress)>,
    )?;
    std::fs::set_permissions(dst, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    Ok(kept)
}

fn storage(e: impl std::fmt::Display) -> ConfigError {
    ConfigError::Storage(e.to_string())
}

fn append(tar: &mut tar::Builder<impl Write>, name: &str, data: &[u8]) -> Result<()> {
    let mut h = tar::Header::new_gnu();
    h.set_size(data.len() as u64);
    h.set_mode(0o600);
    h.set_mtime(rynas_tasks::now_ms() / 1000);
    h.set_cksum();
    tar.append_data(&mut h, name, data).map_err(storage)
}

impl Config {
    /// Writes a `.rynasbak` archive to `out` (mode 0600). Secrets are included only with a
    /// passphrase.
    pub fn export(
        &self,
        out: &Path,
        hostname: &str,
        secrets: Option<(&str, &Secrets)>,
    ) -> Result<Manifest> {
        self.export_with(out, hostname, &[], secrets)
    }

    /// [`Config::export`] plus extra SQLite databases `(member name, path)`; a path that
    /// does not exist is skipped.
    pub fn export_with(
        &self,
        out: &Path,
        hostname: &str,
        databases: &[(&str, &Path)],
        secrets: Option<(&str, &Secrets)>,
    ) -> Result<Manifest> {
        let mut extra = Vec::new();
        for (name, path) in databases {
            if !valid_database_name(name) {
                return Err(ConfigError::Invalid(
                    format!("invalid database member {name}").into(),
                ));
            }
            if path.exists() {
                extra.push((
                    name.to_string(),
                    snapshot_database(path, &self.options().state_dir)?,
                ));
            }
        }
        let tmp = tempfile::NamedTempFile::new_in(&self.options().state_dir)?;
        {
            let db = self.db();
            db.backup(
                rusqlite::MAIN_DB,
                tmp.path(),
                None::<fn(rusqlite::backup::Progress)>,
            )?;
        }
        let db_bytes = std::fs::read(tmp.path())?;
        let keys: Vec<String> = {
            let db = self.db();
            let mut st = db.prepare("SELECT key FROM config ORDER BY key")?;
            st.query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        let manifest = Manifest {
            format: FORMAT,
            rynas_version: env!("CARGO_PKG_VERSION").into(),
            schema_version: crate::db::SCHEMA_VERSION,
            created_ms: rynas_tasks::now_ms(),
            hostname: hostname.into(),
            keys,
            secrets: secrets.is_some(),
            databases: extra.iter().map(|(n, _)| n.clone()).collect(),
        };
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(out)?;
        let enc = zstd::Encoder::new(file, 9)?;
        let mut tar = tar::Builder::new(enc);
        append(&mut tar, MANIFEST, &serde_json::to_vec_pretty(&manifest)?)?;
        append(&mut tar, CONFIG_DB, &db_bytes)?;
        for (name, data) in &extra {
            append(&mut tar, name, data)?;
        }
        if let Some((pass, s)) = secrets {
            if pass.is_empty() {
                return Err(ConfigError::Invalid("empty passphrase".into()));
            }
            let recipient = age::scrypt::Recipient::new(SecretString::from(pass.to_owned()));
            let plain = serde_json::to_vec(s)?;
            let sealed = age::encrypt(&recipient, &plain).map_err(storage)?;
            append(&mut tar, SECRETS, &sealed)?;
        }
        let enc = tar.into_inner().map_err(storage)?;
        enc.finish()?.sync_all()?;
        Ok(manifest)
    }
}

/// Reads an archive. The database copy is migrated to the current schema in a temporary
/// file; an archive from a newer rynas is refused. A wrong passphrase is an error.
pub fn read_archive(path: &Path, passphrase: Option<&str>, tmp_dir: &Path) -> Result<Archive> {
    let file = std::fs::File::open(path)?;
    let dec = zstd::Decoder::new(file)?;
    let mut tar = tar::Archive::new(dec);
    let (mut manifest, mut db, mut sealed) = (None, None, None);
    let mut databases = BTreeMap::new();
    for entry in tar.entries().map_err(storage)? {
        let entry = entry.map_err(storage)?;
        let name = entry
            .path()
            .map_err(storage)?
            .to_string_lossy()
            .into_owned();
        if entry.size() > MAX_MEMBER {
            return Err(storage(format!("{name}: member too large")));
        }
        let mut data = Vec::new();
        entry.take(MAX_MEMBER).read_to_end(&mut data)?;
        match name.as_str() {
            MANIFEST => manifest = Some(serde_json::from_slice::<Manifest>(&data)?),
            CONFIG_DB => db = Some(data),
            SECRETS => sealed = Some(data),
            other if valid_database_name(other) => {
                databases.insert(other.to_string(), data);
            }
            other => tracing::warn!("archive: unknown member {other} ignored"),
        }
    }
    let manifest = manifest.ok_or_else(|| storage("not a rynas archive: no manifest"))?;
    if manifest.format > FORMAT {
        return Err(storage(format!(
            "archive format {} is newer than this rynas ({FORMAT})",
            manifest.format
        )));
    }
    let db = db.ok_or_else(|| storage("archive without config.db"))?;
    let tmp = tempfile::NamedTempFile::new_in(tmp_dir)?;
    std::fs::write(tmp.path(), &db)?;
    let values = {
        let conn = crate::db::open(tmp.path())?;
        let mut st = conn.prepare("SELECT key, value FROM config")?;
        st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .map(|r| {
                let (k, v) = r?;
                Ok((k, serde_json::from_str(&v).unwrap_or(Value::Null)))
            })
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?
    };
    // WAL side files of the temporary copy
    for ext in ["-wal", "-shm"] {
        let mut p = tmp.path().as_os_str().to_owned();
        p.push(ext);
        let _ = std::fs::remove_file(p);
    }
    let secrets = match (sealed, passphrase) {
        (Some(sealed), Some(pass)) => {
            let id = age::scrypt::Identity::new(SecretString::from(pass.to_owned()));
            let plain = age::decrypt(&id, &sealed)
                .map_err(|e| ConfigError::Invalid(format!("secrets: {e}").into()))?;
            Some(serde_json::from_slice(&plain)?)
        }
        _ => None,
    };
    databases.retain(|n, _| manifest.databases.contains(n));
    Ok(Archive {
        manifest,
        values,
        secrets,
        databases,
    })
}
