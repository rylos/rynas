//! Signed online catalogue updates (D-APPCAT note of 2026-11-10,
//! `docs/phase2/docker-apps.md` § 6.12): the pure half, without any network access.
//!
//! The catalogue site serves `index.json`, its minisign signature `index.json.minisig` and one
//! archive `apps-<serial>.tar.zst` named by the index. rynasd downloads them and calls, in
//! this order:
//! 1. [`verify_index`]: the signature by one of the compiled-in keys ([`TRUSTED_KEYS`]), the
//!    trusted comment `serial=<n> expires=<RFC 3339>` equal to the index's own fields (the
//!    signature covers the comment, so a re-signed old index cannot pose as a new one);
//! 2. [`check_fresh`]: refuses an expired index (freeze) and a serial below the last accepted
//!    one (rollback);
//! 3. [`check_archive`]: size and SHA-256 named by the index;
//! 4. [`extract`]: regular files and folders only, relative paths without `..`, limits on
//!    the number of entries and the unpacked size;
//! 5. [`validate`]: every entry loads with the catalogue loader and lint, and matches the
//!    index (versions, `min_rynas`); an entry that fails is accepted only when it needs a
//!    newer rynas than the running one. A catalogue that fails never replaces a good one.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::catalog::{CatalogApp, read_serial};
use crate::version::Version;

/// Public keys (minisign, the second line of the `.pub` file) whose signatures the NAS
/// accepts: the current key and, during a rotation, the next one.
///
/// The current key signs in CI (its secret key is a repository secret,
/// `CATALOG_MINISIGN_KEY`); the next one is kept offline by the maintainer and takes over
/// if the current one is lost or exposed (`docs/phase2/docker-apps.md` § 6.13). Keys made
/// on 2026-11-10.
pub const TRUSTED_KEYS: &[&str] = &[
    // current
    "RWSfdwkFZp8XBSRvif+fFMcGuL8lsgC9DzMC8ZSmLwJunB6VFIqxwRIV",
    // next (offline)
    "RWS94XMpNnUNgE7djMCP6+ooMQji2AvQ7uh8HVswKhYu2wYdPYm7DkcH",
];

/// The only index format this rynas reads.
pub const INDEX_SCHEMA: u32 = 1;
/// Largest `index.json` accepted.
pub const MAX_INDEX: u64 = 1 << 20;
/// Largest `index.json.minisig` accepted.
pub const MAX_SIGNATURE: u64 = 4 << 10;
/// Largest archive accepted.
pub const MAX_ARCHIVE: u64 = 20 << 20;
/// Largest unpacked catalogue (zstd bombs stop here).
pub const MAX_UNPACKED: u64 = 64 << 20;
/// Largest single file in the archive.
pub const MAX_FILE: u64 = 8 << 20;
/// Most entries (files and folders) in the archive.
pub const MAX_ENTRIES: usize = 10_000;
/// Deepest path in the archive (`<id>/metadata/screenshots/<file>` is 4).
const MAX_DEPTH: usize = 6;
/// An index valid for longer than this is refused (a signing mistake would freeze the
/// catalogue for years).
const MAX_VALIDITY_DAYS: i64 = 400;

/// Why an online catalogue was refused or could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnlineError {
    /// No public key in this build.
    NoKey,
    /// The signature does not verify, or cannot be read.
    Signature(String),
    /// Signed by a key this build does not trust.
    UnknownKey,
    /// The trusted comment does not match the index.
    Comment(String),
    /// The index cannot be read, or its content is invalid.
    Index(String),
    /// The serial is below the last accepted one.
    Rollback { serial: u64, last: u64 },
    /// The index expired: the site stopped being refreshed, or someone replays an old one.
    Expired(String),
    /// The archive differs from what the index names.
    Archive(String),
    /// The archive holds something that is not a plain file or folder below its root.
    Unsafe(String),
    /// The unpacked catalogue does not load or does not match the index.
    Invalid(String),
    /// The catalogue site could not be read (network, HTTP status, size limit).
    Download(String),
    /// Local file system error.
    Io(String),
}

impl OnlineError {
    /// A stable code for the API and the logs.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoKey => "no_key",
            Self::Signature(_) => "signature",
            Self::UnknownKey => "unknown_key",
            Self::Comment(_) => "trusted_comment",
            Self::Index(_) => "index",
            Self::Rollback { .. } => "rollback",
            Self::Expired(_) => "expired",
            Self::Archive(_) => "archive",
            Self::Unsafe(_) => "unsafe_archive",
            Self::Invalid(_) => "invalid_catalogue",
            Self::Download(_) => "download",
            Self::Io(_) => "io",
        }
    }

    /// The data was refused (worth a warning), as opposed to a local error.
    pub fn is_refusal(&self) -> bool {
        !matches!(self, Self::NoKey | Self::Download(_) | Self::Io(_))
    }
}

impl std::fmt::Display for OnlineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoKey => write!(f, "this build has no catalogue signing key"),
            Self::Signature(m) => write!(f, "bad signature: {m}"),
            Self::UnknownKey => write!(f, "signed with a key this build does not trust"),
            Self::Comment(m) => write!(f, "trusted comment: {m}"),
            Self::Index(m) => write!(f, "index: {m}"),
            Self::Rollback { serial, last } => {
                write!(f, "serial {serial} is older than the accepted serial {last}")
            }
            Self::Expired(at) => write!(f, "the index expired at {at}"),
            Self::Archive(m) => write!(f, "archive: {m}"),
            Self::Unsafe(m) => write!(f, "unsafe archive: {m}"),
            Self::Invalid(m) => write!(f, "invalid catalogue: {m}"),
            Self::Download(m) => write!(f, "download: {m}"),
            Self::Io(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for OnlineError {}

fn io_err(what: &str, e: impl std::fmt::Display) -> OnlineError {
    OnlineError::Io(format!("{what}: {e}"))
}

/// The archive an index names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveRef {
    /// A file name next to `index.json`, or an absolute `https://` URL.
    pub url: String,
    /// Lowercase hex SHA-256.
    pub sha256: String,
    pub size: u64,
}

/// One entry as the index lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexApp {
    pub id: String,
    pub version: Version,
    pub min_rynas: Version,
}

/// `index.json`. Unknown fields are ignored (a later publisher may add some; the signature
/// covers them all).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    pub schema: u32,
    /// Only goes up; also in the archive's `catalog.toml`.
    pub serial: u64,
    /// When it was published (RFC 3339), informative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    /// RFC 3339; the NAS refuses the index afterwards.
    pub expires: String,
    pub archive: ArchiveRef,
    pub apps: Vec<IndexApp>,
}

impl Index {
    /// When it expires.
    pub fn expires_at(&self) -> Result<jiff::Timestamp, OnlineError> {
        self.expires
            .parse::<jiff::Timestamp>()
            .map_err(|e| OnlineError::Index(format!("expires `{}`: {e}", self.expires)))
    }

    fn check(&self) -> Result<(), OnlineError> {
        let bad = |m: String| Err(OnlineError::Index(m));
        if self.schema != INDEX_SCHEMA {
            return bad(format!("schema {} is not supported (this rynas reads {INDEX_SCHEMA})", self.schema));
        }
        self.expires_at()?;
        let a = &self.archive;
        if a.sha256.len() != 64 || !a.sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return bad("archive.sha256 is not 64 lowercase hex digits".into());
        }
        if a.size == 0 || a.size > MAX_ARCHIVE {
            return bad(format!("archive.size {} is not between 1 and {MAX_ARCHIVE}", a.size));
        }
        if a.url.is_empty() {
            return bad("archive.url is empty".into());
        }
        let mut seen = BTreeSet::new();
        for app in &self.apps {
            if !crate::manifest::is_app_id(&app.id) {
                return bad(format!("`{}` is not an app id", app.id));
            }
            if !seen.insert(app.id.as_str()) {
                return bad(format!("`{}` is listed twice", app.id));
            }
        }
        if self.apps.is_empty() {
            return bad("no apps".into());
        }
        Ok(())
    }
}

/// `serial=<n> expires=<date>` of a trusted comment (other words are ignored).
fn comment_fields(c: &str) -> Result<(u64, String), OnlineError> {
    let (mut serial, mut expires) = (None, None);
    for w in c.split_whitespace() {
        let Some((k, v)) = w.split_once('=') else { continue };
        let slot = match k {
            "serial" => &mut serial,
            "expires" => &mut expires,
            _ => continue,
        };
        if slot.replace(v.to_owned()).is_some() {
            return Err(OnlineError::Comment(format!("`{k}` appears twice")));
        }
    }
    let serial = serial
        .ok_or_else(|| OnlineError::Comment("no serial".into()))?
        .parse::<u64>()
        .map_err(|_| OnlineError::Comment("serial is not a number".into()))?;
    let expires = expires.ok_or_else(|| OnlineError::Comment("no expires".into()))?;
    Ok((serial, expires))
}

/// Checks the minisign signature of `index` with one of `keys` (base64 public keys), then
/// parses the index and checks that the signed trusted comment names its serial and expiry.
pub fn verify_index(index: &[u8], signature: &str, keys: &[&str]) -> Result<Index, OnlineError> {
    use minisign_verify::{Error as MsError, PublicKey, Signature};
    if keys.is_empty() {
        return Err(OnlineError::NoKey);
    }
    if index.len() as u64 > MAX_INDEX {
        return Err(OnlineError::Index(format!("larger than {MAX_INDEX} bytes")));
    }
    let sig = Signature::decode(signature).map_err(|e| OnlineError::Signature(e.to_string()))?;
    let mut verified = false;
    for k in keys {
        let pk = PublicKey::from_base64(k.trim())
            .map_err(|e| OnlineError::Signature(format!("compiled-in key: {e}")))?;
        match pk.verify(index, &sig, false) {
            Ok(()) => {
                verified = true;
                break;
            }
            Err(MsError::UnexpectedKeyId) => continue,
            Err(e) => return Err(OnlineError::Signature(e.to_string())),
        }
    }
    if !verified {
        return Err(OnlineError::UnknownKey);
    }
    let idx: Index = serde_json::from_slice(index).map_err(|e| OnlineError::Index(e.to_string()))?;
    let (serial, expires) = comment_fields(sig.trusted_comment())?;
    if serial != idx.serial {
        return Err(OnlineError::Comment(format!("serial {serial} differs from the index's {}", idx.serial)));
    }
    if expires != idx.expires {
        return Err(OnlineError::Comment(format!("expires {expires} differs from the index's {}", idx.expires)));
    }
    idx.check()?;
    Ok(idx)
}

/// What a verified index means for this NAS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fresh {
    /// A newer catalogue: download it.
    New,
    /// The catalogue already in use.
    Same,
}

/// Expiry and serial rules: refuses an expired index, one valid for too long, and a serial
/// below `last` (the last accepted).
pub fn check_fresh(idx: &Index, now: jiff::Timestamp, last: Option<u64>) -> Result<Fresh, OnlineError> {
    let exp = idx.expires_at()?;
    if exp <= now {
        return Err(OnlineError::Expired(idx.expires.clone()));
    }
    if exp.duration_since(now).as_secs() > MAX_VALIDITY_DAYS * 86_400 {
        return Err(OnlineError::Index(format!("valid for more than {MAX_VALIDITY_DAYS} days")));
    }
    match last {
        Some(l) if idx.serial < l => Err(OnlineError::Rollback { serial: idx.serial, last: l }),
        Some(l) if idx.serial == l => Ok(Fresh::Same),
        _ => Ok(Fresh::New),
    }
}

/// Lowercase hex SHA-256.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// The archive has the size and the SHA-256 the index names.
pub fn check_archive(data: &[u8], a: &ArchiveRef) -> Result<(), OnlineError> {
    if data.len() as u64 != a.size {
        return Err(OnlineError::Archive(format!("{} bytes instead of {}", data.len(), a.size)));
    }
    let got = sha256_hex(data);
    if got != a.sha256 {
        return Err(OnlineError::Archive(format!("SHA-256 {got} instead of {}", a.sha256)));
    }
    Ok(())
}

/// The archive's URL: an absolute `https://` URL (`http://` only with `allow_http`, for
/// tests), or a plain file name resolved next to `index_url`.
pub fn archive_url(index_url: &str, a: &ArchiveRef, allow_http: bool) -> Result<String, OnlineError> {
    let u = a.url.as_str();
    if u.contains("://") {
        let ok = u.starts_with("https://") || (allow_http && u.starts_with("http://"));
        if !ok || u.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(OnlineError::Index(format!("archive.url `{u}` is not an https URL")));
        }
        return Ok(u.to_owned());
    }
    let plain = !u.starts_with('.') && u.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if !plain {
        return Err(OnlineError::Index(format!("archive.url `{u}` is neither a file name nor an https URL")));
    }
    let base = index_url.rsplit_once('/').map_or(index_url, |(b, _)| b);
    Ok(format!("{base}/{u}"))
}

/// What [`extract`] wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Extracted {
    pub files: usize,
    pub dirs: usize,
    pub bytes: u64,
}

/// A relative path of plain components (`./` dropped); refuses absolute paths, `..`, too
/// deep paths and names with control characters.
fn safe_rel(p: &Path) -> Result<PathBuf, OnlineError> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(n) => {
                let s = n.to_str().ok_or_else(|| OnlineError::Unsafe(format!("{}: not UTF-8", p.display())))?;
                if s.len() > 255 || s.chars().any(char::is_control) {
                    return Err(OnlineError::Unsafe(format!("{}: bad name", p.display())));
                }
                out.push(s);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(OnlineError::Unsafe(format!("{}: absolute path or `..`", p.display())));
            }
        }
    }
    if out.components().count() > MAX_DEPTH {
        return Err(OnlineError::Unsafe(format!("{}: too deep", p.display())));
    }
    Ok(out)
}

/// Unpacks a `.tar.zst` catalogue into `dest`, which must not exist yet (created 0755).
/// Only regular files (0644) and folders (0755) are written, never modes, owners, links or
/// devices from the archive; files are created with `O_EXCL`, so a duplicate name fails. On
/// an error `dest` keeps what was written: the caller removes it.
pub fn extract(archive: &[u8], dest: &Path) -> Result<Extracted, OnlineError> {
    fs::create_dir(dest).map_err(|e| io_err(&format!("create {}", dest.display()), e))?;
    let _ = fs::set_permissions(dest, fs::Permissions::from_mode(0o755));
    let dec = zstd::stream::read::Decoder::new(archive).map_err(|e| OnlineError::Archive(format!("zstd: {e}")))?;
    // headers and padding come on top of the file data: one more MiB
    let mut tar = tar::Archive::new(dec.take(MAX_UNPACKED + (1 << 20)));
    let mut st = Extracted::default();
    let entries = tar.entries().map_err(|e| OnlineError::Archive(format!("tar: {e}")))?;
    for (n, e) in entries.enumerate() {
        if n >= MAX_ENTRIES {
            return Err(OnlineError::Unsafe(format!("more than {MAX_ENTRIES} entries")));
        }
        let mut e = e.map_err(|e| OnlineError::Archive(format!("tar: {e}")))?;
        let raw = e.path().map_err(|e| OnlineError::Archive(format!("tar: {e}")))?.into_owned();
        let rel = safe_rel(&raw)?;
        let kind = e.header().entry_type();
        let target = dest.join(&rel);
        if kind.is_dir() {
            if rel.as_os_str().is_empty() {
                continue;
            }
            fs::create_dir_all(&target).map_err(|e| io_err(&format!("create {}", rel.display()), e))?;
            let _ = fs::set_permissions(&target, fs::Permissions::from_mode(0o755));
            st.dirs += 1;
        } else if kind.is_file() {
            if rel.as_os_str().is_empty() {
                return Err(OnlineError::Unsafe("a file without a name".into()));
            }
            let size = e.header().size().map_err(|e| OnlineError::Archive(format!("tar: {e}")))?;
            if size > MAX_FILE {
                return Err(OnlineError::Unsafe(format!("{}: larger than {MAX_FILE} bytes", rel.display())));
            }
            st.bytes += size;
            if st.bytes > MAX_UNPACKED {
                return Err(OnlineError::Unsafe(format!("more than {MAX_UNPACKED} bytes unpacked")));
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| io_err(&format!("create {}", rel.display()), e))?;
            }
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .custom_flags(libc_nofollow())
                .open(&target)
                .map_err(|e| OnlineError::Unsafe(format!("{}: {e}", rel.display())))?;
            let copied = io::copy(&mut (&mut e).take(size), &mut f).map_err(|e| OnlineError::Archive(format!("{}: {e}", rel.display())))?;
            if copied != size {
                return Err(OnlineError::Archive(format!("{}: truncated", rel.display())));
            }
            st.files += 1;
        } else {
            return Err(OnlineError::Unsafe(format!("{}: {:?} entries are not allowed", rel.display(), kind)));
        }
    }
    Ok(st)
}

/// `O_NOFOLLOW` (the archive never creates links, this only makes sure).
fn libc_nofollow() -> i32 {
    #[cfg(target_os = "linux")]
    {
        0o400000
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// What [`validate`] accepted.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Validated {
    /// Entries that load.
    pub apps: Vec<String>,
    /// Entries that need a newer rynas and do not load in this one: left out.
    pub skipped: Vec<String>,
}

/// Checks an unpacked catalogue against its index before it may replace the one in use:
/// `catalog.toml` names the index's serial, the folders are exactly the listed apps, every
/// entry loads with [`CatalogApp::load`] (manifest, form, compose lint of every overlay,
/// test render) with the listed version and `min_rynas`. An entry may fail only when it
/// needs a newer rynas than `running`.
pub fn validate(dir: &Path, idx: &Index, running: Version) -> Result<Validated, OnlineError> {
    let bad = |m: String| Err(OnlineError::Invalid(m));
    match read_serial(dir) {
        Ok(Some(s)) if s == idx.serial => {}
        Ok(Some(s)) => return bad(format!("catalog.toml has serial {s}, the index {}", idx.serial)),
        Ok(None) => return bad("no catalog.toml".into()),
        Err(e) => return bad(e),
    }
    let mut dirs = BTreeSet::new();
    for e in fs::read_dir(dir).map_err(|e| io_err("read the catalogue", e))? {
        let e = e.map_err(|e| io_err("read the catalogue", e))?;
        let name = e.file_name().to_string_lossy().into_owned();
        let ft = e.file_type().map_err(|e| io_err("read the catalogue", e))?;
        if ft.is_dir() {
            dirs.insert(name);
        } else if name != "catalog.toml" {
            return bad(format!("unexpected file `{name}` at the top"));
        }
    }
    let listed: BTreeMap<&str, &IndexApp> = idx.apps.iter().map(|a| (a.id.as_str(), a)).collect();
    let listed_ids: BTreeSet<String> = listed.keys().map(|s| (*s).to_owned()).collect();
    if dirs != listed_ids {
        let extra: Vec<_> = dirs.difference(&listed_ids).cloned().collect();
        let missing: Vec<_> = listed_ids.difference(&dirs).cloned().collect();
        return bad(format!("folders and index differ (not listed: {extra:?}, missing: {missing:?})"));
    }
    let mut out = Validated::default();
    for (id, entry) in listed {
        match CatalogApp::load(&dir.join(id)) {
            Ok(app) => {
                if app.manifest.version != entry.version || app.manifest.min_rynas != entry.min_rynas {
                    return bad(format!(
                        "{id}: app.toml has {} (min_rynas {}), the index {} (min_rynas {})",
                        app.manifest.version, app.manifest.min_rynas, entry.version, entry.min_rynas
                    ));
                }
                out.apps.push(id.to_owned());
            }
            Err(_) if entry.min_rynas > running => out.skipped.push(id.to_owned()),
            Err(issues) => {
                let first: Vec<String> = issues.0.iter().take(3).map(|i| format!("{}: {}", i.file, i.message)).collect();
                return bad(format!("{id}: {}", first.join("; ")));
            }
        }
    }
    if out.apps.is_empty() {
        return bad("no entry loads in this rynas".into());
    }
    Ok(out)
}

/// Signs test indexes with a key made in the test (minisign format, prehashed). Never used
/// with a real key.
#[cfg(feature = "test-signing")]
pub mod test_signer {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use blake2::Digest as _;
    use ed25519_dalek::{Signer as _, SigningKey};

    /// A key pair from a seed, with a key id.
    pub struct TestKey {
        key: SigningKey,
        id: [u8; 8],
    }

    impl TestKey {
        pub fn new(seed: u8) -> Self {
            Self { key: SigningKey::from_bytes(&[seed; 32]), id: [seed, 1, 2, 3, 4, 5, 6, 7] }
        }

        /// The public key as minisign's base64 line.
        pub fn public(&self) -> String {
            let mut b = b"Ed".to_vec();
            b.extend_from_slice(&self.id);
            b.extend_from_slice(self.key.verifying_key().as_bytes());
            STANDARD.encode(b)
        }

        /// A `.minisig` of `data` with `trusted` as trusted comment.
        pub fn sign(&self, data: &[u8], trusted: &str) -> String {
            let hash = blake2::Blake2b512::digest(data);
            let sig = self.key.sign(&hash);
            let mut bin = b"ED".to_vec();
            bin.extend_from_slice(&self.id);
            bin.extend_from_slice(&sig.to_bytes());
            let mut global = sig.to_bytes().to_vec();
            global.extend_from_slice(trusted.as_bytes());
            let gsig = self.key.sign(&global);
            format!(
                "untrusted comment: test key\n{}\ntrusted comment: {trusted}\n{}\n",
                STANDARD.encode(bin),
                STANDARD.encode(gsig.to_bytes())
            )
        }
    }

    /// A `.tar.zst` of `(path, content)` files (folders implied by the paths).
    pub fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (p, data) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            tar.append_data(&mut h, p, *data).unwrap();
        }
        let raw = tar.into_inner().unwrap();
        zstd::stream::encode_all(&raw[..], 3).unwrap()
    }

    /// Every file below `dir` as archive input (`prefix/<relative path>`).
    pub fn files_of(dir: &std::path::Path, prefix: &str) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![(dir.to_path_buf(), prefix.to_owned())];
        while let Some((d, p)) = stack.pop() {
            let mut entries: Vec<_> = std::fs::read_dir(&d).unwrap().flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let name = e.file_name().to_string_lossy().into_owned();
                let rel = if p.is_empty() { name } else { format!("{p}/{name}") };
                if e.file_type().unwrap().is_dir() {
                    stack.push((e.path(), rel));
                } else {
                    out.push((rel, std::fs::read(e.path()).unwrap()));
                }
            }
        }
        out
    }
}

#[cfg(all(test, feature = "test-signing"))]
mod tests {
    use super::test_signer::{TestKey, archive};
    use super::*;

    fn now() -> jiff::Timestamp {
        "2026-11-10T12:00:00Z".parse().unwrap()
    }

    fn index(serial: u64, expires: &str) -> Index {
        Index {
            schema: 1,
            serial,
            published: None,
            expires: expires.into(),
            archive: ArchiveRef { url: format!("apps-{serial}.tar.zst"), sha256: "0".repeat(64), size: 10 },
            apps: vec![IndexApp { id: "demo".into(), version: Version::new(1, 0, 0), min_rynas: Version::new(0, 1, 0) }],
        }
    }

    fn signed(k: &TestKey, idx: &Index) -> (Vec<u8>, String) {
        let body = serde_json::to_vec(idx).unwrap();
        let sig = k.sign(&body, &format!("serial={} expires={}", idx.serial, idx.expires));
        (body, sig)
    }

    #[test]
    fn good_signature_verifies() {
        let k = TestKey::new(7);
        let idx = index(5, "2026-12-10T00:00:00Z");
        let (body, sig) = signed(&k, &idx);
        assert_eq!(verify_index(&body, &sig, &[&k.public()]).unwrap(), idx);
        // the next key of a rotation is listed first: still found by its key id
        let next = TestKey::new(9);
        assert!(verify_index(&body, &sig, &[&next.public(), &k.public()]).is_ok());
    }

    #[test]
    fn no_key_refuses() {
        let k = TestKey::new(7);
        let (body, sig) = signed(&k, &index(5, "2026-12-10T00:00:00Z"));
        assert_eq!(verify_index(&body, &sig, &[]), Err(OnlineError::NoKey));
    }

    #[test]
    fn tampered_index_or_signature_fails() {
        let k = TestKey::new(7);
        let (mut body, sig) = signed(&k, &index(5, "2026-12-10T00:00:00Z"));
        let pk = k.public();
        let n = body.len() - 2;
        body[n] ^= 1;
        assert!(matches!(verify_index(&body, &sig, &[&pk]), Err(OnlineError::Signature(_))));
        // a flipped signature byte
        let (body, sig) = signed(&k, &index(5, "2026-12-10T00:00:00Z"));
        let mut lines: Vec<String> = sig.lines().map(String::from).collect();
        let mut raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &lines[1]).unwrap();
        raw[20] ^= 1;
        lines[1] = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw);
        assert!(matches!(verify_index(&body, &lines.join("\n"), &[&pk]), Err(OnlineError::Signature(_))));
        assert!(matches!(verify_index(&body, "garbage", &[&pk]), Err(OnlineError::Signature(_))));
    }

    #[test]
    fn legacy_signature_from_real_minisign_is_refused() {
        // minisign-verify's test vector: a real minisign key and a legacy (not prehashed)
        // signature of "test"; only prehashed signatures (`minisign -S`, the default) count
        let pk = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
        let sig = "untrusted comment: signature from minisign secret key\nRWQf6LRCGA9i59SLOFxz6NxvASXDJeRtuZykwQepbDEGt87ig1BNpWaVWuNrm73YiIiJbq71Wi+dP9eKL8OC351vwIasSSbXxwA=\ntrusted comment: timestamp:1555779966\tfile:test\nQtKMXWyYcwdpZAlPF7tE2ENJkRd1ujvKjlj1m9RtHTBnZPa5WKU5uWRs5GoP5M/VqE81QFuMKI5k/SfNQUaOAA==\n";
        assert!(matches!(verify_index(b"test", sig, &[pk]), Err(OnlineError::Signature(_))));
    }

    #[test]
    fn wrong_key_is_unknown() {
        let k = TestKey::new(7);
        let (body, sig) = signed(&k, &index(5, "2026-12-10T00:00:00Z"));
        assert_eq!(verify_index(&body, &sig, &[&TestKey::new(8).public()]), Err(OnlineError::UnknownKey));
    }

    #[test]
    fn trusted_comment_is_signed_and_must_match() {
        let k = TestKey::new(7);
        let idx = index(5, "2026-12-10T00:00:00Z");
        let (body, sig) = signed(&k, &idx);
        // an edited trusted comment breaks the global signature
        let edited = sig.replace("serial=5", "serial=6");
        assert!(matches!(verify_index(&body, &edited, &[&k.public()]), Err(OnlineError::Signature(_))));
        // a validly signed comment that names another serial or expiry is refused
        let other = k.sign(&body, "serial=4 expires=2026-12-10T00:00:00Z");
        assert!(matches!(verify_index(&body, &other, &[&k.public()]), Err(OnlineError::Comment(_))));
        let other = k.sign(&body, "serial=5 expires=2027-12-10T00:00:00Z");
        assert!(matches!(verify_index(&body, &other, &[&k.public()]), Err(OnlineError::Comment(_))));
        let other = k.sign(&body, "timestamp:1 file:index.json");
        assert!(matches!(verify_index(&body, &other, &[&k.public()]), Err(OnlineError::Comment(_))));
    }

    #[test]
    fn index_content_is_checked() {
        let k = TestKey::new(7);
        let mut idx = index(5, "2026-12-10T00:00:00Z");
        idx.schema = 2;
        let (body, sig) = signed(&k, &idx);
        assert!(matches!(verify_index(&body, &sig, &[&k.public()]), Err(OnlineError::Index(_))));
        let mut idx = index(5, "2026-12-10T00:00:00Z");
        idx.apps.push(idx.apps[0].clone());
        let (body, sig) = signed(&k, &idx);
        assert!(matches!(verify_index(&body, &sig, &[&k.public()]), Err(OnlineError::Index(_))));
        let mut idx = index(5, "2026-12-10T00:00:00Z");
        idx.archive.size = MAX_ARCHIVE + 1;
        let (body, sig) = signed(&k, &idx);
        assert!(matches!(verify_index(&body, &sig, &[&k.public()]), Err(OnlineError::Index(_))));
    }

    #[test]
    fn serial_and_expiry_rules() {
        let idx = index(5, "2026-12-10T00:00:00Z");
        assert_eq!(check_fresh(&idx, now(), None), Ok(Fresh::New));
        assert_eq!(check_fresh(&idx, now(), Some(4)), Ok(Fresh::New));
        assert_eq!(check_fresh(&idx, now(), Some(5)), Ok(Fresh::Same));
        assert_eq!(check_fresh(&idx, now(), Some(6)), Err(OnlineError::Rollback { serial: 5, last: 6 }));
        let old = index(9, "2026-11-01T00:00:00Z");
        assert!(matches!(check_fresh(&old, now(), Some(1)), Err(OnlineError::Expired(_))));
        let forever = index(9, "2030-01-01T00:00:00Z");
        assert!(matches!(check_fresh(&forever, now(), None), Err(OnlineError::Index(_))));
    }

    #[test]
    fn archive_size_and_hash() {
        let data = b"catalogue".to_vec();
        let a = ArchiveRef { url: "a.tar.zst".into(), sha256: sha256_hex(&data), size: data.len() as u64 };
        assert!(check_archive(&data, &a).is_ok());
        assert!(matches!(check_archive(b"catalogu", &a), Err(OnlineError::Archive(_))));
        assert!(matches!(check_archive(b"Catalogue", &a), Err(OnlineError::Archive(_))));
    }

    #[test]
    fn archive_urls() {
        let a = |u: &str| ArchiveRef { url: u.into(), sha256: String::new(), size: 1 };
        let base = "https://example.org/cat/v1/index.json";
        assert_eq!(archive_url(base, &a("apps-3.tar.zst"), false).unwrap(), "https://example.org/cat/v1/apps-3.tar.zst");
        assert_eq!(archive_url(base, &a("https://cdn.example.org/a.tar.zst"), false).unwrap(), "https://cdn.example.org/a.tar.zst");
        for bad in ["http://example.org/a", "../a.tar.zst", "a/b.tar.zst", ".hidden", "file:///etc/passwd", "https://a b"] {
            assert!(archive_url(base, &a(bad), false).is_err(), "{bad}");
        }
        assert!(archive_url(base, &a("http://127.0.0.1/a"), true).is_ok());
    }

    /// A raw tar with one entry whose name and type the test chooses (no checks).
    fn raw_entry(name: &[u8], kind: tar::EntryType, link: Option<&str>, data: &[u8]) -> Vec<u8> {
        let mut h = tar::Header::new_gnu();
        {
            let g = h.as_gnu_mut().unwrap();
            g.name[..name.len()].copy_from_slice(name);
        }
        h.set_entry_type(kind);
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        if let Some(l) = link {
            h.set_link_name(l).unwrap();
        }
        h.set_cksum();
        let mut out = h.as_bytes().to_vec();
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
        out.extend_from_slice(&[0u8; 1024]);
        zstd::stream::encode_all(&out[..], 1).unwrap()
    }

    #[test]
    fn extraction_writes_plain_files() {
        let dir = tempfile::tempdir().unwrap();
        let ar = archive(&[("catalog.toml", b"serial = 3\n"), ("demo/app.toml", b"x"), ("demo/metadata/icon.svg", b"<svg/>")]);
        let st = extract(&ar, &dir.path().join("out")).unwrap();
        assert_eq!(st.files, 3);
        assert_eq!(fs::read(dir.path().join("out/demo/metadata/icon.svg")).unwrap(), b"<svg/>");
        let mode = fs::metadata(dir.path().join("out/demo/app.toml")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        // the destination must be new
        assert!(extract(&ar, &dir.path().join("out")).is_err());
    }

    #[test]
    fn extraction_refuses_traversal_links_and_devices() {
        let dir = tempfile::tempdir().unwrap();
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("dotdot", raw_entry(b"../evil", tar::EntryType::Regular, None, b"x")),
            ("nested dotdot", raw_entry(b"demo/../../evil", tar::EntryType::Regular, None, b"x")),
            ("absolute", raw_entry(b"/etc/evil", tar::EntryType::Regular, None, b"x")),
            ("symlink", raw_entry(b"demo/link", tar::EntryType::Symlink, Some("/etc/shadow"), b"")),
            ("hardlink", raw_entry(b"demo/hard", tar::EntryType::Link, Some("/etc/shadow"), b"")),
            ("char device", raw_entry(b"demo/null", tar::EntryType::Char, None, b"")),
            ("fifo", raw_entry(b"demo/fifo", tar::EntryType::Fifo, None, b"")),
            ("too deep", raw_entry(b"a/b/c/d/e/f/g", tar::EntryType::Regular, None, b"x")),
        ];
        for (i, (what, ar)) in cases.into_iter().enumerate() {
            let out = dir.path().join(format!("o{i}"));
            let r = extract(&ar, &out);
            assert!(matches!(r, Err(OnlineError::Unsafe(_))), "{what}: {r:?}");
        }
        assert!(!dir.path().join("evil").exists());
        assert!(!Path::new("/etc/evil").exists());
    }

    #[test]
    fn extraction_refuses_duplicates_and_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let ar = archive(&[("a/x", b"1"), ("a/x", b"2")]);
        assert!(matches!(extract(&ar, &dir.path().join("dup")), Err(OnlineError::Unsafe(_))));
        assert!(matches!(extract(b"not zstd", &dir.path().join("g")), Err(OnlineError::Archive(_))));
    }
}
