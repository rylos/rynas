//! Signed online catalogue updates (D-APPCAT note of 2026-11-10,
//! `docs/phase2/docker-apps.md` § 6.12): the catalogue changes without a rynas release.
//!
//! - Once a day (random delay) and on "Check now" (task `apps.catalog-update`), rynasd reads
//!   `<base>/index.json` (conditional GET: ETag / Last-Modified) and `index.json.minisig`,
//!   verifies them with [`rynas_apps::online`], downloads the archive the index names and
//!   unpacks it to `<state>/catalog/.new-<serial>`. Only a catalogue that validates entirely
//!   is renamed to `<state>/catalog/<serial>/` and becomes `current` (a symbolic link switched
//!   with a rename); the one before stays as `previous` for fallback, older ones are deleted.
//! - `state.json` keeps the last accepted serial (anti-rollback, together with `current`),
//!   the ETag, the last check and its error.
//! - The catalogue the apps pages read is the shipped one (`--apps-dir`) merged with the
//!   downloaded one ([`rynas_apps::catalog::merge`]); a download whose serial is not above
//!   the shipped `catalog.toml` is ignored (a newer package wins over an older download).
//! - With no compiled-in key ([`rynas_apps::online::TRUSTED_KEYS`]) or the setting off
//!   (`DockerSettings::apps_online_catalog`), nothing is fetched or merged.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rynas_api_types::apps::CatalogSourceState;
use rynas_api_types::notify::{NotifyCategory, NotifySeverity};
use rynas_apps::Version;
use rynas_apps::online::{self as ol, Fresh, Index, OnlineError, Validated};
use rynas_notify::event::{code, field, firing, oneshot, resolved};
use serde::{Deserialize, Serialize};

use super::AppsSvc;

/// The official online catalogue (GitHub Pages of `rylos/rynas-catalog`).
pub const DEFAULT_URL: &str = "https://rylos.github.io/rynas-catalog/v1/";
const INDEX: &str = "index.json";
const SIGNATURE: &str = "index.json.minisig";
const STATE: &str = "state.json";
const CURRENT: &str = "current";
const PREVIOUS: &str = "previous";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const INDEX_TIMEOUT: Duration = Duration::from_secs(60);
const ARCHIVE_TIMEOUT: Duration = Duration::from_secs(300);
/// Between two automatic checks, plus up to [`JITTER`].
const INTERVAL: Duration = Duration::from_secs(24 * 3600);
const JITTER: u64 = 3 * 3600;
/// After a failed automatic check (network down), the next try.
const RETRY: Duration = Duration::from_secs(3 * 3600);
/// The signature and the index are two requests: while the site is being published they may
/// not match, so a refused signature is tried once more after this.
const SIGNATURE_RETRY: Duration = Duration::from_secs(30);
const NOTIFY_KEY: &str = "apps:catalog:refused";

/// An `https://` base URL without query, fragment or blanks.
pub fn check_base_url(u: &str) -> Result<(), String> {
    if u.len() > 512 {
        return Err("longer than 512 characters".into());
    }
    let rest = u
        .strip_prefix("https://")
        .ok_or("must start with https://")?;
    if rest.is_empty() || rest.starts_with('/') {
        return Err("no host".into());
    }
    if u.chars()
        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '\\' | '?' | '#'))
    {
        return Err("blanks, quotes, query and fragment are not allowed".into());
    }
    Ok(())
}

/// The base URL with a final `/`.
pub fn normalize(u: &str) -> String {
    if u.ends_with('/') {
        u.to_owned()
    } else {
        format!("{u}/")
    }
}

fn unix_now() -> i64 {
    jiff::Timestamp::now().as_second()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    /// The last accepted serial (never goes down).
    serial: Option<u64>,
    /// Index URL the validators below belong to.
    url: Option<String>,
    etag: Option<String>,
    last_modified: Option<String>,
    last_check: Option<i64>,
    last_success: Option<i64>,
    last_error: Option<String>,
    last_error_code: Option<String>,
}

/// The downloaded catalogue in use.
#[derive(Debug, Clone)]
pub struct Active {
    pub serial: u64,
    pub dir: PathBuf,
    pub index: Index,
}

/// What a check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The site answered 304: the index in use is still the published one.
    NotModified,
    /// The published serial is the one in use.
    Unchanged(u64),
    /// A newer catalogue is now in use.
    Updated {
        serial: u64,
        previous: Option<u64>,
        skipped: Vec<String>,
    },
}

/// The downloads and the folder they go to.
pub struct Online {
    /// `<state>/catalog`.
    pub dir: PathBuf,
    keys: Vec<String>,
    /// Plain `http://` URLs (tests with a local server only).
    allow_http: bool,
    /// [`SIGNATURE_RETRY`] (none in tests).
    sig_retry: Duration,
    state: Mutex<State>,
    checking: AtomicBool,
    run: tokio::sync::Mutex<()>,
}

struct Got {
    status: u16,
    body: Vec<u8>,
    etag: Option<String>,
    last_modified: Option<String>,
}

/// Clears [`Online::checking`] on every way out.
struct Checking<'a>(&'a AtomicBool);

impl Drop for Checking<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// The serial a link (`current`, `previous`) points at, when it names an existing folder.
fn link_serial(dir: &Path, name: &str) -> Option<(u64, PathBuf)> {
    let target = fs::read_link(dir.join(name)).ok()?;
    let s = target.to_str()?;
    let serial: u64 = s.parse().ok()?;
    let p = dir.join(s);
    fs::symlink_metadata(&p)
        .ok()?
        .is_dir()
        .then_some((serial, p))
}

fn read_index(dir: &Path) -> Option<Index> {
    serde_json::from_slice(&fs::read(dir.join(INDEX)).ok()?).ok()
}

/// Points `name` at `<serial>` (a new link renamed over the old one).
fn switch(dir: &Path, name: &str, serial: u64) -> Result<(), OnlineError> {
    let tmp = dir.join(format!(".{name}.new"));
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(serial.to_string(), &tmp)
        .and_then(|()| fs::rename(&tmp, dir.join(name)))
        .map_err(|e| OnlineError::Io(format!("switch {name}: {e}")))
}

/// Unpacks, validates and switches to a verified catalogue; returns the serial it replaced.
fn install(
    dir: &Path,
    idx: &Index,
    archive: &[u8],
    index_bytes: &[u8],
    running: Version,
) -> Result<(Option<u64>, Validated), OnlineError> {
    let io = |what: &str, e: std::io::Error| OnlineError::Io(format!("{what}: {e}"));
    fs::create_dir_all(dir).map_err(|e| io("create the catalogue folder", e))?;
    let tmp = dir.join(format!(".new-{}", idx.serial));
    if fs::symlink_metadata(&tmp).is_ok() {
        fs::remove_dir_all(&tmp).map_err(|e| io("remove a leftover", e))?;
    }
    let unpacked = ol::extract(archive, &tmp).and_then(|_| {
        let v = ol::validate(&tmp, idx, running)?;
        fs::write(tmp.join(INDEX), index_bytes).map_err(|e| io("write index.json", e))?;
        Ok(v)
    });
    let validated = match unpacked {
        Ok(v) => v,
        Err(e) => {
            let _ = fs::remove_dir_all(&tmp);
            return Err(e);
        }
    };
    let target = dir.join(idx.serial.to_string());
    if fs::symlink_metadata(&target).is_ok() {
        // a leftover of an interrupted switch: never the one in use (its serial is lower)
        fs::remove_dir_all(&target).map_err(|e| io("remove a leftover", e))?;
    }
    fs::rename(&tmp, &target).map_err(|e| io("rename the new catalogue", e))?;
    let previous = link_serial(dir, CURRENT).map(|(s, _)| s);
    if let Some(p) = previous {
        switch(dir, PREVIOUS, p)?;
    }
    switch(dir, CURRENT, idx.serial)?;
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    // keep the new and the previous catalogue only
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let keep = name == idx.serial.to_string()
                || previous.is_some_and(|p| name == p.to_string())
                || matches!(name.as_str(), CURRENT | PREVIOUS | STATE);
            let numbered = name.parse::<u64>().is_ok() || name.starts_with(".new-");
            if !keep && numbered && e.file_type().is_ok_and(|t| t.is_dir()) {
                let _ = fs::remove_dir_all(e.path());
            }
        }
    }
    Ok((previous, validated))
}

impl Online {
    pub fn new(dir: PathBuf, keys: &[&str]) -> Arc<Self> {
        let state = fs::read(dir.join(STATE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Arc::new(Self {
            dir,
            keys: keys.iter().map(|k| (*k).to_owned()).collect(),
            allow_http: false,
            sig_retry: SIGNATURE_RETRY,
            state: Mutex::new(state),
            checking: AtomicBool::new(false),
            run: tokio::sync::Mutex::new(()),
        })
    }

    /// Accepts `http://` URLs and does not wait before retrying a signature: tests against a
    /// local server only.
    #[doc(hidden)]
    pub fn new_for_tests(dir: PathBuf, keys: &[&str]) -> Arc<Self> {
        let mut o = Arc::into_inner(Self::new(dir, keys)).expect("fresh Arc");
        o.allow_http = true;
        o.sig_retry = Duration::ZERO;
        Arc::new(o)
    }

    /// This build can verify catalogues.
    pub fn has_key(&self) -> bool {
        !self.keys.is_empty()
    }

    /// The downloaded catalogue in use: `current`, else `previous` when `current` is
    /// unreadable.
    pub fn active(&self) -> Option<Active> {
        [CURRENT, PREVIOUS].iter().find_map(|name| {
            let (serial, dir) = link_serial(&self.dir, name)?;
            let index = read_index(&dir)?;
            Some(Active { serial, dir, index })
        })
    }

    /// What changes when the catalogue in use changes (for the reload stamp).
    pub fn active_name(&self) -> Option<std::ffi::OsString> {
        fs::read_link(self.dir.join(CURRENT))
            .ok()
            .map(PathBuf::into_os_string)
    }

    fn save(&self, st: &State) {
        let write = || -> std::io::Result<()> {
            fs::create_dir_all(&self.dir)?;
            let tmp = self.dir.join(".state.json.new");
            fs::write(&tmp, serde_json::to_vec_pretty(st).unwrap_or_default())?;
            fs::rename(&tmp, self.dir.join(STATE))
        };
        if let Err(e) = write() {
            tracing::warn!("apps: cannot save the online catalogue state: {e}");
        }
    }

    fn client(&self) -> Result<reqwest::Client, OnlineError> {
        let allow_http = self.allow_http;
        let policy = reqwest::redirect::Policy::custom(move |a| {
            let ok = a.url().scheme() == "https" || (allow_http && a.url().scheme() == "http");
            if !ok {
                a.error("redirect to a non-https URL")
            } else if a.previous().len() >= 5 {
                a.error("too many redirects")
            } else {
                a.follow()
            }
        });
        reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(policy)
            .https_only(!allow_http)
            .user_agent(concat!("rynas/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| OnlineError::Download(e.to_string()))
    }

    async fn get(
        &self,
        client: &reqwest::Client,
        url: &str,
        limit: u64,
        validators: (Option<String>, Option<String>),
        timeout: Duration,
    ) -> Result<Got, OnlineError> {
        let dl = |m: String| OnlineError::Download(format!("{url}: {m}"));
        let mut req = client.get(url).timeout(timeout);
        if let Some(e) = validators.0 {
            req = req.header(reqwest::header::IF_NONE_MATCH, e);
        }
        if let Some(m) = validators.1 {
            req = req.header(reqwest::header::IF_MODIFIED_SINCE, m);
        }
        let mut resp = req.send().await.map_err(|e| dl(e.to_string()))?;
        let status = resp.status().as_u16();
        let header = |h: reqwest::header::HeaderName| {
            resp.headers()
                .get(h)
                .and_then(|v| v.to_str().ok())
                .filter(|v| v.len() <= 256)
                .map(String::from)
        };
        let (etag, last_modified) = (
            header(reqwest::header::ETAG),
            header(reqwest::header::LAST_MODIFIED),
        );
        if status == 304 {
            return Ok(Got {
                status,
                body: Vec::new(),
                etag,
                last_modified,
            });
        }
        if status != 200 {
            return Err(dl(format!("HTTP {status}")));
        }
        if resp.content_length().is_some_and(|n| n > limit) {
            return Err(dl(format!("larger than {limit} bytes")));
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| dl(e.to_string()))? {
            if (body.len() + chunk.len()) as u64 > limit {
                return Err(dl(format!("larger than {limit} bytes")));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Got {
            status,
            body,
            etag,
            last_modified,
        })
    }

    /// Checks the site at `base` once and switches to a newer catalogue when there is one.
    /// Records the result in `state.json`.
    pub async fn check(
        &self,
        base: &str,
        running: Version,
        log: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<Outcome, OnlineError> {
        let _run = self.run.lock().await;
        self.checking.store(true, Ordering::SeqCst);
        let _clear = Checking(&self.checking);
        let mut r = self.check_once(base, running, log).await;
        if matches!(r, Err(OnlineError::Signature(_) | OnlineError::Comment(_))) {
            log("the signature does not match the index: trying again in 30 s (the site may be publishing)");
            tokio::time::sleep(self.sig_retry).await;
            r = self.check_once(base, running, log).await;
        }
        let now = unix_now();
        let mut st = self.state.lock().unwrap();
        st.last_check = Some(now);
        match &r {
            Ok(_) => {
                st.last_success = Some(now);
                st.last_error = None;
                st.last_error_code = None;
            }
            Err(e) => {
                st.last_error = Some(e.to_string());
                st.last_error_code = Some(e.code().into());
            }
        }
        self.save(&st);
        r
    }

    async fn check_once(
        &self,
        base: &str,
        running: Version,
        log: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<Outcome, OnlineError> {
        if self.keys.is_empty() {
            return Err(OnlineError::NoKey);
        }
        let base = normalize(base);
        let index_url = format!("{base}{INDEX}");
        let client = self.client()?;
        let active = self.active();
        let validators = {
            let st = self.state.lock().unwrap();
            if active.is_some() && st.url.as_deref() == Some(index_url.as_str()) {
                (st.etag.clone(), st.last_modified.clone())
            } else {
                (None, None)
            }
        };
        log(&format!("reading {index_url}"));
        let got = self
            .get(&client, &index_url, ol::MAX_INDEX, validators, INDEX_TIMEOUT)
            .await?;
        if got.status == 304 {
            log("not modified since the last check");
            if let Some(a) = &active
                && a.index.expires_at().is_ok_and(|t| t <= jiff::Timestamp::now())
            {
                return Err(OnlineError::Expired(a.index.expires.clone()));
            }
            return Ok(Outcome::NotModified);
        }
        let sig = self
            .get(
                &client,
                &format!("{base}{SIGNATURE}"),
                ol::MAX_SIGNATURE,
                (None, None),
                INDEX_TIMEOUT,
            )
            .await?;
        let sig = String::from_utf8(sig.body)
            .map_err(|_| OnlineError::Signature("the signature is not text".into()))?;
        let keys: Vec<&str> = self.keys.iter().map(String::as_str).collect();
        let idx = ol::verify_index(&got.body, &sig, &keys)?;
        let last = self
            .state
            .lock()
            .unwrap()
            .serial
            .max(active.as_ref().map(|a| a.serial));
        let fresh = ol::check_fresh(&idx, jiff::Timestamp::now(), last)?;
        log(&format!(
            "index verified: serial {}, expires {}, {} apps",
            idx.serial,
            idx.expires,
            idx.apps.len()
        ));
        let remember = |st: &mut State| {
            st.serial = Some(idx.serial.max(st.serial.unwrap_or(0)));
            st.url = Some(index_url.clone());
            st.etag = got.etag.clone();
            st.last_modified = got.last_modified.clone();
        };
        if fresh == Fresh::Same {
            log("this catalogue is already in use");
            remember(&mut self.state.lock().unwrap());
            return Ok(Outcome::Unchanged(idx.serial));
        }
        let url = ol::archive_url(&index_url, &idx.archive, self.allow_http)?;
        log(&format!(
            "downloading {url} ({} bytes)",
            idx.archive.size
        ));
        let ar = self
            .get(&client, &url, idx.archive.size, (None, None), ARCHIVE_TIMEOUT)
            .await?;
        ol::check_archive(&ar.body, &idx.archive)?;
        log("archive size and SHA-256 match; unpacking and validating every entry");
        let (dir, idx2, body) = (self.dir.clone(), idx.clone(), got.body.clone());
        let (previous, validated) = tokio::task::spawn_blocking(move || {
            install(&dir, &idx2, &ar.body, &body, running)
        })
        .await
        .map_err(|e| OnlineError::Io(e.to_string()))??;
        for s in &validated.skipped {
            log(&format!("{s}: needs a newer rynas, left out"));
        }
        log(&format!(
            "catalogue {} in use ({} entries)",
            idx.serial,
            validated.apps.len()
        ));
        remember(&mut self.state.lock().unwrap());
        Ok(Outcome::Updated {
            serial: idx.serial,
            previous,
            skipped: validated.skipped,
        })
    }

    /// The state for the API (without what [`AppsSvc`] adds).
    fn status(&self) -> CatalogSourceState {
        let st = self.state.lock().unwrap().clone();
        let active = self.active();
        let expired = active.as_ref().is_some_and(|a| {
            a.index
                .expires_at()
                .is_ok_and(|t| t <= jiff::Timestamp::now())
        });
        CatalogSourceState {
            online_serial: active.as_ref().map(|a| a.serial),
            published: active.as_ref().and_then(|a| a.index.published.clone()),
            expires: active.as_ref().map(|a| a.index.expires.clone()),
            expired,
            last_check: st.last_check,
            last_success: st.last_success,
            last_error: st.last_error,
            last_error_code: st.last_error_code,
            checking: self.checking.load(Ordering::SeqCst),
            ..Default::default()
        }
    }

    /// When the next automatic check is due, from the last one.
    fn next_due(&self) -> Duration {
        let last = self.state.lock().unwrap().last_check;
        let jitter = Duration::from_secs(random_below(JITTER));
        match last {
            // first check of this NAS: a few minutes after the start
            None => Duration::from_secs(120 + random_below(1800)),
            Some(t) => {
                let since = Duration::from_secs(u64::try_from(unix_now() - t).unwrap_or(0));
                (INTERVAL + jitter)
                    .checked_sub(since)
                    .unwrap_or(Duration::from_secs(60 + random_below(600)))
            }
        }
    }
}

fn random_below(n: u64) -> u64 {
    let mut b = [0u8; 8];
    if getrandom::fill(&mut b).is_err() || n == 0 {
        return 0;
    }
    u64::from_le_bytes(b) % n
}

/// Whether the online catalogue is used, and from where.
pub(super) struct Use {
    pub online: Option<Arc<Online>>,
    pub reason: Option<&'static str>,
    pub url: String,
}

impl AppsSvc {
    /// The online catalogue when it is usable now (key compiled in, setting on).
    pub(super) fn online_use(&self) -> Use {
        let set = self.docker.settings();
        let url = set
            .apps_catalog_url
            .as_deref()
            .map(normalize)
            .unwrap_or_else(|| DEFAULT_URL.to_owned());
        let (online, reason) = match self.online.get() {
            Some(o) if !o.has_key() => (None, Some("no_key")),
            None => (None, Some("no_key")),
            Some(_) if !set.apps_online_catalog => (None, Some("off")),
            Some(o) => (Some(o.clone()), None),
        };
        Use {
            online,
            reason,
            url,
        }
    }

    /// `GET /v1/apps/catalog-source`.
    pub async fn catalog_source(&self) -> CatalogSourceState {
        let u = self.online_use();
        let (cat, _) = self.catalog().await;
        let mut s = self
            .online
            .get()
            .map(|o| o.status())
            .unwrap_or_default();
        s.online_enabled = u.online.is_some();
        s.disabled_reason = u.reason.map(String::from);
        s.url = u.url;
        s.shipped_serial = rynas_apps::catalog::read_serial(&self.dir).ok().flatten();
        let used = u.online.as_ref().and_then(|o| o.active());
        // a download not above the shipped serial is not merged
        let used = used.filter(|a| s.shipped_serial.is_none_or(|sh| a.serial > sh));
        s.online_apps = used.as_ref().map_or(0, |a| {
            cat.apps.values().filter(|x| x.dir.starts_with(&a.dir)).count() as u32
        });
        if used.is_none() {
            s.online_serial = None;
            s.published = None;
            s.expires = None;
            s.expired = false;
        }
        s
    }

    /// One check (automatic or "Check now"): notifies when the new catalogue brings updates
    /// for installed apps, warns when the site's data is refused.
    /// The callers check [`AppsSvc::online_use`] first (turned off: nothing to do).
    pub async fn check_catalog(
        &self,
        log: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<Outcome, OnlineError> {
        let u = self.online_use();
        let Some(o) = u.online else {
            return Err(OnlineError::NoKey);
        };
        let running = env!("CARGO_PKG_VERSION")
            .parse::<Version>()
            .unwrap_or(Version::new(0, 0, 0));
        let r = o.check(&u.url, running, log).await;
        match &r {
            Ok(outcome) => {
                self.docker.notifier.emit(code(
                    resolved(
                        NotifyCategory::Update,
                        "apps",
                        NOTIFY_KEY,
                        "Online app catalogue verified again",
                    ),
                    "apps.catalog_ok",
                ));
                if let Outcome::Updated { serial, .. } = outcome {
                    let (cat, _) = self.catalog().await;
                    let mut names: Vec<String> = self
                        .on_disk()
                        .iter()
                        .filter(|d| Self::update_available(&cat, d))
                        .map(|d| d.stack.clone())
                        .collect();
                    names.sort();
                    names.dedup();
                    if !names.is_empty() {
                        log(&format!("updates for installed apps: {}", names.join(", ")));
                        let mut ev = oneshot(
                            NotifyCategory::Update,
                            NotifySeverity::Info,
                            "apps",
                            format!("apps:catalog:{serial}"),
                            format!("App updates available: {}", names.join(", ")),
                        );
                        ev.body = "The app catalogue was updated. Review and update the apps on the Apps & Plugins page (a snapshot is taken first).".into();
                        self.docker.notifier.emit(field(
                            field(code(ev, "apps.catalog_updates"), "apps", names.join(", ")),
                            "serial",
                            serial,
                        ));
                    }
                }
            }
            Err(e) if e.is_refusal() => {
                tracing::warn!("apps: online catalogue refused: {e}");
                let mut ev = firing(
                    NotifyCategory::Security,
                    NotifySeverity::Warning,
                    "apps",
                    NOTIFY_KEY,
                    "Online app catalogue refused",
                );
                ev.body = format!(
                    "rynas keeps the catalogue it has. The downloaded catalogue failed verification: {e}"
                );
                self.docker.notifier.emit(field(
                    field(code(ev, "apps.catalog_refused"), "reason", e),
                    "code",
                    e.code(),
                ));
            }
            Err(e) => tracing::info!("apps: online catalogue check failed: {e}"),
        }
        r
    }

    /// The daily check, with a random delay; logs once when the online catalogue is off.
    pub fn spawn_catalog_updates(self: &Arc<Self>) {
        let a = self.clone();
        tokio::spawn(async move {
            let Some(o) = a.online.get().cloned() else {
                return;
            };
            if !o.has_key() {
                tracing::info!(
                    "apps: online catalogue disabled: this build has no catalogue signing key (rynas_apps::online::TRUSTED_KEYS)"
                );
                return;
            }
            let mut wait = o.next_due();
            loop {
                tokio::time::sleep(wait).await;
                if a.online_use().online.is_none() {
                    // turned off: look again later
                    wait = RETRY;
                    continue;
                }
                let log = |l: &str| tracing::debug!("apps: catalogue: {l}");
                wait = match a.check_catalog(&log).await {
                    Ok(r) => {
                        tracing::info!("apps: online catalogue check: {r:?}");
                        o.next_due()
                    }
                    // the site was not reachable: sooner than tomorrow
                    Err(OnlineError::Download(_)) => RETRY,
                    Err(_) => o.next_due(),
                };
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls() {
        assert!(check_base_url(DEFAULT_URL).is_ok());
        assert!(check_base_url("https://example.org/cat").is_ok());
        for bad in [
            "http://example.org/",
            "https://",
            "https:///x",
            "https://a b/",
            "https://a/?x=1",
            "ftp://a/",
        ] {
            assert!(check_base_url(bad).is_err(), "{bad}");
        }
        assert_eq!(normalize("https://a/b"), "https://a/b/");
        assert_eq!(normalize("https://a/b/"), "https://a/b/");
    }

    fn copy_dir(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for e in fs::read_dir(from).unwrap().flatten() {
            let t = to.join(e.file_name());
            if e.file_type().unwrap().is_dir() {
                copy_dir(&e.path(), &t);
            } else {
                fs::copy(e.path(), t).unwrap();
            }
        }
    }

    fn catalogue(root: &Path, serial: u64, version: &str) -> PathBuf {
        let apps = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps");
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("catalog.toml"), format!("serial = {serial}\n")).unwrap();
        copy_dir(&apps.join("uptime-kuma"), &root.join("uptime-kuma"));
        let p = root.join("uptime-kuma/app.toml");
        let t = fs::read_to_string(&p)
            .unwrap()
            .replace("version = \"1.0.0\"", &format!("version = \"{version}\""));
        fs::write(p, t).unwrap();
        root.to_owned()
    }

    fn active(dir: &Path, serial: u64) -> Active {
        Active {
            serial,
            dir: dir.to_owned(),
            index: Index {
                schema: 1,
                serial,
                published: None,
                expires: "2099-01-01T00:00:00Z".into(),
                archive: ol::ArchiveRef {
                    url: "a".into(),
                    sha256: String::new(),
                    size: 1,
                },
                apps: vec![],
            },
        }
    }

    #[test]
    fn download_is_merged_only_above_the_shipped_serial() {
        let t = tempfile::tempdir().unwrap();
        let shipped = catalogue(&t.path().join("shipped"), 5, "1.0.0");
        let dl = catalogue(&t.path().join("dl"), 5, "1.1.0");
        let version = |c: &rynas_apps::Catalog| c.get("uptime-kuma").unwrap().manifest.version;
        // same serial as the package: the package wins, even with an older entry
        let (c, issues) = super::super::load_sources(&shipped, Some(&active(&dl, 5)));
        assert!(issues.is_empty());
        assert_eq!(version(&c), Version::new(1, 0, 0));
        // a newer download: its newer entry wins
        let (c, _) = super::super::load_sources(&shipped, Some(&active(&dl, 6)));
        assert_eq!(version(&c), Version::new(1, 1, 0));
        assert!(c.get("uptime-kuma").unwrap().dir.starts_with(&dl));
        // the stamp follows the download in use
        assert_ne!(
            super::super::sources_stamp(&shipped, None),
            super::super::sources_stamp(&shipped, Some(&active(&dl, 6)))
        );
    }
}
