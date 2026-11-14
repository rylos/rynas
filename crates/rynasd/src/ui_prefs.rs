//! Preferences of the web interface kept for each user (`/v1/prefs/…`): today the sidebar
//! layout ([`rynas_api_types::ui_prefs`]). Not configuration: no changeset, revision or
//! backup archive; each user reads and writes only their own, as the user and realm
//! rynas-web names in `x-rynas-user` and `x-rynas-realm` (the client never names them).
//!
//! One file per user and preference, `<state_dir>/ui-prefs/<key>/sidebar.json` (0600, in
//! 0700 folders, written through a temporary file and a rename), where `<key>` is the SHA-256
//! (hex) of the realm and the name ([`owner_key`]): domain names (`DOMAIN\user`,
//! `user@realm`) need no file-name rules, and users of the same name in two realms never
//! share a file. The default layout is never stored, so the defaults of later releases reach
//! users who never changed theirs. A stored layout that no longer validates (written by a
//! newer release before a downgrade) is still answered: the interface merges it with its own
//! default ([`rynas_api_types::ui_prefs::merge`]).

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use rynas_api_types::error_codes;
use rynas_api_types::ui_prefs::{SIDEBAR_MAX_BYTES, SidebarLayout, SidebarPrefs};
use rynas_auth::ipc::{REALM_HEADER, USER_HEADER};
use sha2::{Digest, Sha256};

use crate::ops::{Error, Res};

/// Where the preferences live.
pub struct UiPrefs {
    dir: PathBuf,
}

pub type PrefsState = Option<Arc<UiPrefs>>;

const SIDEBAR_FILE: &str = "sidebar.json";
/// Longest user name and realm accepted from rynas-web, in bytes.
const NAME_MAX: usize = 256;
const REALM_MAX: usize = 64;

/// The folder name of a user's preferences: SHA-256 (lowercase hex) of the realm and the name,
/// the realm's length first so that no two pairs give the same text.
pub fn owner_key(realm: &str, user: &str) -> String {
    let digest = Sha256::digest(format!("{}:{realm}:{user}", realm.len()).as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The user rynas-web acts for and its realm, as a file key: both present, not empty, within
/// their length limit and without control characters (any other character is fine: the key
/// is a hash).
fn owner(h: &HeaderMap) -> Res<String> {
    let get = |name: &str, max: usize| {
        h.get(name)
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control))
    };
    match (get(REALM_HEADER, REALM_MAX), get(USER_HEADER, NAME_MAX)) {
        (Some(realm), Some(user)) => Ok(owner_key(realm, user)),
        _ => Err(Error::new(
            StatusCode::BAD_REQUEST,
            error_codes::BAD_REQUEST,
            format!("missing or invalid {USER_HEADER} or {REALM_HEADER} header"),
        )),
    }
}

impl UiPrefs {
    /// Preferences under `dir` (created on the first write).
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn file(&self, key: &str) -> PathBuf {
        // `key` is a hex digest ([`owner_key`])
        self.dir.join(key).join(SIDEBAR_FILE)
    }

    /// The saved layout of the owner `key`, `None` without one (or with an unreadable one,
    /// logged).
    pub fn sidebar(&self, key: &str) -> io::Result<Option<SidebarLayout>> {
        let p = self.file(key);
        let bytes = match std::fs::read(&p) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        match serde_json::from_slice::<SidebarLayout>(&bytes) {
            Ok(l) => Ok(Some(l)),
            Err(e) => {
                tracing::warn!("ui prefs: {} unreadable, ignored: {e}", p.display());
                Ok(None)
            }
        }
    }

    /// Stores `layout` for the owner `key` (validated by the caller); the default one removes
    /// the file.
    pub fn set_sidebar(&self, key: &str, layout: &SidebarLayout) -> io::Result<()> {
        if layout.is_default() {
            return self.clear_sidebar(key);
        }
        let data = serde_json::to_vec(layout).map_err(io::Error::other)?;
        let dir = self.dir.join(key);
        make_private_dir(&self.dir)?;
        make_private_dir(&dir)?;
        crate::tls::write_atomic(&dir.join(SIDEBAR_FILE), &data, 0o600, None)
    }

    /// Back to the default layout.
    pub fn clear_sidebar(&self, key: &str) -> io::Result<()> {
        match std::fs::remove_file(self.file(key)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

fn make_private_dir(dir: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(e),
        _ => Ok(()),
    }
}

/// `GET|PUT|DELETE /v1/prefs/sidebar`: every signed-in user, for themselves.
pub fn router(st: PrefsState) -> Router {
    Router::new()
        .route("/v1/prefs/sidebar", get(sidebar_get).put(sidebar_put).delete(sidebar_delete))
        // the cap is the whole JSON body (413 above it)
        .layer(DefaultBodyLimit::max(SIDEBAR_MAX_BYTES))
        .with_state(st)
}

fn prefs(st: &PrefsState) -> Res<Arc<UiPrefs>> {
    st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "preferences are not available".into(),
        )
    })
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> io::Result<T> + Send + 'static) -> Res<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(Error::internal)?
        .map_err(Error::internal)
}

async fn sidebar_get(State(st): State<PrefsState>, hd: HeaderMap) -> Res<Json<SidebarPrefs>> {
    let (p, key) = (prefs(&st)?, owner(&hd)?);
    let layout = blocking(move || p.sidebar(&key)).await?;
    Ok(Json(SidebarPrefs { layout }))
}

async fn sidebar_put(
    State(st): State<PrefsState>,
    hd: HeaderMap,
    body: Result<Json<SidebarLayout>, JsonRejection>,
) -> Res<Json<SidebarPrefs>> {
    let (p, key) = (prefs(&st)?, owner(&hd)?);
    let Json(layout) = body?;
    layout
        .validate()
        .map_err(|e| Error::invalid(e.field, e.message))?;
    let stored = (!layout.is_default()).then(|| layout.clone());
    blocking(move || p.set_sidebar(&key, &layout)).await?;
    Ok(Json(SidebarPrefs { layout: stored }))
}

async fn sidebar_delete(State(st): State<PrefsState>, hd: HeaderMap) -> Res<StatusCode> {
    let (p, key) = (prefs(&st)?, owner(&hd)?);
    blocking(move || p.clear_sidebar(&key)).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use rynas_api_types::ui_prefs::SidebarPin;
    use std::os::unix::fs::PermissionsExt;
    use tower::ServiceExt;

    fn app(dir: &std::path::Path) -> Router {
        router(Some(Arc::new(UiPrefs::new(dir.join("ui-prefs")))))
    }

    /// A request as rynas-web sends it: `who` = (realm, user).
    async fn call(app: &Router, method: &str, who: Option<(&str, &str)>, body: Option<String>) -> (StatusCode, serde_json::Value) {
        let mut b = Request::builder().method(method).uri("/v1/prefs/sidebar");
        if let Some((realm, user)) = who {
            b = b.header(REALM_HEADER, realm).header(USER_HEADER, user);
        }
        let req = match body {
            Some(s) => b.header("content-type", "application/json").body(Body::from(s)).unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let r = app.clone().oneshot(req).await.unwrap();
        let status = r.status();
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    fn custom() -> SidebarLayout {
        let mut l = SidebarLayout::default_layout();
        l.hidden.push("snapshots".into());
        l.pins.push(SidebarPin {
            id: "p1".into(),
            path: "/system/scheduler".into(),
            label: "Task Scheduler".into(),
            icon: "calendar-clock".into(),
        });
        l.groups[0].push("p1".into());
        l
    }

    const ALICE: (&str, &str) = ("pam", "alice");

    #[test]
    fn keys_tell_realms_apart() {
        assert_ne!(owner_key("pam", "alice"), owner_key("rynas", "alice"));
        // the realm's length keeps "a:b" + "c" apart from "a" + "b:c"
        assert_ne!(owner_key("a:b", "c"), owner_key("a", "b:c"));
        let k = owner_key("pam", "EXAMPLE\\alice");
        assert_eq!(k.len(), 64);
        assert!(k.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    }

    #[tokio::test]
    async fn per_user_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(tmp.path());
        let (s, v) = call(&app, "GET", Some(ALICE), None).await;
        assert_eq!((s, v), (StatusCode::OK, serde_json::json!({ "layout": null })));

        let body = serde_json::to_string(&custom()).unwrap();
        let (s, v) = call(&app, "PUT", Some(ALICE), Some(body)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["layout"]["pins"][0]["id"], "p1");
        let (_, v) = call(&app, "GET", Some(ALICE), None).await;
        let got: SidebarPrefs = serde_json::from_value(v).unwrap();
        assert_eq!(got.layout, Some(custom()));
        // private file in private folders
        let d = tmp.path().join("ui-prefs").join(owner_key("pam", "alice"));
        let f = d.join("sidebar.json");
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o700);

        // another user, or the same name in another realm, sees nothing of it
        let (_, v) = call(&app, "GET", Some(("pam", "bob")), None).await;
        assert_eq!(v, serde_json::json!({ "layout": null }));
        let (_, v) = call(&app, "GET", Some(("rynas", "alice")), None).await;
        assert_eq!(v, serde_json::json!({ "layout": null }));

        // the default layout is not stored
        let body = serde_json::to_string(&SidebarLayout::default_layout()).unwrap();
        let (s, v) = call(&app, "PUT", Some(ALICE), Some(body)).await;
        assert_eq!((s, v), (StatusCode::OK, serde_json::json!({ "layout": null })));
        assert!(!f.exists());

        // reset
        call(&app, "PUT", Some(ALICE), Some(serde_json::to_string(&custom()).unwrap())).await;
        let (s, _) = call(&app, "DELETE", Some(ALICE), None).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        assert!(!f.exists());
        let (s, _) = call(&app, "DELETE", Some(ALICE), None).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn domain_users() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(tmp.path());
        let body = serde_json::to_string(&custom()).unwrap();
        for who in [("pam", "EXAMPLE\\alice"), ("pam", "alice@example.lan"), ("pam", "../../etc/x")] {
            let (s, _) = call(&app, "PUT", Some(who), Some(body.clone())).await;
            assert_eq!(s, StatusCode::OK, "{who:?}");
            let (_, v) = call(&app, "GET", Some(who), None).await;
            assert_eq!(v["layout"]["pins"][0]["id"], "p1");
        }
        // everything stays in folders named by the hash
        let names: Vec<String> = std::fs::read_dir(tmp.path().join("ui-prefs"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 3);
        assert!(names.iter().all(|n| n.len() == 64));
        assert!(!tmp.path().join("etc").exists());
    }

    #[tokio::test]
    async fn refusals() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(tmp.path());
        // no identity, no realm, an empty or overlong name
        assert_eq!(call(&app, "GET", None, None).await.0, StatusCode::BAD_REQUEST);
        let r = Request::builder().uri("/v1/prefs/sidebar").header(USER_HEADER, "alice").body(Body::empty()).unwrap();
        assert_eq!(app.clone().oneshot(r).await.unwrap().status(), StatusCode::BAD_REQUEST);
        assert_eq!(call(&app, "GET", Some(("pam", "")), None).await.0, StatusCode::BAD_REQUEST);
        let long = "a".repeat(NAME_MAX + 1);
        assert_eq!(call(&app, "GET", Some(("pam", &long)), None).await.0, StatusCode::BAD_REQUEST);

        let mut bad = custom();
        bad.pins[0].path = "https://evil.example".into();
        let (s, v) = call(&app, "PUT", Some(ALICE), Some(serde_json::to_string(&bad).unwrap())).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!((v["code"].as_str(), v["field"].as_str()), (Some(error_codes::INVALID_VALUE), Some("pins")));

        let mut bad = custom();
        bad.groups[1].push("bogus".into());
        let (s, v) = call(&app, "PUT", Some(ALICE), Some(serde_json::to_string(&bad).unwrap())).await;
        assert_eq!((s, v["field"].as_str()), (StatusCode::BAD_REQUEST, Some("groups")));

        let mut bad = custom();
        bad.version = 2;
        let (s, v) = call(&app, "PUT", Some(ALICE), Some(serde_json::to_string(&bad).unwrap())).await;
        assert_eq!((s, v["field"].as_str()), (StatusCode::BAD_REQUEST, Some("version")));

        // over the size cap
        let mut big = custom();
        big.known = vec!["x".repeat(SIDEBAR_MAX_BYTES)];
        let (s, v) = call(&app, "PUT", Some(ALICE), Some(serde_json::to_string(&big).unwrap())).await;
        assert_eq!((s, v["code"].as_str()), (StatusCode::PAYLOAD_TOO_LARGE, Some(error_codes::PAYLOAD_TOO_LARGE)));

        let (s, _) = call(&app, "PUT", Some(ALICE), Some("{".into())).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(!tmp.path().join("ui-prefs").exists());
    }

    #[tokio::test]
    async fn unreadable_file_is_no_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(tmp.path());
        let d = tmp.path().join("ui-prefs").join(owner_key("pam", "alice"));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("sidebar.json"), b"not json").unwrap();
        let (s, v) = call(&app, "GET", Some(ALICE), None).await;
        assert_eq!((s, v), (StatusCode::OK, serde_json::json!({ "layout": null })));
    }
}
