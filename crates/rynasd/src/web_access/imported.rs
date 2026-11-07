//! Imported certificates (§5.5 of `docs/phase2/docker-apps.md`): certificates made elsewhere
//! (a router's ACME client, a company CA, a bought wildcard) served by rynas-caddy for the
//! names they cover.
//!
//! - **Store.** rynasd's private folder `<state>/web-access/imported/` (0700 root):
//!   `index.json` (ids, labels, sources, pinned names, problems) and `<id>.crt` / `<id>.key`
//!   (0600), the last good pair. Never in the configuration key: the key is not in
//!   `config.db`, its revisions or a passphrase-less `.rynasbak`. Every read and write of the
//!   store holds [`WebAccessSvc`]'s `imports` lock; a render reads each pair once into
//!   [`Active`] and writes the engine copies from that snapshot (review 2026-11-07, M3).
//! - **Watched folder.** `/var/lib/rynas-cert-drop/<id>/` (`2770 root:rynas-cert-drop` in a
//!   base `0750 root:rynas-cert-drop`, usable as an sshd `ChrootDirectory`): another tool
//!   writes `fullchain.pem` and `privkey.pem` there. rynasd polls the two files every
//!   [`POLL`] (a `stat` through descriptors), waits until they have stayed unchanged for
//!   [`SETTLE`] (acme.sh's ssh deploy rewrites the files in place with `echo >`, one after the
//!   other, so a rename-based trigger alone would catch half-written pairs), then validates.
//!   Files are opened below the folder without following symlinks, regular files with one
//!   link only, at most [`MAX_PEM`] bytes; a file that changed since it settled is read again
//!   later. A good pair replaces the stored one and reloads the proxy; a bad one leaves
//!   everything as it is and raises `web_access.cert_import_failed` (a key that does not
//!   match the chain, or a missing file, only when it is still so one settle window later:
//!   the writer may be between the two files).
//! - **Pinned names** (review 2026-11-07, M1). Everyone in the group can write every watched
//!   folder, so a pushed pair may only renew the names the administrator agreed to: the
//!   DNS-name set given at creation, or that of the first good pair, or of an upload. A
//!   pushed pair for another set is held (`problem: names_changed`, `pending_names`,
//!   `web_access.cert_import_names_changed`) until `POST .../{id}/accept`.
//! - **Not valid yet** (L1). A pair is served from `not_before - `[`SKEW_MS`]; a pushed pair
//!   that starts later does not replace a stored pair that still serves: it waits in the
//!   folder (`problem: not_yet_valid`) and is taken once it starts.
//! - **Engine files.** Each valid pair is copied to `rynas/import-<id>.{crt,key}` in the
//!   engine's state folder (0640 root:rynas-caddy, key first), next to the local leaf; the
//!   engine never reads the watched folder.
//! - **Precedence.** A valid imported certificate takes over every served name its DNS names
//!   cover (exact, or a one-label wildcard, RFC 6125 §6.4.3): through a TLS connection policy
//!   per certificate (`match.sni` + `certificate_selection.any_tag`), so the choice never
//!   depends on certmagic's cache order; ACME subjects it covers are not ordered until it
//!   `expires_soon` (L2: then they are ordered while the policy keeps serving the import, so
//!   the switch at expiry finds the ACME certificate ready). Between two imports, an exact
//!   name beats a wildcard, then the later expiry, then the id. An expired import is dropped
//!   from the render: its names go back to ACME or the local CA.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use rynas_api_types::error_codes;
use rynas_api_types::notify::{NewEvent, NotifyCategory, NotifySeverity};
use rynas_api_types::tls::{TlsCertificate, TlsWarning};
use rynas_api_types::web_access::{
    CertImportRequest, CertStatus, ImportProblem, ImportSource, ImportedCert, ProxyMode,
    WebAccessConfig,
};
use rynas_docker::safefs;
use rynas_notify::event::{code, field, firing, resolved};
use rynas_tls::PemPair;
use serde::{Deserialize, Serialize};

use super::{WebAccessSvc, certs, domains, group_gid, resolve};
use crate::ops::{Error, Res};

/// The base of the watched folders (one per `source: folder` import).
pub const DROP_DIR: &str = "/var/lib/rynas-cert-drop";
/// Group whose members may write into the watched folders.
pub const DROP_GROUP: &str = "rynas-cert-drop";
pub const FULLCHAIN: &str = "fullchain.pem";
pub const PRIVKEY: &str = "privkey.pem";
/// Largest PEM file read from a watched folder.
pub const MAX_PEM: u64 = 64 * 1024;
/// How often the watched folders are looked at.
pub const POLL: Duration = Duration::from_secs(2);
/// How long both files must stay unchanged before they are read.
pub const SETTLE: Duration = Duration::from_secs(3);
/// Clock skew allowed on a pair's start (`not_before`).
pub const SKEW_MS: i64 = 5 * 60_000;
/// Prefix of the engine's copies (`rynas/import-<id>.crt`) and of the Caddy tags.
pub const ENGINE_PREFIX: &str = "import-";
pub const TAG_PREFIX: &str = "rynas-import-";
/// Most DNS names in `CertImportRequest::names`.
const MAX_NAMES: usize = 100;
/// `rynas_tls`'s refusal of a key that is not the chain's (a writer between the two files).
const KEY_MISMATCH: &str = "the private key does not belong to the certificate";

/// What `index.json` keeps per import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Meta {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub source: ImportSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_at_ms: Option<i64>,
    /// Pinned DNS names (lowercase, sorted, unique); empty until the first good pair.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    /// Names of a held pair (`problem: names_changed`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_names: Vec<String>,
    /// Start of a held pair (`problem: not_yet_valid`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_from_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<ImportProblem>,
}

impl Meta {
    /// A problem that fired a notification (resolved by `web_access.cert_import_ok`).
    fn fired(&self) -> bool {
        matches!(
            self.problem,
            Some(ImportProblem::Refused | ImportProblem::NamesChanged)
        )
    }

    fn clear_problem(&mut self) {
        self.problem = None;
        self.last_error = None;
        self.last_error_at_ms = None;
        self.pending_names.clear();
        self.pending_from_ms = None;
    }
}

/// A valid import as the renderer needs it, with its pair read once under the lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Active {
    pub id: String,
    /// DNS names of the leaf, lowercased.
    pub dns: Vec<String>,
    pub not_after_ms: i64,
    /// It `expires_soon`: the ACME subjects it covers are ordered again (it keeps serving
    /// its names until it expires).
    pub renew_acme: bool,
    /// The engine's copies (absolute engine paths).
    pub cert: String,
    pub key: String,
    /// The stored pair (`Debug` redacts the key).
    pub pair: PemPair,
}

impl Active {
    pub fn tag(&self) -> String {
        format!("{TAG_PREFIX}{}", self.id)
    }
}

/// `stat` of both files: inode, size, mtime and ctime (ns). `None` entries: missing.
pub type Sig = [Option<FileSig>; 2];
/// `stat` of one file: inode, size, mtime and ctime (ns).
pub type FileSig = (u64, u64, i64, i64);

/// Poll state of one watched folder.
#[derive(Debug, Default)]
pub struct WatchState {
    seen: Option<Sig>,
    since: Option<Instant>,
    /// The signature last read to an outcome (taken, refused, held); `None` after an I/O
    /// error, so the same files are read again.
    done: Option<Sig>,
    /// A held pair that starts later: read again from then (ms since the epoch).
    wait_until_ms: Option<i64>,
    /// Files refused for a reason that may be a writer between the two files (key of
    /// another chain, one file missing): refused only when still so a window later.
    suspect: Option<Sig>,
}

/// What the import code remembers between calls (behind [`WebAccessSvc`]'s `imports`).
#[derive(Debug, Default)]
pub struct ImportMem {
    pub watch: BTreeMap<String, WatchState>,
}

/// What reading a watched folder led to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Took {
    /// A new pair is stored.
    Changed,
    /// The same pair as stored (or no such import).
    Same,
    /// Refused, recorded and notified.
    Refused,
    /// Valid, but for other names than the pinned ones.
    Held,
    /// Valid, but it starts at this time while the stored pair still serves.
    Deferred(i64),
    /// The files changed while they were read: settle again.
    Moved,
    /// Possibly half written: confirm one window later.
    Suspect,
}

/// Why the files of a watched folder were not taken.
enum DropErr {
    Moved,
    Bad { msg: String, transient: bool },
}

fn bad_drop(msg: String, transient: bool) -> DropErr {
    DropErr::Bad { msg, transient }
}

// ---------------------------------------------------------------------------------------------
// pure rules
// ---------------------------------------------------------------------------------------------

/// Ids name a folder, two files and a tag: `[a-z0-9-]` 1-32, not starting with `-`.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A DNS name as a certificate carries it: labels of `[a-z0-9-]` (1-63, ASCII
/// case-insensitive, A-labels for IDN), at least two, optionally `*.` in front; ≤ 253.
pub fn valid_dns_name(n: &str) -> bool {
    let rest = n.strip_prefix("*.").unwrap_or(n);
    n.len() <= 253
        && rest.contains('.')
        && rest.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// The comparable set of DNS names: lowercase, sorted, unique.
pub fn name_set<S: AsRef<str>>(names: &[S]) -> Vec<String> {
    let set: BTreeSet<String> = names
        .iter()
        .map(|n| n.as_ref().to_ascii_lowercase())
        .collect();
    set.into_iter().collect()
}

/// A certificate DNS name covers `name`: equal (ASCII case-insensitive), or `*.<rest>` with
/// `name` = one label + `.<rest>` (the wildcard is the whole left-most label and matches
/// exactly one label; RFC 6125 §6.4.3, CA/Browser Forum BR 3.2.2.6).
pub fn covers(san: &str, name: &str) -> bool {
    let (san, name) = (san.to_ascii_lowercase(), name.to_ascii_lowercase());
    if san == name {
        return true;
    }
    let Some(rest) = san.strip_prefix("*.") else {
        return false;
    };
    if rest.contains('*') || !rest.contains('.') {
        return false;
    }
    match name.split_once('.') {
        Some((first, tail)) => !first.is_empty() && first != "*" && tail == rest,
        None => false,
    }
}

/// Which import serves each name of `names` (only names some import covers): an exact DNS
/// name beats a wildcard, then the later expiry, then the smaller id.
pub fn assign(active: &[Active], names: &[String]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for n in names {
        let best = active
            .iter()
            .filter_map(|a| {
                let exact = a.dns.iter().any(|d| d.eq_ignore_ascii_case(n));
                let any = exact || a.dns.iter().any(|d| covers(d, n));
                any.then_some((exact, a.not_after_ms, std::cmp::Reverse(a.id.as_str())))
            })
            .max();
        if let Some((_, _, std::cmp::Reverse(id))) = best {
            out.insert(n.clone(), id.to_string());
        }
    }
    out
}

/// An ACME subject an import replaces: a wildcard subject only by the same wildcard (a list
/// of names does not cover every name below), a plain one by any covering DNS name. An
/// import that expires soon replaces nothing (its subjects are ordered ahead of its expiry).
pub fn replaces_subject(a: &Active, subject: &str) -> bool {
    if a.renew_acme {
        return false;
    }
    if subject.starts_with("*.") {
        a.dns.iter().any(|d| d.eq_ignore_ascii_case(subject))
    } else {
        a.dns.iter().any(|d| covers(d, subject))
    }
}

/// The ACME subjects of `cfg`'s domains that no valid import replaces.
pub fn acme_subjects(
    cfg: &WebAccessConfig,
    d: &rynas_api_types::web_access::WebDomain,
    active: &[Active],
) -> Vec<String> {
    domains::cert_subjects(cfg, d)
        .into_iter()
        .filter(|s| !active.iter().any(|a| replaces_subject(a, s)))
        .collect()
}

/// Checks a pair: the validation of `/system/tls/import` plus at least one DNS name (an
/// address-only certificate cannot take over a served name).
pub fn validate(chain: &str, key: &str, now: i64) -> Result<rynas_tls::Imported, (String, String)> {
    let imp = rynas_tls::validate_import(chain, key, now)
        .map_err(|e| (e.field.to_string(), e.message))?;
    if imp.chain[0].dns_names.is_empty() {
        return Err((
            "certificate".into(),
            "the certificate has no DNS names (Subject Alternative Name): it cannot serve a host name".into(),
        ));
    }
    Ok(imp)
}

/// Status of a stored leaf.
pub fn status_of(c: &TlsCertificate, now: i64) -> CertStatus {
    certs::status_of(c, false, now)
}

/// The leaf is served at `now`: not expired, started (with [`SKEW_MS`]).
pub fn serves(c: &TlsCertificate, now: i64) -> bool {
    c.not_after_ms > now && c.not_before_ms <= now + SKEW_MS
}

// ---------------------------------------------------------------------------------------------
// notifications
// ---------------------------------------------------------------------------------------------

const SRC: &str = "web-access";

fn display(m: &Meta) -> String {
    if m.label.is_empty() {
        m.id.clone()
    } else {
        m.label.clone()
    }
}

/// Files in a watched folder were refused.
pub fn failed_event(m: &Meta, err: &str) -> NewEvent {
    let ev = firing(
        NotifyCategory::System,
        NotifySeverity::Warning,
        SRC,
        format!("web-access:import:{}", m.id),
        format!("The certificate files for {} were refused", display(m)),
    );
    let mut ev = code(ev, "web_access.cert_import_failed");
    ev.body = format!(
        "The previous certificate stays in use. {err}. Folder: {DROP_DIR}/{}",
        m.id
    );
    let ev = field(ev, "name", display(m));
    let ev = field(ev, "cert", format!("imported:{}", m.id));
    field(ev, "error", err)
}

/// A watched folder holds a valid pair for other names than the pinned ones (M1).
pub fn names_event(m: &Meta, names: &[String]) -> NewEvent {
    let ev = firing(
        NotifyCategory::System,
        NotifySeverity::Warning,
        SRC,
        format!("web-access:import:{}", m.id),
        format!(
            "The certificate pushed for {} is for other names",
            display(m)
        ),
    );
    let mut ev = code(ev, "web_access.cert_import_names_changed");
    ev.body = format!(
        "It is for {} instead of {}. The previous certificate stays in use until an administrator accepts the new names.",
        names.join(", "),
        m.names.join(", ")
    );
    let ev = field(ev, "name", display(m));
    let ev = field(ev, "cert", format!("imported:{}", m.id));
    field(ev, "names", names.join(", "))
}

/// A good pair after refused or held files, or the import removed or replaced.
pub fn ok_event(m: &Meta) -> NewEvent {
    let ev = resolved(
        NotifyCategory::System,
        SRC,
        format!("web-access:import:{}", m.id),
        format!("The certificate files for {} were taken", display(m)),
    );
    let ev = code(ev, "web_access.cert_import_ok");
    let ev = field(ev, "name", display(m));
    field(ev, "cert", format!("imported:{}", m.id))
}

// ---------------------------------------------------------------------------------------------
// the service side
// ---------------------------------------------------------------------------------------------

fn bad(field: &str, m: impl Into<String>) -> Error {
    Error::invalid(field, m.into())
}

fn conflict(msg: String) -> Error {
    Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, msg)
}

fn sig_of(f: &std::fs::File) -> io::Result<FileSig> {
    let m = f.metadata()?;
    Ok((
        m.ino(),
        m.size(),
        m.mtime() * 1_000_000_000 + m.mtime_nsec(),
        m.ctime() * 1_000_000_000 + m.ctime_nsec(),
    ))
}

/// Reads one file of a watched folder: a regular file (no symlink, no FIFO, no device) with
/// one link (a hard link would let a group member hand over a file it cannot read, L5), at
/// most [`MAX_PEM`] bytes. Returns the text and the signature after the read.
fn read_one(d: &std::fs::File, n: &str) -> Result<(String, FileSig), DropErr> {
    let f = safefs::open_file_at(d, n).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => bad_drop(format!("{n} is missing"), true),
        io::ErrorKind::InvalidInput => bad_drop(format!("{n} is not a regular file"), false),
        _ if e.raw_os_error() == Some(libc::ELOOP) => {
            bad_drop(format!("{n} is a symbolic link"), false)
        }
        _ => bad_drop(format!("{n} cannot be opened: {e}"), false),
    })?;
    let m = f
        .metadata()
        .map_err(|e| bad_drop(format!("{n} cannot be read: {e}"), false))?;
    if m.nlink() > 1 {
        return Err(bad_drop(
            format!("{n} has other hard links: write it as a file of its own"),
            false,
        ));
    }
    if m.size() > MAX_PEM {
        return Err(bad_drop(
            format!("{n} is larger than {} KiB", MAX_PEM / 1024),
            false,
        ));
    }
    let mut s = String::new();
    (&f).take(MAX_PEM + 1)
        .read_to_string(&mut s)
        .map_err(|e| bad_drop(format!("{n} cannot be read: {e}"), false))?;
    if s.len() as u64 > MAX_PEM {
        return Err(bad_drop(
            format!("{n} is larger than {} KiB", MAX_PEM / 1024),
            false,
        ));
    }
    let sig = sig_of(&f).map_err(|e| bad_drop(format!("{n} cannot be read: {e}"), false))?;
    Ok((s, sig))
}

impl WebAccessSvc {
    fn import_dir(&self) -> PathBuf {
        self.state_dir.join("web-access/imported")
    }

    fn drop_base(&self) -> PathBuf {
        self.path(DROP_DIR)
    }

    fn lock_imports(&self) -> std::sync::MutexGuard<'_, ImportMem> {
        self.imports.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The import index (empty when missing or unreadable).
    pub fn import_index(&self) -> Vec<Meta> {
        std::fs::read_to_string(self.import_dir().join("index.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn find_meta(&self, id: &str) -> Res<Meta> {
        self.import_index()
            .into_iter()
            .find(|m| m.id == id)
            .ok_or_else(|| Error::not_found(format!("no imported certificate {id}")))
    }

    fn store_dir(&self) -> io::Result<PathBuf> {
        let d = self.import_dir();
        std::fs::create_dir_all(&d)?;
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700))?;
        Ok(d)
    }

    fn save_index(&self, idx: &[Meta]) -> io::Result<()> {
        let d = self.store_dir()?;
        let mut text = serde_json::to_vec_pretty(idx).map_err(io::Error::other)?;
        text.push(b'\n');
        crate::tls::write_atomic(&d.join("index.json"), &text, 0o600, None)
    }

    /// The stored pair (callers hold the `imports` lock).
    fn read_stored(&self, id: &str) -> Option<PemPair> {
        let d = self.import_dir();
        let chain = std::fs::read_to_string(d.join(format!("{id}.crt"))).ok()?;
        let key = std::fs::read_to_string(d.join(format!("{id}.key"))).ok()?;
        Some(PemPair { chain, key })
    }

    /// The stored chain (callers hold the `imports` lock).
    fn leaf_of(&self, id: &str) -> Option<Vec<TlsCertificate>> {
        let chain = std::fs::read_to_string(self.import_dir().join(format!("{id}.crt"))).ok()?;
        rynas_tls::inspect(&chain).ok()
    }

    /// The stored chain of `id`, leaf first.
    pub(super) fn stored_leaf(&self, id: &str) -> Option<Vec<TlsCertificate>> {
        let _g = self.lock_imports();
        self.leaf_of(id)
    }

    fn write_stored(&self, id: &str, p: &PemPair) -> io::Result<()> {
        let d = self.store_dir()?;
        crate::tls::write_atomic(&d.join(format!("{id}.key")), p.key.as_bytes(), 0o600, None)?;
        crate::tls::write_atomic(
            &d.join(format!("{id}.crt")),
            p.chain.as_bytes(),
            0o600,
            None,
        )
    }

    fn update_meta(&self, id: &str, f: impl FnOnce(&mut Meta)) -> io::Result<Option<Meta>> {
        let mut idx = self.import_index();
        let Some(m) = idx.iter_mut().find(|m| m.id == id) else {
            return Ok(None);
        };
        f(m);
        let out = m.clone();
        self.save_index(&idx)?;
        Ok(Some(out))
    }

    fn emit(&self, ev: NewEvent) {
        if let Some(n) = self.notifier.get() {
            n.emit(ev);
        }
    }

    /// The valid (not expired, already started) imports, each pair read once under the
    /// `imports` lock: the render and the engine copies come from this snapshot.
    pub fn active_imports(&self, now: i64) -> Vec<Active> {
        let _g = self.lock_imports();
        self.load_active(now)
    }

    fn load_active(&self, now: i64) -> Vec<Active> {
        let dir = Path::new(&self.paths.cert)
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        self.import_index()
            .iter()
            .filter_map(|m| {
                let pair = self.read_stored(&m.id)?;
                let leaf = rynas_tls::inspect(&pair.chain).ok()?.into_iter().next()?;
                if !serves(&leaf, now) || rynas_tls::certified_key(&pair).is_err() {
                    return None;
                }
                Some(Active {
                    id: m.id.clone(),
                    dns: leaf
                        .dns_names
                        .iter()
                        .map(|d| d.to_ascii_lowercase())
                        .collect(),
                    not_after_ms: leaf.not_after_ms,
                    renew_acme: leaf.not_after_ms - now < certs::SOON_MS,
                    cert: format!("{dir}/{ENGINE_PREFIX}{}.crt", m.id),
                    key: format!("{dir}/{ENGINE_PREFIX}{}.key", m.id),
                    pair,
                })
            })
            .collect()
    }

    /// Copies the valid pairs (from the snapshot `active`) into the engine's `rynas/` folder
    /// and removes copies no longer used. Returns whether a file changed (the load must
    /// re-read the same paths).
    pub(super) fn write_import_files(&self, active: &[Active]) -> io::Result<bool> {
        let gid = group_gid(&self.root, super::ENGINE_GROUP);
        let dir = if active.is_empty() {
            match self.existing_rynas_dir()? {
                Some(d) => d,
                None => return Ok(false),
            }
        } else {
            self.rynas_dir(gid)?
        };
        let owner = gid.map(|g| (0, g));
        let mut changed = false;
        let mut keep = BTreeSet::new();
        for a in active {
            for (ext, data) in [("key", &a.pair.key), ("crt", &a.pair.chain)] {
                let name = format!("{ENGINE_PREFIX}{}.{ext}", a.id);
                keep.insert(name.clone());
                if safefs::read_at(&dir, &name, MAX_PEM * 4).is_ok_and(|old| &old == data)
                    && safefs::mode_at(&dir, &name) == Some(0o640)
                {
                    continue;
                }
                safefs::write_at(&dir, &name, data.as_bytes(), 0o640, owner)?;
                changed = true;
            }
        }
        for name in safefs::list_at(&dir)? {
            if name.starts_with(ENGINE_PREFIX)
                && (name.ends_with(".crt") || name.ends_with(".key"))
                && !keep.contains(&name)
            {
                safefs::remove_tree_at(&dir, &name)?;
                changed = true;
            }
        }
        Ok(changed)
    }

    /// Whether the watched folders' group must exist (as root; tests may ask for it).
    fn drop_group_needed(&self) -> bool {
        *self
            .drop_group_check
            .get()
            .unwrap_or(&(super::euid() == 0))
    }

    fn drop_group_missing(&self) -> bool {
        self.drop_group_needed() && group_gid(&self.root, DROP_GROUP).is_none()
    }

    /// The watched folder of `id`, made when missing; owner and mode put back when they
    /// drifted (the base must stay non-writable for the group: an sshd chroot refuses it).
    /// Refused when the group is missing (L7): the folder would be root's only, and the tool
    /// pushing into it would fail without anyone being told.
    fn ensure_drop(&self, id: &str) -> Res<()> {
        let gid = group_gid(&self.root, DROP_GROUP);
        if gid.is_none() && self.drop_group_needed() {
            let m = format!(
                "the group {DROP_GROUP} is missing on this system, so no account could write into the watched folder: reinstall the rynas package or run `addgroup --system {DROP_GROUP}`"
            );
            tracing::error!("web access: imported certificate {id}: {m}");
            return Err(conflict(m).field("source"));
        }
        let owner = gid.map(|g| (0, g));
        let made = || -> io::Result<()> {
            let base = self.drop_base();
            let parent = base.parent().expect("has a parent");
            std::fs::create_dir_all(parent)?;
            let start = safefs::open_start(parent)?;
            let name = base
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("rynas-cert-drop");
            let (b, _) = safefs::ensure_dir_at(&start, name, 0o750, owner)?;
            if safefs::stat_of(&b)?.1 != super::euid() {
                return Err(io::Error::other(format!(
                    "{DROP_DIR} is not rynasd's own folder"
                )));
            }
            safefs::set_owner_mode(&b, 0o750, owner)?;
            let (d, _) = safefs::ensure_dir_at(&b, id, 0o2770, owner)?;
            safefs::set_owner_mode(&d, 0o2770, owner)
        };
        made().map_err(Error::internal)
    }

    fn remove_drop(&self, id: &str) -> io::Result<()> {
        match safefs::open_start(&self.drop_base()) {
            Ok(b) => safefs::remove_tree_at(&b, id),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Opens the watched folder of `id` (no symlink below the base).
    fn open_drop(&self, id: &str) -> io::Result<std::fs::File> {
        let b = safefs::open_start(&self.drop_base())?;
        safefs::open_dir_at(&b, id)
    }

    fn drop_sig(&self, id: &str) -> Option<Sig> {
        let d = self.open_drop(id).ok()?;
        let one = |n: &str| {
            safefs::open_file_at(&d, n)
                .ok()
                .and_then(|f| sig_of(&f).ok())
        };
        Some([one(FULLCHAIN), one(PRIVKEY)])
    }

    /// Reads and validates the pair in the watched folder. With `expect`, files that no
    /// longer match the settled signature are [`DropErr::Moved`] (L4).
    fn read_drop(
        &self,
        id: &str,
        now: i64,
        expect: Option<Sig>,
    ) -> Result<rynas_tls::Imported, DropErr> {
        let d = self
            .open_drop(id)
            .map_err(|e| bad_drop(format!("the folder cannot be opened: {e}"), false))?;
        let (chain, s1) = read_one(&d, FULLCHAIN)?;
        let (key, s2) = read_one(&d, PRIVKEY)?;
        if expect.is_some_and(|e| e != [Some(s1), Some(s2)]) {
            return Err(DropErr::Moved);
        }
        validate(&chain, &key, now).map_err(|(f, m)| {
            let transient = m == KEY_MISMATCH;
            let file = if f == "private_key" {
                PRIVKEY
            } else {
                FULLCHAIN
            };
            bad_drop(format!("{file}: {m}"), transient)
        })
    }

    /// Records and notifies (once per distinct error) refused files.
    fn refuse(&self, meta: &Meta, err: String, now: i64) -> io::Result<()> {
        tracing::warn!("web access: imported certificate {}: {err}", meta.id);
        if meta.problem == Some(ImportProblem::Refused)
            && meta.last_error.as_deref() == Some(err.as_str())
        {
            return Ok(());
        }
        self.update_meta(&meta.id, |m| {
            m.clear_problem();
            m.problem = Some(ImportProblem::Refused);
            m.last_error = Some(err.clone());
            m.last_error_at_ms = Some(now);
        })?;
        self.emit(failed_event(meta, &err));
        Ok(())
    }

    /// Reads the watched folder of `id` (callers hold the `imports` lock): a good pair with
    /// the pinned names that differs from the stored one is taken; other names are held
    /// (M1); a pair that starts later waits while the stored one serves (L1); bad files are
    /// recorded and notified, unless they may be half written and `confirmed` is false.
    fn take_drop(
        &self,
        id: &str,
        now: i64,
        expect: Option<Sig>,
        confirmed: bool,
    ) -> io::Result<Took> {
        let Some(meta) = self.import_index().into_iter().find(|m| m.id == id) else {
            return Ok(Took::Same);
        };
        let imp = match self.read_drop(id, now, expect) {
            Ok(imp) => imp,
            Err(DropErr::Moved) => return Ok(Took::Moved),
            Err(DropErr::Bad { transient: true, .. }) if !confirmed => return Ok(Took::Suspect),
            Err(DropErr::Bad { msg, .. }) => {
                self.refuse(&meta, msg, now)?;
                return Ok(Took::Refused);
            }
        };
        let leaf = &imp.chain[0];
        let names = name_set(&leaf.dns_names);
        if !meta.names.is_empty() && meta.names != names {
            let first =
                meta.problem != Some(ImportProblem::NamesChanged) || meta.pending_names != names;
            if first {
                tracing::warn!(
                    "web access: imported certificate {id}: pushed for {} instead of {}, held until an administrator accepts it",
                    names.join(", "),
                    meta.names.join(", ")
                );
                self.update_meta(id, |m| {
                    m.clear_problem();
                    m.problem = Some(ImportProblem::NamesChanged);
                    m.pending_names = names.clone();
                    m.last_error = Some(format!(
                        "the certificate is for {} instead of {}: accept the new names to use it",
                        names.join(", "),
                        meta.names.join(", ")
                    ));
                    m.last_error_at_ms = Some(now);
                })?;
                self.emit(names_event(&meta, &names));
            }
            return Ok(Took::Held);
        }
        if leaf.not_before_ms > now + SKEW_MS
            && self
                .leaf_of(id)
                .and_then(|c| c.into_iter().next())
                .is_some_and(|c| serves(&c, now))
        {
            let from = leaf.not_before_ms;
            if meta.problem != Some(ImportProblem::NotYetValid) || meta.pending_from_ms != Some(from)
            {
                tracing::info!(
                    "web access: imported certificate {id}: the new pair starts later, the current one stays in use until then"
                );
                let fired = meta.fired();
                self.update_meta(id, |m| {
                    m.clear_problem();
                    m.problem = Some(ImportProblem::NotYetValid);
                    m.pending_from_ms = Some(from);
                    m.last_error = Some(
                        "the new certificate is not valid yet: the current one stays in use until it starts".into(),
                    );
                    m.last_error_at_ms = Some(now);
                })?;
                if fired {
                    self.emit(ok_event(&meta));
                }
            }
            return Ok(Took::Deferred(from));
        }
        let same = self
            .read_stored(id)
            .is_some_and(|p| p.chain == imp.pair.chain && p.key == imp.pair.key);
        if !same {
            self.write_stored(id, &imp.pair)?;
            tracing::info!(
                "web access: imported certificate {id} taken from its folder: {} ({})",
                leaf.subject,
                leaf.fingerprint_sha256
            );
        }
        self.update_meta(id, |m| {
            if !same {
                m.imported_at_ms = Some(now);
            }
            m.names = names.clone();
            m.clear_problem();
        })?;
        if meta.fired() {
            self.emit(ok_event(&meta));
        }
        Ok(if same { Took::Same } else { Took::Changed })
    }

    /// One look at every watched folder at `at`: a folder whose files changed and then stayed
    /// unchanged for [`SETTLE`] is read. Reloads the proxy when a stored pair changed.
    /// Returns the ids whose pair changed.
    pub fn scan_drops_at(&self, at: Instant) -> Vec<String> {
        let ids: Vec<String> = self
            .import_index()
            .into_iter()
            .filter(|m| m.source == ImportSource::Folder)
            .map(|m| m.id)
            .collect();
        let now = rynas_tls::now();
        let mut changed = Vec::new();
        {
            let mut mem = self.lock_imports();
            mem.watch.retain(|k, _| ids.contains(k));
            for id in &ids {
                let Some(sig) = self.drop_sig(id) else {
                    continue;
                };
                let w = mem.watch.entry(id.clone()).or_default();
                if w.seen != Some(sig) {
                    w.seen = Some(sig);
                    w.since = Some(at);
                    w.suspect = None;
                    continue;
                }
                let settled = w.since.is_some_and(|s| at.duration_since(s) >= SETTLE);
                let wait_over = w.wait_until_ms.is_some_and(|t| now >= t);
                if !settled || sig == [None, None] || (w.done == Some(sig) && !wait_over) {
                    continue;
                }
                let confirmed = w.suspect == Some(sig);
                match self.take_drop(id, now, Some(sig), confirmed) {
                    Err(e) => {
                        // the store could not be written: the same files again after another
                        // window (M2)
                        tracing::warn!("web access: imported certificate {id}: {e}");
                        w.done = None;
                        w.since = Some(at);
                    }
                    Ok(Took::Moved) => w.seen = None,
                    Ok(Took::Suspect) => {
                        w.suspect = Some(sig);
                        w.since = Some(at);
                    }
                    Ok(t) => {
                        w.done = Some(sig);
                        w.suspect = None;
                        w.wait_until_ms = match t {
                            Took::Deferred(from) => Some(from - SKEW_MS),
                            _ => None,
                        };
                        if t == Took::Changed {
                            changed.push(id.clone());
                        }
                    }
                }
            }
        }
        if !changed.is_empty() {
            self.reload_engine("an imported certificate changed");
        }
        changed
    }

    /// Loads the applied configuration again (the import files changed) when the proxy runs.
    fn reload_engine(&self, why: &str) {
        let has_value = self.ops.get().is_some_and(|o| {
            o.config
                .applied(super::WEB_ACCESS_KEY)
                .ok()
                .flatten()
                .is_some()
        });
        if !has_value || !self.proxy_active() {
            return;
        }
        let cfg = self.applied();
        let facts = self.facts();
        match self.push(&cfg, &facts, &|l| tracing::info!("{l}")) {
            Ok(_) => tracing::info!("web access: proxy reloaded ({why})"),
            Err(e) => tracing::warn!("web access: reloading the proxy ({why}): {e}"),
        }
    }

    /// The view of `m` against the applied configuration.
    fn import_view(&self, m: &Meta, active: &[Active], now: i64) -> ImportedCert {
        let cfg = self.applied();
        let facts = self.facts();
        let names = resolve(&cfg, &facts).all_names();
        let chain = self.stored_leaf(&m.id).unwrap_or_default();
        self.view_of(m, chain, &cfg, &names, active, now)
    }

    fn view_of(
        &self,
        m: &Meta,
        mut chain: Vec<TlsCertificate>,
        cfg: &WebAccessConfig,
        names: &[String],
        active: &[Active],
        now: i64,
    ) -> ImportedCert {
        let folder = (m.source == ImportSource::Folder).then(|| format!("{DROP_DIR}/{}", m.id));
        let mut problem = m.problem;
        if problem.is_none() && folder.is_some() && self.drop_group_missing() {
            problem = Some(ImportProblem::GroupMissing);
        }
        let mut v = ImportedCert {
            id: m.id.clone(),
            label: m.label.clone(),
            source: m.source,
            folder,
            imported_at_ms: m.imported_at_ms,
            last_error: m.last_error.clone(),
            last_error_at_ms: m.last_error_at_ms,
            problem,
            names: m.names.clone(),
            pending_names: m.pending_names.clone(),
            pending_from_ms: m.pending_from_ms,
            status: CertStatus::Pending,
            ..Default::default()
        };
        if chain.is_empty() {
            return v;
        }
        v.warnings = rynas_tls::warnings(&chain, &[], now);
        v.warnings.retain(|w| *w != TlsWarning::NamesNotCovered);
        let leaf = chain.remove(0);
        v.status = status_of(&leaf, now);
        if let Some(me) = active.iter().find(|a| a.id == m.id) {
            let mine: Vec<String> = names
                .iter()
                .filter(|n| me.dns.iter().any(|d| covers(d, n)))
                .cloned()
                .collect();
            let who = assign(active, &mine);
            for n in mine {
                if who.get(&n) == Some(&m.id) {
                    v.covers.push(n);
                } else {
                    v.shadowed.push(n);
                }
            }
            if cfg.settings.mode == ProxyMode::Builtin {
                for d in domains::active(cfg) {
                    v.replaces_acme.extend(
                        domains::cert_subjects(cfg, d)
                            .into_iter()
                            .filter(|s| replaces_subject(me, s)),
                    );
                }
            }
        }
        v.certificate = Some(leaf);
        v.chain = chain;
        v
    }

    /// `GET /web-access/imports`.
    pub fn imports(&self) -> Vec<ImportedCert> {
        let now = rynas_tls::now();
        let active = self.active_imports(now);
        self.import_index()
            .iter()
            .map(|m| self.import_view(m, &active, now))
            .collect()
    }

    fn import_one(&self, id: &str) -> Res<ImportedCert> {
        let now = rynas_tls::now();
        let active = self.active_imports(now);
        let m = self.find_meta(id)?;
        Ok(self.import_view(&m, &active, now))
    }

    /// Checks the request's fields; returns its `names` as a set.
    fn check_request(req: &CertImportRequest) -> Res<Vec<String>> {
        if !valid_id(&req.id) {
            return Err(bad(
                "id",
                "1-32 characters: a-z, 0-9 and -, not starting with -",
            ));
        }
        if req.label.chars().count() > 80 || req.label.chars().any(char::is_control) {
            return Err(bad("label", "at most 80 characters, no control characters"));
        }
        if req.names.len() > MAX_NAMES {
            return Err(bad("names", format!("at most {MAX_NAMES} names")));
        }
        if let Some(n) = req.names.iter().find(|n| !valid_dns_name(n)) {
            return Err(bad(
                "names",
                format!("{n:?} is not a DNS name (labels of a-z, 0-9 and -, optionally *. in front)"),
            ));
        }
        Ok(name_set(&req.names))
    }

    /// The pair of a request, validated (`None` when it carries none); with `names`, they
    /// must be the pair's.
    fn request_pair(
        req: &CertImportRequest,
        names: &[String],
        now: i64,
    ) -> Res<Option<rynas_tls::Imported>> {
        let imp = match (&req.certificate, &req.private_key) {
            (None, None) if req.source == ImportSource::Folder => return Ok(None),
            (None, _) => return Err(bad("certificate", "the certificate chain is required")),
            (_, None) => return Err(bad("private_key", "the private key is required")),
            (Some(c), Some(k)) => validate(c, k, now).map_err(|(f, m)| bad(&f, m))?,
        };
        let own = name_set(&imp.chain[0].dns_names);
        if !names.is_empty() && names != own {
            return Err(bad(
                "names",
                format!("the certificate is for {}", own.join(", ")),
            ));
        }
        Ok(Some(imp))
    }

    /// `POST /web-access/imports/inspect`: what an import would be, nothing stored.
    pub fn inspect_import(&self, req: &CertImportRequest) -> Res<ImportedCert> {
        let names = Self::check_request(req)?;
        let now = rynas_tls::now();
        let imp = Self::request_pair(req, &names, now)?;
        let mut meta = Meta {
            id: req.id.clone(),
            label: req.label.clone(),
            source: req.source,
            names,
            ..Default::default()
        };
        let mut active: Vec<Active> = self
            .active_imports(now)
            .into_iter()
            .filter(|a| a.id != req.id)
            .collect();
        let (chain, pair) = match imp {
            Some(i) => (i.chain, Some(i.pair)),
            None => (Vec::new(), None),
        };
        if let (Some(leaf), Some(pair)) = (chain.first(), pair) {
            meta.names = name_set(&leaf.dns_names);
            if serves(leaf, now) {
                active.push(Active {
                    id: req.id.clone(),
                    dns: leaf
                        .dns_names
                        .iter()
                        .map(|d| d.to_ascii_lowercase())
                        .collect(),
                    not_after_ms: leaf.not_after_ms,
                    renew_acme: leaf.not_after_ms - now < certs::SOON_MS,
                    cert: String::new(),
                    key: String::new(),
                    pair,
                });
            }
        }
        let cfg = self.applied();
        let names = resolve(&cfg, &self.facts()).all_names();
        let mut v = self.view_of(&meta, chain, &cfg, &names, &active, now);
        v.imported_at_ms = None;
        Ok(v)
    }

    /// `POST /web-access/imports`: creates or replaces an import, then reloads the proxy.
    pub fn save_import(&self, req: &CertImportRequest) -> Res<ImportedCert> {
        let names = Self::check_request(req)?;
        let now = rynas_tls::now();
        let imp = Self::request_pair(req, &names, now)?;
        {
            let mut mem = self.lock_imports();
            let mut idx = self.import_index();
            let prev = idx.iter().find(|m| m.id == req.id).cloned();
            // a pair that starts later never replaces one that still serves (L1)
            if let Some(imp) = &imp
                && imp.chain[0].not_before_ms > now + SKEW_MS
                && self
                    .leaf_of(&req.id)
                    .and_then(|c| c.into_iter().next())
                    .is_some_and(|c| serves(&c, now))
            {
                return Err(bad(
                    "certificate",
                    "the certificate is not valid yet and the stored one still is: import it once it has started",
                ));
            }
            // the folder first: a missing group refuses the save before anything is stored
            if req.source == ImportSource::Folder {
                self.ensure_drop(&req.id)?;
            } else if prev
                .as_ref()
                .is_some_and(|p| p.source == ImportSource::Folder)
            {
                self.remove_drop(&req.id).map_err(Error::internal)?;
            }
            if let Some(imp) = &imp {
                self.write_stored(&req.id, &imp.pair)
                    .map_err(Error::internal)?;
            }
            let mut m = prev.unwrap_or_else(|| Meta {
                id: req.id.clone(),
                ..Default::default()
            });
            let before = m.clone();
            m.label = req.label.clone();
            m.source = req.source;
            if let Some(imp) = &imp {
                // an upload is the administrator's choice of names
                m.imported_at_ms = Some(now);
                m.names = name_set(&imp.chain[0].dns_names);
                m.clear_problem();
            } else if !names.is_empty() {
                m.names = names;
            }
            if req.source == ImportSource::Upload {
                m.clear_problem();
            }
            idx.retain(|x| x.id != req.id);
            idx.push(m.clone());
            idx.sort_by(|a, b| a.id.cmp(&b.id));
            self.save_index(&idx).map_err(Error::internal)?;
            // the folder is read again on the next look (new pinned names, new source)
            mem.watch.remove(&req.id);
            if before.fired() && !m.fired() {
                self.emit(ok_event(&before));
            }
        }
        if let Some(imp) = &imp {
            tracing::info!(
                "web access: imported certificate {}: {} ({})",
                req.id,
                imp.chain[0].subject,
                imp.chain[0].fingerprint_sha256
            );
        }
        self.reload_engine("an imported certificate changed");
        self.import_one(&req.id)
    }

    fn folder_meta(&self, id: &str) -> Res<Meta> {
        let m = self.find_meta(id)?;
        if m.source != ImportSource::Folder {
            return Err(conflict(format!(
                "imported certificate {id} has no watched folder"
            )));
        }
        Ok(m)
    }

    /// `POST /web-access/imports/{id}/check`: reads the watched folder now (no settle wait).
    pub fn check_import(&self, id: &str) -> Res<ImportedCert> {
        self.folder_meta(id)?;
        let changed = {
            let mut mem = self.lock_imports();
            mem.watch.remove(id);
            self.take_drop(id, rynas_tls::now(), None, true)
                .map_err(Error::internal)?
                == Took::Changed
        };
        if changed {
            self.reload_engine("an imported certificate changed");
        }
        self.import_one(id)
    }

    /// `POST /web-access/imports/{id}/accept`: pins the names of the held pair and reads the
    /// folder again (a pair pushed since for yet other names is held again). Returns the
    /// view and the names accepted.
    pub fn accept_import(&self, id: &str) -> Res<(ImportedCert, Vec<String>)> {
        let m = self.folder_meta(id)?;
        let (changed, accepted) = {
            let mut mem = self.lock_imports();
            let m = self.find_meta(id)?;
            if m.problem != Some(ImportProblem::NamesChanged) || m.pending_names.is_empty() {
                return Err(conflict(format!(
                    "imported certificate {id} holds no pair with new names"
                )));
            }
            let accepted = m.pending_names.clone();
            self.update_meta(id, |x| x.names = accepted.clone())
                .map_err(Error::internal)?;
            mem.watch.remove(id);
            let t = self
                .take_drop(id, rynas_tls::now(), None, true)
                .map_err(Error::internal)?;
            (t == Took::Changed, accepted)
        };
        tracing::info!(
            "web access: imported certificate {}: names {} accepted",
            m.id,
            accepted.join(", ")
        );
        if changed {
            self.reload_engine("an imported certificate changed");
        }
        Ok((self.import_one(id)?, accepted))
    }

    /// `DELETE /web-access/imports/{id}`: its names go back to ACME or the local CA.
    pub fn delete_import(&self, id: &str) -> Res<()> {
        {
            let mut mem = self.lock_imports();
            let mut idx = self.import_index();
            let Some(m) = idx.iter().find(|m| m.id == id).cloned() else {
                return Err(Error::not_found(format!("no imported certificate {id}")));
            };
            idx.retain(|x| x.id != id);
            self.save_index(&idx).map_err(Error::internal)?;
            for ext in ["key", "crt"] {
                match std::fs::remove_file(self.import_dir().join(format!("{id}.{ext}"))) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => {
                        return Err(Error::internal(e));
                    }
                    _ => {}
                }
            }
            if m.source == ImportSource::Folder {
                self.remove_drop(id).map_err(Error::internal)?;
            }
            mem.watch.remove(id);
            // a notification still firing for it is resolved (L3)
            if m.fired() {
                self.emit(ok_event(&m));
            }
        }
        self.reload_engine("an imported certificate was removed");
        Ok(())
    }
}

/// Polls the watched folders every [`POLL`] (a thread: two `stat`s per folder).
pub fn spawn_watch(svc: Arc<WebAccessSvc>) {
    std::thread::Builder::new()
        .name("cert-drop".into())
        .spawn(move || {
            loop {
                svc.scan_drops_at(Instant::now());
                std::thread::sleep(POLL);
            }
        })
        .map(|_| ())
        .unwrap_or_else(|e| tracing::error!("web access: watched certificate folders: {e}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn act(id: &str, dns: &[&str], not_after: i64) -> Active {
        Active {
            id: id.into(),
            dns: dns.iter().map(|s| s.to_string()).collect(),
            not_after_ms: not_after,
            renew_acme: false,
            cert: String::new(),
            key: String::new(),
            pair: PemPair {
                chain: String::new(),
                key: String::new(),
            },
        }
    }

    #[test]
    fn ids() {
        assert!(valid_id("router-wildcard"));
        assert!(valid_id("a"));
        for bad in ["", "-a", "A", "a_b", "a/b", "..", &"x".repeat(33)] {
            assert!(!valid_id(bad), "{bad}");
        }
    }

    #[test]
    fn dns_names_and_sets() {
        for ok in ["nas.example.com", "*.nas.example.com", "NAS.Example.com", "xn--bcher-kva.example"] {
            assert!(valid_dns_name(ok), "{ok}");
        }
        for bad in ["", "com", "*.com", "a..b", "-a.b", "a-.b", "a b.c", "*a.b.c", "a.*.c", &format!("{}.com", "a".repeat(64))] {
            assert!(!valid_dns_name(bad), "{bad}");
        }
        assert_eq!(
            name_set(&["B.example.com", "a.example.com", "b.example.com"]),
            ["a.example.com", "b.example.com"]
        );
    }

    #[test]
    fn wildcard_matching() {
        assert!(covers("nas.example.com", "NAS.example.com"));
        assert!(covers("*.example.com", "immich.example.com"));
        assert!(covers("*.Example.com", "immich.example.COM"));
        assert!(!covers("*.example.com", "example.com"));
        assert!(!covers("*.example.com", "a.b.example.com"));
        assert!(
            !covers("*.com", "example.com"),
            "no wildcard over a single label"
        );
        assert!(
            !covers("a*.example.com", "ab.example.com"),
            "partial wildcards refused"
        );
        assert!(!covers("*.*.example.com", "a.b.example.com"));
        assert!(!covers("*.example.com", "*.example.com.evil"));
        assert!(
            covers("*.example.com", "*.example.com"),
            "same wildcard string"
        );
    }

    #[test]
    fn exact_beats_wildcard_then_expiry_then_id() {
        let a = act("a-wild", &["*.example.com"], 3_000);
        let b = act("b-exact", &["photos.example.com"], 1_000);
        let c = act("c-wild", &["*.example.com"], 3_000);
        let d = act("d-later", &["*.example.com"], 9_000);
        let names: Vec<String> = ["photos.example.com", "files.example.com", "nas.local"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let m = assign(&[a.clone(), b.clone(), c.clone()], &names);
        assert_eq!(m["photos.example.com"], "b-exact");
        assert_eq!(
            m["files.example.com"], "a-wild",
            "same expiry: the smaller id"
        );
        assert!(!m.contains_key("nas.local"));
        let m = assign(&[a, b, c, d], &names);
        assert_eq!(m["files.example.com"], "d-later");
        assert_eq!(m["photos.example.com"], "b-exact");
    }

    #[test]
    fn acme_subjects_replaced() {
        let w = act("w", &["*.nas.example.com", "nas.example.com"], 1);
        assert!(replaces_subject(&w, "*.nas.example.com"));
        assert!(replaces_subject(&w, "nas.example.com"));
        let list = act("l", &["a.nas.example.com", "b.nas.example.com"], 1);
        assert!(!replaces_subject(&list, "*.nas.example.com"));
        assert!(!replaces_subject(&list, "nas.example.com"));
        let wide = act("x", &["*.example.com"], 1);
        assert!(replaces_subject(&wide, "nas.example.com"));
        assert!(!replaces_subject(&wide, "*.nas.example.com"));
        // expiring soon: ACME is ordered again ahead of the expiry (L2)
        let mut soon = w.clone();
        soon.renew_acme = true;
        assert!(!replaces_subject(&soon, "nas.example.com"));
    }
}
