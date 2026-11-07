//! Imported certificates (§5.5 of `docs/phase2/docker-apps.md`): certificates made elsewhere
//! (a router's ACME client, a company CA, a bought wildcard) served by rynas-caddy for the
//! names they cover.
//!
//! - **Store.** rynasd's private folder `<state>/web-access/imported/` (0700 root):
//!   `index.json` (ids, labels, sources, last error) and `<id>.crt` / `<id>.key` (0600), the
//!   last good pair. Never in the configuration key: the key is not in `config.db`, its
//!   revisions or a passphrase-less `.rynasbak`.
//! - **Watched folder.** `/var/lib/rynas-cert-drop/<id>/` (`2770 root:rynas-cert-drop` in a
//!   base `0750 root:rynas-cert-drop`, usable as an sshd `ChrootDirectory`): another tool
//!   writes `fullchain.pem` and `privkey.pem` there. rynasd polls the two files every
//!   [`POLL`] (a `stat` through descriptors), waits until they have stayed unchanged for
//!   [`SETTLE`] (acme.sh's ssh deploy rewrites the files in place with `echo >`, one after the
//!   other, so a rename-based trigger alone would catch half-written pairs), then validates.
//!   Files are opened below the folder without following symlinks, regular files only, at
//!   most [`MAX_PEM`] bytes. A good pair replaces the stored one and reloads the proxy; a bad
//!   one leaves everything as it is and raises `web_access.cert_import_failed`.
//! - **Engine files.** Each valid pair is copied to `rynas/import-<id>.{crt,key}` in the
//!   engine's state folder (0640 root:rynas-caddy, key first), next to the local leaf; the
//!   engine never reads the watched folder.
//! - **Precedence.** A valid imported certificate takes over every served name its DNS names
//!   cover (exact, or a one-label wildcard, RFC 6125 §6.4.3): through a TLS connection policy
//!   per certificate (`match.sni` + `certificate_selection.any_tag`), so the choice never
//!   depends on certmagic's cache order; ACME subjects it covers are not ordered. Between two
//!   imports, an exact name beats a wildcard, then the later expiry, then the id. An expired
//!   import is dropped from the render: its names go back to ACME or the local CA.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rynas_api_types::notify::{NewEvent, NotifyCategory, NotifySeverity};
use rynas_api_types::tls::{TlsCertificate, TlsWarning};
use rynas_api_types::web_access::{
    CertImportRequest, CertStatus, ImportSource, ImportedCert, ProxyMode, WebAccessConfig,
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
/// Prefix of the engine's copies (`rynas/import-<id>.crt`) and of the Caddy tags.
pub const ENGINE_PREFIX: &str = "import-";
pub const TAG_PREFIX: &str = "rynas-import-";

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
}

/// A valid import as the renderer needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Active {
    pub id: String,
    /// DNS names of the leaf, lowercased.
    pub dns: Vec<String>,
    pub not_after_ms: i64,
    /// The engine's copies (absolute engine paths).
    pub cert: String,
    pub key: String,
}

impl Active {
    pub fn tag(&self) -> String {
        format!("{TAG_PREFIX}{}", self.id)
    }
}

/// `stat` of both files: inode, size, mtime and ctime (ns). `None` entries: missing.
pub type Sig = [Option<(u64, u64, i64, i64)>; 2];

/// Poll state of one watched folder.
#[derive(Debug, Default)]
pub struct WatchState {
    seen: Option<Sig>,
    since: Option<Instant>,
    done: Option<Sig>,
}

/// What the import code remembers between calls (behind [`WebAccessSvc`]'s `imports`).
#[derive(Debug, Default)]
pub struct ImportMem {
    pub watch: BTreeMap<String, WatchState>,
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
/// of names does not cover every name below), a plain one by any covering DNS name.
pub fn replaces_subject(a: &Active, subject: &str) -> bool {
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

/// A good pair after refused files.
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

fn sig_of(f: &std::fs::File) -> io::Result<(u64, u64, i64, i64)> {
    let m = f.metadata()?;
    Ok((
        m.ino(),
        m.size(),
        m.mtime() * 1_000_000_000 + m.mtime_nsec(),
        m.ctime() * 1_000_000_000 + m.ctime_nsec(),
    ))
}

impl WebAccessSvc {
    fn import_dir(&self) -> PathBuf {
        self.state_dir.join("web-access/imported")
    }

    fn drop_base(&self) -> PathBuf {
        self.path(DROP_DIR)
    }

    /// The import index (empty when missing or unreadable).
    pub fn import_index(&self) -> Vec<Meta> {
        std::fs::read_to_string(self.import_dir().join("index.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
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

    fn read_stored(&self, id: &str) -> Option<PemPair> {
        let d = self.import_dir();
        let chain = std::fs::read_to_string(d.join(format!("{id}.crt"))).ok()?;
        let key = std::fs::read_to_string(d.join(format!("{id}.key"))).ok()?;
        Some(PemPair { chain, key })
    }

    pub(super) fn stored_leaf(&self, id: &str) -> Option<Vec<TlsCertificate>> {
        let chain = std::fs::read_to_string(self.import_dir().join(format!("{id}.crt"))).ok()?;
        rynas_tls::inspect(&chain).ok()
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

    /// The valid (not expired, already started) imports with their engine paths.
    pub fn active_imports(&self, now: i64) -> Vec<Active> {
        let dir = Path::new(&self.paths.cert)
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        self.import_index()
            .iter()
            .filter_map(|m| {
                let leaf = self.stored_leaf(&m.id)?.into_iter().next()?;
                (leaf.not_after_ms > now && leaf.not_before_ms <= now + certs::DAY_MS).then(|| {
                    Active {
                        id: m.id.clone(),
                        dns: leaf
                            .dns_names
                            .iter()
                            .map(|d| d.to_ascii_lowercase())
                            .collect(),
                        not_after_ms: leaf.not_after_ms,
                        cert: format!("{dir}/{ENGINE_PREFIX}{}.crt", m.id),
                        key: format!("{dir}/{ENGINE_PREFIX}{}.key", m.id),
                    }
                })
            })
            .collect()
    }

    /// Copies the valid pairs into the engine's `rynas/` folder and removes copies no longer
    /// used. Returns whether a file changed (the load must re-read the same paths).
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
            let Some(pair) = self.read_stored(&a.id) else {
                continue;
            };
            for (ext, data) in [("key", &pair.key), ("crt", &pair.chain)] {
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

    /// The watched folder of `id`, made when missing; owner and mode put back when they
    /// drifted (the base must stay non-writable for the group: an sshd chroot refuses it).
    fn ensure_drop(&self, id: &str) -> io::Result<()> {
        let base = self.drop_base();
        let parent = base.parent().expect("has a parent");
        std::fs::create_dir_all(parent)?;
        let start = safefs::open_start(parent)?;
        let gid = group_gid(&self.root, DROP_GROUP);
        let owner = gid.map(|g| (0, g));
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

    /// Reads and validates the pair in the watched folder.
    fn read_drop(&self, id: &str, now: i64) -> Result<rynas_tls::Imported, String> {
        let d = self
            .open_drop(id)
            .map_err(|e| format!("the folder cannot be opened: {e}"))?;
        let read = |n: &str| {
            safefs::read_at(&d, n, MAX_PEM).map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => format!("{n} is missing"),
                _ => format!("{n} cannot be read: {e}"),
            })
        };
        let chain = read(FULLCHAIN)?;
        let key = read(PRIVKEY)?;
        validate(&chain, &key, now).map_err(|(f, m)| {
            let file = if f == "private_key" {
                PRIVKEY
            } else {
                FULLCHAIN
            };
            format!("{file}: {m}")
        })
    }

    /// Takes the watched folder's pair when it is good and differs from the stored one;
    /// records and notifies a refusal. Returns whether the stored pair changed.
    fn take_drop(&self, id: &str, now: i64) -> io::Result<bool> {
        let Some(meta) = self.import_index().into_iter().find(|m| m.id == id) else {
            return Ok(false);
        };
        match self.read_drop(id, now) {
            Ok(imp) => {
                let same = self
                    .read_stored(id)
                    .is_some_and(|p| p.chain == imp.pair.chain && p.key == imp.pair.key);
                if !same {
                    self.write_stored(id, &imp.pair)?;
                    tracing::info!(
                        "web access: imported certificate {id} taken from its folder: {} ({})",
                        imp.chain[0].subject,
                        imp.chain[0].fingerprint_sha256
                    );
                }
                let had_error = meta.last_error.is_some();
                self.update_meta(id, |m| {
                    if !same {
                        m.imported_at_ms = Some(now);
                    }
                    m.last_error = None;
                    m.last_error_at_ms = None;
                })?;
                if had_error && let Some(n) = self.notifier.get() {
                    n.emit(ok_event(&meta));
                }
                Ok(!same)
            }
            Err(err) => {
                tracing::warn!("web access: imported certificate {id}: {err}");
                if meta.last_error.as_deref() != Some(err.as_str()) {
                    self.update_meta(id, |m| {
                        m.last_error = Some(err.clone());
                        m.last_error_at_ms = Some(now);
                    })?;
                    if let Some(n) = self.notifier.get() {
                        n.emit(failed_event(&meta, &err));
                    }
                }
                Ok(false)
            }
        }
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
        let mut due = Vec::new();
        {
            let mut mem = self.imports.lock().unwrap_or_else(|e| e.into_inner());
            mem.watch.retain(|k, _| ids.contains(k));
            for id in &ids {
                let Some(sig) = self.drop_sig(id) else {
                    continue;
                };
                let w = mem.watch.entry(id.clone()).or_default();
                if w.seen != Some(sig) {
                    w.seen = Some(sig);
                    w.since = Some(at);
                    continue;
                }
                let settled = w.since.is_some_and(|s| at.duration_since(s) >= SETTLE);
                if settled && w.done != Some(sig) && sig != [None, None] {
                    w.done = Some(sig);
                    due.push(id.clone());
                }
            }
        }
        let now = rynas_tls::now();
        let changed: Vec<String> = due
            .into_iter()
            .filter(|id| {
                let _g = self.imports.lock().unwrap_or_else(|e| e.into_inner());
                self.take_drop(id, now)
                    .map_err(|e| tracing::warn!("web access: imported certificate {id}: {e}"))
                    .unwrap_or(false)
            })
            .collect();
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
        let mut v = ImportedCert {
            id: m.id.clone(),
            label: m.label.clone(),
            source: m.source,
            folder,
            imported_at_ms: m.imported_at_ms,
            last_error: m.last_error.clone(),
            last_error_at_ms: m.last_error_at_ms,
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
        let m = self
            .import_index()
            .into_iter()
            .find(|m| m.id == id)
            .ok_or_else(|| Error::not_found(format!("no imported certificate {id}")))?;
        Ok(self.import_view(&m, &active, now))
    }

    fn check_request(req: &CertImportRequest) -> Res<()> {
        if !valid_id(&req.id) {
            return Err(bad(
                "id",
                "1-32 characters: a-z, 0-9 and -, not starting with -",
            ));
        }
        if req.label.chars().count() > 80 || req.label.chars().any(char::is_control) {
            return Err(bad("label", "at most 80 characters, no control characters"));
        }
        Ok(())
    }

    /// The pair of a request, validated (`None` when it carries none).
    fn request_pair(req: &CertImportRequest, now: i64) -> Res<Option<rynas_tls::Imported>> {
        match (&req.certificate, &req.private_key) {
            (None, None) if req.source == ImportSource::Folder => Ok(None),
            (None, _) => Err(bad("certificate", "the certificate chain is required")),
            (_, None) => Err(bad("private_key", "the private key is required")),
            (Some(c), Some(k)) => validate(c, k, now).map(Some).map_err(|(f, m)| bad(&f, m)),
        }
    }

    /// `POST /web-access/imports/inspect`: what an import would be, nothing stored.
    pub fn inspect_import(&self, req: &CertImportRequest) -> Res<ImportedCert> {
        Self::check_request(req)?;
        let now = rynas_tls::now();
        let imp = Self::request_pair(req, now)?;
        let meta = Meta {
            id: req.id.clone(),
            label: req.label.clone(),
            source: req.source,
            ..Default::default()
        };
        let mut active: Vec<Active> = self
            .active_imports(now)
            .into_iter()
            .filter(|a| a.id != req.id)
            .collect();
        let chain = imp.map(|i| i.chain).unwrap_or_default();
        if let Some(leaf) = chain.first() {
            active.push(Active {
                id: req.id.clone(),
                dns: leaf
                    .dns_names
                    .iter()
                    .map(|d| d.to_ascii_lowercase())
                    .collect(),
                not_after_ms: leaf.not_after_ms,
                cert: String::new(),
                key: String::new(),
            });
        }
        let cfg = self.applied();
        let names = resolve(&cfg, &self.facts()).all_names();
        let mut v = self.view_of(&meta, chain, &cfg, &names, &active, now);
        v.imported_at_ms = None;
        Ok(v)
    }

    /// `POST /web-access/imports`: creates or replaces an import, then reloads the proxy.
    pub fn save_import(&self, req: &CertImportRequest) -> Res<ImportedCert> {
        Self::check_request(req)?;
        let now = rynas_tls::now();
        let imp = Self::request_pair(req, now)?;
        {
            let _g = self.imports.lock().unwrap_or_else(|e| e.into_inner());
            let mut idx = self.import_index();
            let prev = idx.iter().find(|m| m.id == req.id).cloned();
            if let Some(imp) = &imp {
                self.write_stored(&req.id, &imp.pair)
                    .map_err(Error::internal)?;
            }
            if req.source == ImportSource::Folder {
                self.ensure_drop(&req.id).map_err(Error::internal)?;
            } else if prev
                .as_ref()
                .is_some_and(|p| p.source == ImportSource::Folder)
            {
                self.remove_drop(&req.id).map_err(Error::internal)?;
            }
            let mut m = prev.unwrap_or_else(|| Meta {
                id: req.id.clone(),
                ..Default::default()
            });
            m.label = req.label.clone();
            m.source = req.source;
            if imp.is_some() {
                m.imported_at_ms = Some(now);
                m.last_error = None;
                m.last_error_at_ms = None;
            }
            idx.retain(|x| x.id != req.id);
            idx.push(m);
            idx.sort_by(|a, b| a.id.cmp(&b.id));
            self.save_index(&idx).map_err(Error::internal)?;
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

    /// `POST /web-access/imports/{id}/check`: reads the watched folder now (no settle wait).
    pub fn check_import(&self, id: &str) -> Res<ImportedCert> {
        let m = self
            .import_index()
            .into_iter()
            .find(|m| m.id == id)
            .ok_or_else(|| Error::not_found(format!("no imported certificate {id}")))?;
        if m.source != ImportSource::Folder {
            return Err(Error::new(
                axum::http::StatusCode::CONFLICT,
                rynas_api_types::error_codes::CONFLICT,
                format!("imported certificate {id} has no watched folder"),
            ));
        }
        let changed = {
            let _g = self.imports.lock().unwrap_or_else(|e| e.into_inner());
            self.take_drop(id, rynas_tls::now())
                .map_err(Error::internal)?
        };
        if changed {
            self.reload_engine("an imported certificate changed");
        }
        self.import_one(id)
    }

    /// `DELETE /web-access/imports/{id}`: its names go back to ACME or the local CA.
    pub fn delete_import(&self, id: &str) -> Res<()> {
        {
            let _g = self.imports.lock().unwrap_or_else(|e| e.into_inner());
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
            cert: String::new(),
            key: String::new(),
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
    }
}
