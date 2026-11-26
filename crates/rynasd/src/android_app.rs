//! The Android app published on GitHub (`GET /v1/devices/android-app`, docs/phase2/mobile-devices.md §6).
//!
//! The app lives at a fixed release, tag `android` ([`ANDROID_APK_URL`]); next to it
//! `android.json` says which version it is, its size and its hashes. The browser cannot read
//! that file (GitHub's downloads send no CORS headers), so rynasd reads it, checks it and keeps
//! the answer in memory: [`SUCCESS_TTL`] after a good answer, [`FAILURE_TTL`] after a failure,
//! with a short timeout so an offline NAS answers quickly. One fetch at a time: the others wait
//! for its answer. A failure is the typed [`AndroidApp::Unavailable`], never an error: the web
//! interface links the APK anyway.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use axum::{Json, Router, routing::get};
use rynas_api_types::devices::{
    ANDROID_METADATA_URL, ANDROID_RELEASE_PREFIX, AndroidApp, AndroidRelease,
};
use serde::Deserialize;

/// How long a good answer is kept.
pub const SUCCESS_TTL: Duration = Duration::from_secs(6 * 3600);
/// How long a failure is kept before the next try.
pub const FAILURE_TTL: Duration = Duration::from_secs(10 * 60);
/// The whole request, redirects included.
const TIMEOUT: Duration = Duration::from_secs(8);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The metadata is a few hundred bytes: anything larger is not it.
const MAX_BODY: usize = 16 * 1024;
/// The sizes an APK of the app can have.
const MIN_APK: u64 = 64 * 1024;
const MAX_APK: u64 = 512 * 1024 * 1024;
const MAX_VERSION: usize = 32;
const MAX_URL: usize = 512;

/// `android.json` as published (other fields are ignored).
#[derive(Deserialize)]
struct Metadata {
    version: String,
    size: u64,
    sha256: String,
    signer_sha256: String,
    url: String,
}

fn sha256_hex(s: &str) -> Option<String> {
    (s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())).then(|| s.to_ascii_lowercase())
}

/// The metadata, checked: a version made of letters, digits and `.-+_`, a plausible size,
/// SHA-256 hashes in hex and an HTTPS address among the project's release assets.
pub fn parse(body: &[u8]) -> Result<AndroidRelease, String> {
    if body.len() > MAX_BODY {
        return Err(format!("larger than {MAX_BODY} bytes"));
    }
    let m: Metadata = serde_json::from_slice(body).map_err(|e| format!("not valid JSON: {e}"))?;
    let version = m.version.trim();
    if version.is_empty()
        || version.len() > MAX_VERSION
        || !version.bytes().all(|b| b.is_ascii_alphanumeric() || b".-+_".contains(&b))
    {
        return Err(format!("bad version {:?}", m.version));
    }
    if !(MIN_APK..=MAX_APK).contains(&m.size) {
        return Err(format!("implausible size {}", m.size));
    }
    let sha256 = sha256_hex(&m.sha256).ok_or("bad sha256")?;
    let signer_sha256 = sha256_hex(&m.signer_sha256).ok_or("bad signer_sha256")?;
    let rest = m.url.strip_prefix(ANDROID_RELEASE_PREFIX).ok_or("url outside the project's releases")?;
    if m.url.len() > MAX_URL
        || rest.is_empty()
        || !rest.bytes().all(|b| b.is_ascii_graphic() && !b"\"'<>\\`?#%".contains(&b))
        || rest.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return Err(format!("bad url {:?}", m.url));
    }
    Ok(AndroidRelease { version: version.to_string(), size: m.size, sha256, signer_sha256, url: m.url })
}

/// The last answer and when it was read.
#[derive(Default)]
pub struct Cache {
    entry: Option<(Instant, AndroidApp)>,
}

impl Cache {
    /// The kept answer, while it is fresh.
    pub fn fresh(&self, now: Instant) -> Option<AndroidApp> {
        let (at, v) = self.entry.as_ref()?;
        let ttl = match v {
            AndroidApp::Available(_) => SUCCESS_TTL,
            AndroidApp::Unavailable => FAILURE_TTL,
        };
        (now.saturating_duration_since(*at) < ttl).then(|| v.clone())
    }

    pub fn store(&mut self, now: Instant, v: AndroidApp) {
        self.entry = Some((now, v));
    }
}

static CACHE: LazyLock<tokio::sync::Mutex<Cache>> = LazyLock::new(Default::default);

async fn fetch() -> Result<AndroidRelease, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(TIMEOUT)
        .https_only(true)
        .user_agent(concat!("rynas/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let mut resp = client.get(ANDROID_METADATA_URL).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    if resp.content_length().is_some_and(|n| n > MAX_BODY as u64) {
        return Err(format!("larger than {MAX_BODY} bytes"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        if body.len() + chunk.len() > MAX_BODY {
            return Err(format!("larger than {MAX_BODY} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
    parse(&body)
}

/// The app as last read, read again when the kept answer is stale.
pub async fn current() -> AndroidApp {
    let mut cache = CACHE.lock().await;
    if let Some(v) = cache.fresh(Instant::now()) {
        return v;
    }
    let v = match fetch().await {
        Ok(r) => AndroidApp::Available(r),
        Err(e) => {
            tracing::warn!("android app: cannot read {ANDROID_METADATA_URL}: {e}");
            AndroidApp::Unavailable
        }
    };
    cache.store(Instant::now(), v.clone());
    v
}

async fn app_get() -> Json<AndroidApp> {
    Json(current().await)
}

pub fn router() -> Router {
    Router::new().route("/v1/devices/android-app", get(app_get))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "4f2c0e6b1a9d8c7b6a5f4e3d2c1b0a99887766554433221100ffeeddccbbaa00";

    fn json(version: &str, size: u64, sha: &str, signer: &str, url: &str) -> Vec<u8> {
        serde_json::json!({
            "version": version, "version_code": 88, "size": size, "sha256": sha,
            "signer_sha256": signer, "url": url, "published_ms": 1_795_000_000_000u64,
        })
        .to_string()
        .into_bytes()
    }

    const URL: &str = "https://github.com/rylos/rynas/releases/download/android/rynas-android.apk";

    #[test]
    fn a_good_file_is_read() {
        let r = parse(&json("0.2.0", 11_526_584, SHA, &SHA.to_uppercase(), URL)).unwrap();
        assert_eq!(r.version, "0.2.0");
        assert_eq!(r.size, 11_526_584);
        assert_eq!(r.sha256, SHA);
        assert_eq!(r.signer_sha256, SHA, "hex is lowercased");
        assert_eq!(r.url, URL);
    }

    #[test]
    fn addresses_outside_the_projects_releases_are_refused() {
        for url in [
            "http://github.com/rylos/rynas/releases/download/android/rynas-android.apk",
            "https://github.com/other/rynas/releases/download/android/rynas-android.apk",
            "https://github.com/rylos/rynas/releases/download/",
            "https://github.com/rylos/rynas/releases/download/../../../evil/x.apk",
            "https://github.com/rylos/rynas/releases/download/android//x.apk",
            "https://github.com/rylos/rynas/releases/download/android/x y.apk",
            "https://github.com/rylos/rynas/releases/download/android/x.apk?y",
            "https://github.com/rylos/rynas/releases/download/android/\"><script>",
            "https://github.com.evil.example/rylos/rynas/releases/download/android/x.apk",
            "javascript:alert(1)",
        ] {
            assert!(parse(&json("0.2.0", 11_526_584, SHA, SHA, url)).is_err(), "{url}");
        }
    }

    #[test]
    fn bad_versions_sizes_and_hashes_are_refused() {
        let long = "1".repeat(33);
        for v in ["", " ", "0.2.0<b>", long.as_str()] {
            assert!(parse(&json(v, 11_526_584, SHA, SHA, URL)).is_err(), "{v:?}");
        }
        for size in [0, MIN_APK - 1, MAX_APK + 1] {
            assert!(parse(&json("0.2.0", size, SHA, SHA, URL)).is_err(), "{size}");
        }
        let not_hex = format!("{}g", &SHA[1..]);
        for sha in ["", &SHA[1..], not_hex.as_str()] {
            assert!(parse(&json("0.2.0", 11_526_584, sha, SHA, URL)).is_err(), "{sha}");
            assert!(parse(&json("0.2.0", 11_526_584, SHA, sha, URL)).is_err(), "{sha}");
        }
        assert!(parse(b"not json").is_err());
        assert!(parse(br#"{"version":"0.2.0"}"#).is_err(), "fields missing");
        let mut big = json("0.2.0", 11_526_584, SHA, SHA, URL);
        big.resize(MAX_BODY + 1, b' ');
        assert!(parse(&big).is_err(), "too large");
    }

    #[test]
    fn answers_are_kept_six_hours_failures_ten_minutes() {
        let t0 = Instant::now();
        let mut c = Cache::default();
        assert_eq!(c.fresh(t0), None);
        let ok = AndroidApp::Available(parse(&json("0.2.0", 11_526_584, SHA, SHA, URL)).unwrap());
        c.store(t0, ok.clone());
        assert_eq!(c.fresh(t0 + SUCCESS_TTL - Duration::from_secs(1)), Some(ok));
        assert_eq!(c.fresh(t0 + SUCCESS_TTL), None);
        c.store(t0, AndroidApp::Unavailable);
        assert_eq!(c.fresh(t0 + FAILURE_TTL - Duration::from_secs(1)), Some(AndroidApp::Unavailable));
        assert_eq!(c.fresh(t0 + FAILURE_TTL), None);
    }

    #[test]
    fn the_answer_is_tagged() {
        assert_eq!(serde_json::to_string(&AndroidApp::Unavailable).unwrap(), r#"{"state":"unavailable"}"#);
        let ok = AndroidApp::Available(parse(&json("0.2.0", 11_526_584, SHA, SHA, URL)).unwrap());
        let v: serde_json::Value = serde_json::to_value(&ok).unwrap();
        assert_eq!(v["state"], "available");
        assert_eq!(v["version"], "0.2.0");
        assert_eq!(serde_json::from_value::<AndroidApp>(v).unwrap(), ok);
    }
}
