//! The web interface: files from `--ui-dir` (trunk output), SPA fallback to `index.html`.
//!
//! - `index.html` is never cached (`no-cache`) and carries a CSP whose `script-src` lists the
//!   SHA-256 of its inline scripts (trunk's loader, the theme script), computed from the file
//!   itself: no `'unsafe-inline'` for scripts.
//!   It is read and hashed once, and again only when its modification time changes.
//! - Assets with a content hash in the name (trunk: `name-<16 hex>.ext`) are immutable for a
//!   year; fonts (not hashed by trunk) are cached for a day; other files are revalidated
//!   (`no-cache`, `Last-Modified` from `ServeDir`).
//! - `.br`/`.gz` siblings, if the build produced them, are served as is; otherwise the
//!   compression layer of the app gzips on the fly (level 4, bodies over 1 KiB).
//! - `files-app/` is the file manager (a Web Component the Files page loads on demand): plain
//!   ES modules without hashes, revalidated (`no-cache` + `Last-Modified`).
//! - `/files/<path>` is a route of the interface even when the last segment has a dot (a
//!   folder `photos.2024`): it gets `index.html`, not a 404.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use base64::Engine;
use sha2::{Digest, Sha256};
use tower_http::services::ServeDir;

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const ONE_DAY: &str = "public, max-age=86400";
const NO_CACHE: &str = "no-cache";

/// CSP of every response; `index.html` replaces `{scripts}` with its inline script hashes.
/// `blob:` images and media: previews the file manager makes in the browser (HEIC photos
/// decoded to JPEG in a worker).
pub const CSP_TEMPLATE: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'{scripts}; \
style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; media-src 'self' blob:; font-src 'self' data:; \
connect-src 'self'; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'";

/// A user's file shown inside the file manager (`GET /api/v1/files/view`: a PDF in an
/// iframe, an image, a video): framed by rynas itself only, and nothing in it can run a script
/// (an HTML or SVG file opened there is inert). `object-src 'self'` is what the browsers'
/// PDF viewers need.
pub const FILE_VIEW_CSP: &str = "default-src 'none'; img-src 'self' data: blob:; media-src 'self'; \
style-src 'unsafe-inline'; font-src 'self' data:; object-src 'self'; frame-ancestors 'self'; \
base-uri 'none'; form-action 'none'";

/// Headers of [`FILE_VIEW_CSP`] on the file manager's `view` answers (set before the app-wide
/// defaults, which are only added when absent).
pub async fn file_view_headers(req: Request, next: Next) -> Response {
    let view = req.uri().path() == "/api/v1/files/view";
    let mut r = next.run(req).await;
    if view && r.status().is_success() {
        let h = r.headers_mut();
        h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(FILE_VIEW_CSP));
        h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("SAMEORIGIN"));
    }
    r
}

/// Routes of the interface whose last segment may contain a dot (folder names).
pub fn is_ui_route(path: &str) -> bool {
    path == "/files" || path.starts_with("/files/")
}

pub fn default_csp() -> String {
    CSP_TEMPLATE.replace("{scripts}", "")
}

/// Contents of the inline `<script>` elements (those without `src`).
pub fn inline_scripts(html: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<script") {
        let after = &rest[start + 7..];
        let Some(tag_end) = after.find('>') else {
            break;
        };
        let attrs = &after[..tag_end];
        let body = &after[tag_end + 1..];
        let Some(end) = body.find("</script>") else {
            break;
        };
        if !attrs.contains("src=") && !body[..end].is_empty() {
            out.push(&body[..end]);
        }
        rest = &body[end + 9..];
    }
    out
}

pub fn csp_for(html: &str) -> String {
    let hashes: String = inline_scripts(html)
        .into_iter()
        .map(|s| {
            let h = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(s.as_bytes()));
            format!(" 'sha256-{h}'")
        })
        .collect();
    CSP_TEMPLATE.replace("{scripts}", &hashes)
}

/// Whether a file name carries a content hash (trunk: `rynas-ui-0123456789abcdef_bg.wasm`).
pub fn is_hashed(path: &str) -> bool {
    let file = path.rsplit('/').next().unwrap_or(path);
    let stem = file.split('.').next().unwrap_or(file);
    stem.split(['-', '_'])
        .any(|p| p.len() >= 16 && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The UI directory and the last `index.html` read from it.
struct Shell {
    dir: PathBuf,
    cached: Mutex<Option<CachedIndex>>,
}

#[derive(Clone)]
struct CachedIndex {
    mtime: SystemTime,
    html: Arc<str>,
    csp: Option<HeaderValue>,
}

impl Shell {
    /// `index.html` with its CSP: from memory while the file's mtime is unchanged.
    async fn index(&self) -> std::io::Result<CachedIndex> {
        let path = self.dir.join("index.html");
        let mtime = tokio::fs::metadata(&path).await?.modified()?;
        if let Some(c) = self
            .cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|c| c.mtime == mtime)
        {
            return Ok(c.clone());
        }
        let html = tokio::fs::read_to_string(&path).await?;
        let c = CachedIndex {
            mtime,
            csp: HeaderValue::from_str(&csp_for(&html)).ok(),
            html: html.into(),
        };
        *self.cached.lock().unwrap_or_else(|e| e.into_inner()) = Some(c.clone());
        Ok(c)
    }
}

async fn index(State(shell): State<Arc<Shell>>, uri: Uri) -> Response {
    let path = uri.path();
    let last = path.rsplit('/').next().unwrap_or("");
    if path != "/index.html" && last.contains('.') && !is_ui_route(path) {
        // a missing asset is a 404, not the SPA shell
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    match shell.index().await {
        Ok(c) => {
            let mut r = Html(c.html.to_string()).into_response();
            let h = r.headers_mut();
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static(NO_CACHE));
            if let Some(v) = c.csp {
                h.insert(header::CONTENT_SECURITY_POLICY, v);
            }
            r
        }
        Err(e) => {
            tracing::warn!("{}/index.html: {e}", shell.dir.display());
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Html("<!doctype html><title>rynas</title><p>The rynas web interface is not installed (--ui-dir).</p>"),
            )
                .into_response()
        }
    }
}

/// Fonts: trunk copies them without a hash, but they change only with a release.
fn is_font(path: &str) -> bool {
    [".woff2", ".woff", ".ttf", ".otf"]
        .iter()
        .any(|e| path.ends_with(e))
}

async fn cache_headers(req: Request, next: Next) -> Response {
    let hashed = is_hashed(req.uri().path());
    let font = is_font(req.uri().path());
    let mut r = next.run(req).await;
    let s = r.status();
    if (s.is_success() || s == StatusCode::NOT_MODIFIED)
        && !r.headers().contains_key(header::CACHE_CONTROL)
    {
        let v = if hashed {
            IMMUTABLE
        } else if font {
            ONE_DAY
        } else {
            NO_CACHE
        };
        r.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static(v));
    }
    r
}

pub fn router(ui_dir: PathBuf) -> Router {
    let shell = Arc::new(Shell {
        dir: ui_dir.clone(),
        cached: Mutex::new(None),
    });
    let shell = get(index).with_state(shell);
    let files = ServeDir::new(&ui_dir)
        .precompressed_br()
        .precompressed_gzip()
        .append_index_html_on_directories(false)
        .fallback(shell.clone());
    Router::new()
        .route("/", shell.clone())
        .route("/index.html", shell)
        .fallback_service(files)
        .layer(middleware::from_fn(cache_headers))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashed_names() {
        assert!(is_hashed("/rynas-ui-6f1e2d3c4b5a6978_bg.wasm"));
        assert!(is_hashed("/rynas-ui-6f1e2d3c4b5a6978.js"));
        assert!(is_hashed("/assets/style-0123456789abcdef.css"));
        assert!(!is_hashed("/favicon.svg"));
        assert!(!is_hashed("/index.html"));
        assert!(!is_hashed("/fonts/inter-var.woff2"));
    }

    #[test]
    fn files_routes_are_the_shell() {
        assert!(is_ui_route("/files"));
        assert!(is_ui_route("/files/photos/2024.summer"));
        assert!(!is_ui_route("/files-app/rynas-files.js"));
        assert!(!is_ui_route("/favicon.svg"));
        assert!(FILE_VIEW_CSP.contains("frame-ancestors 'self'"));
        assert!(!FILE_VIEW_CSP.contains("script-src"));
        assert!(default_csp().contains("img-src 'self' data: blob:"));
    }

    #[test]
    fn csp_hashes_inline_scripts_only() {
        let html = r#"<head><script>document.documentElement.dataset.theme='dark'</script>
<script type="module" src="/app-0123456789abcdef.js"></script>
<script type="module">import init from '/app.js';init();</script></head>"#;
        let s = inline_scripts(html);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0], "document.documentElement.dataset.theme='dark'");
        let csp = csp_for(html);
        assert_eq!(csp.matches("'sha256-").count(), 2);
        // sha256 of "alert(1)" — known vector
        assert!(
            csp_for("<script>alert(1)</script>")
                .contains("'sha256-bhHHL3z2vDgxUt0W3dWQOrprscmda2Y5pLsLg4GF+pI='")
        );
        assert!(!default_csp().contains("sha256"));
    }
}
