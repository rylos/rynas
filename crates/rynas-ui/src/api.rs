//! The one place that talks HTTP to rynas-web.
//!
//! - Every request other than GET carries the session's CSRF token (`X-CSRF-Token`), kept in
//!   memory only (see [`crate::auth`]).
//! - A 401 from any endpoint except the login itself means the session is gone: the app goes
//!   back to the login screen without reloading (the current route is kept).
//! - Errors keep the server's `ApiError.code` and the `Retry-After` header for the UI.
//! - Requests made while the user is away — no key, click or wheel for [`USER_ACTIVE_MS`]:
//!   pollers, timers, reconnects — carry `X-Rynas-Background: 1`, so they do not renew the
//!   session's idle timeout (DSM model: an unattended open tab is signed out). A request
//!   started by a user action is always within that window.

use std::cell::Cell;

use leptos::prelude::window;
use rynas_api_types::{BACKGROUND_HEADER, CSRF_HEADER, error_codes};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Headers, RequestInit, Response};

#[derive(Clone, Debug, PartialEq)]
pub enum ApiErr {
    /// No answer at all (server down, network gone).
    Network,
    /// No answer within [`TIMEOUT_MS`].
    Timeout,
    /// The server answered with an error status. `message` is English, for details only
    /// (never parsed); `field` names the invalid form field when the server says which.
    Http { status: u16, code: String, retry_after: Option<u32>, message: String, field: Option<String> },
    /// The answer was not what we expected.
    Decode,
}

impl ApiErr {
    pub fn code(&self) -> &str {
        match self {
            ApiErr::Http { code, .. } => code,
            ApiErr::Network => "network",
            ApiErr::Timeout => error_codes::TIMEOUT,
            ApiErr::Decode => "decode",
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            ApiErr::Http { status, .. } => *status,
            _ => 0,
        }
    }

    /// The server's English detail (shown under the translated text, never parsed).
    pub fn message(&self) -> &str {
        match self {
            ApiErr::Http { message, .. } => message,
            _ => "",
        }
    }

    /// Form field the error is about (`ApiError.field`, guidelines A10), when given.
    pub fn field(&self) -> Option<&str> {
        match self {
            ApiErr::Http { field, .. } => field.as_deref(),
            _ => None,
        }
    }

    pub fn retry_after(&self) -> Option<u32> {
        match self {
            ApiErr::Http { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// The one error text of the UI (guidelines A10): the translated code (`err.<code>`) and,
    /// when the server sent one, its English message as a secondary detail after
    /// [`DETAIL_SEP`]. [`crate::ui::err_view`] (form errors, toasts, notices, field errors)
    /// shows the detail apart, smaller and marked `lang="en"`; it is never the main text.
    pub fn text(&self) -> String {
        let main = crate::i18n::err_text(self.code());
        match self.message() {
            "" => main.to_string(),
            m => format!("{main}{DETAIL_SEP}{m}"),
        }
    }
}

/// Separates the translated text of an error from the server's detail (see [`ApiErr::text`]).
pub const DETAIL_SEP: char = '\u{1f}';

/// Translated text and optional detail of a message built by [`ApiErr::text`].
pub fn split_detail(msg: &str) -> (&str, Option<&str>) {
    match msg.split_once(DETAIL_SEP) {
        Some((a, b)) => (a, Some(b)),
        None => (msg, None),
    }
}

/// Wire form of `ApiError`, with the optional `field` of guidelines A10 (ignored when absent).
#[derive(Deserialize, Default)]
struct WireError {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    field: Option<String>,
}

thread_local! {
    /// `Date` header of the last answer, in ms (to estimate the clock skew with the server).
    static LAST_DATE: Cell<Option<f64>> = const { Cell::new(None) };
    /// When the wasm started ([`init`]).
    static STARTED: Cell<f64> = const { Cell::new(0.0) };
    /// Time of the last key, click or wheel event (ms, `Date.now()`); page load counts.
    static LAST_INPUT: Cell<f64> = const { Cell::new(0.0) };
}

/// A request within this long after the last user input renews the session's idle timeout;
/// later ones are background requests.
pub const USER_ACTIVE_MS: f64 = 60_000.0;

fn user_active() -> bool {
    js_sys::Date::now() - LAST_INPUT.with(Cell::get) < USER_ACTIVE_MS
}

/// Starts tracking user input for the background marker (called once from `main`).
pub fn init() {
    STARTED.with(|c| c.set(js_sys::Date::now()));
    let mark = |_| LAST_INPUT.with(|c| c.set(js_sys::Date::now()));
    mark(());
    // registered once for the life of the page
    let _ = leptos::prelude::window_event_listener(leptos::ev::keydown, move |_| mark(()));
    let _ = leptos::prelude::window_event_listener(leptos::ev::pointerdown, move |_| mark(()));
    let _ = leptos::prelude::window_event_listener(leptos::ev::wheel, move |_| mark(()));
}

/// Server time of the last answer (ms since the epoch), when it had a `Date` header.
pub fn last_server_date() -> Option<f64> {
    LAST_DATE.with(Cell::get)
}

async fn text(resp: &Response) -> Option<String> {
    JsFuture::from(resp.text().ok()?).await.ok()?.as_string()
}

/// Every request gives up after this long (the API answers in milliseconds; long work is a task).
pub const TIMEOUT_MS: u32 = 30_000;

/// How long after the page started the requests of `index.html` can stand for a new one.
const EARLY_MAX_MS: f64 = 10_000.0;

/// Drops the requests `index.html` started: they belong to the session of that moment (called
/// when a session ends, so a new sign-in does not reuse a 401).
pub fn forget_early() {
    let _ = js_sys::Reflect::delete_property(window().unchecked_ref::<js_sys::Object>(), &"__rynasEarly".into());
}

/// The request `index.html` started for this GET while the wasm was loading
/// (`window.__rynasEarly`), taken only once: a later reload of the same URL asks the server again.
fn early_answer(method: &str, url: &str) -> Option<js_sys::Promise> {
    if method != "GET" {
        return None;
    }
    // for the page's own start only, not for a later resume with data that old
    if js_sys::Date::now() - STARTED.with(Cell::get) > EARLY_MAX_MS {
        forget_early();
        return None;
    }
    let early = js_sys::Reflect::get(&window(), &"__rynasEarly".into()).ok()?;
    if !early.is_object() {
        return None;
    }
    let p = js_sys::Reflect::get(&early, &url.into()).ok()?.dyn_into::<js_sys::Promise>().ok()?;
    let _ = js_sys::Reflect::delete_property(early.unchecked_ref::<js_sys::Object>(), &url.into());
    Some(p)
}

async fn send(method: &str, url: &str, body: Option<String>) -> Result<Response, ApiErr> {
    let init = RequestInit::new();
    init.set_method(method);
    let headers = Headers::new().map_err(|_| ApiErr::Network)?;
    let _ = headers.set("Accept", "application/json");
    if let Some(b) = body {
        let _ = headers.set("Content-Type", "application/json");
        init.set_body(&JsValue::from_str(&b));
    }
    if method != "GET"
        && let Some(token) = crate::auth::csrf_token()
    {
        let _ = headers.set(CSRF_HEADER, &token);
    }
    if !user_active() {
        let _ = headers.set(BACKGROUND_HEADER, "1");
    }
    init.set_headers(&headers);
    init.set_signal(Some(&web_sys::AbortSignal::timeout_with_u32(TIMEOUT_MS)));
    let call = early_answer(method, url).unwrap_or_else(|| window().fetch_with_str_and_init(url, &init));
    let resp: Response = JsFuture::from(call)
        .await
        .map_err(|e| {
            let name = js_sys::Reflect::get(&e, &JsValue::from_str("name")).ok().and_then(|n| n.as_string());
            if name.as_deref() == Some("TimeoutError") { ApiErr::Timeout } else { ApiErr::Network }
        })?
        .unchecked_into();
    if let Ok(Some(d)) = resp.headers().get("Date") {
        let ms = js_sys::Date::parse(&d);
        if ms.is_finite() {
            LAST_DATE.with(|c| c.set(Some(ms)));
        }
    }
    if resp.ok() {
        return Ok(resp);
    }
    let status = resp.status();
    let retry_after = resp.headers().get("Retry-After").ok().flatten().and_then(|v| v.trim().parse().ok());
    let err: WireError = text(&resp).await.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let login_answer = matches!(
        err.code.as_str(),
        error_codes::INVALID_CREDENTIALS | error_codes::TOTP_REQUIRED | error_codes::INVALID_TOTP
    );
    if status == 401 && !login_answer && !url.ends_with("/auth/login") {
        crate::auth::session_lost();
    }
    Err(ApiErr::Http { status, code: err.code, retry_after, message: err.message, field: err.field })
}

async fn json<T: DeserializeOwned>(resp: Response) -> Result<T, ApiErr> {
    let t = text(&resp).await.ok_or(ApiErr::Decode)?;
    serde_json::from_str(&t).map_err(|_| ApiErr::Decode)
}

pub async fn get<T: DeserializeOwned>(url: &str) -> Result<T, ApiErr> {
    json(send("GET", url, None).await?).await
}

/// GET the body as text (non-generic: the caller parses it, see [`crate::ui::Remote`]).
pub async fn get_text(url: &str) -> Result<String, ApiErr> {
    text(&send("GET", url, None).await?).await.ok_or(ApiErr::Decode)
}

/// POST with an optional JSON body, JSON answer.
pub async fn post<B: Serialize, T: DeserializeOwned>(url: &str, body: Option<&B>) -> Result<T, ApiErr> {
    let b = body.map(|b| serde_json::to_string(b).unwrap_or_default());
    json(send("POST", url, b).await?).await
}

/// POST whose answer has no body (204).
pub async fn post_empty<B: Serialize>(url: &str, body: Option<&B>) -> Result<(), ApiErr> {
    let b = body.map(|b| serde_json::to_string(b).unwrap_or_default());
    send("POST", url, b).await.map(|_| ())
}

/// PUT with a JSON body, JSON answer (staging a configuration object: `PUT /api/v1/<domain>`).
/// Call [`crate::config::changed`] afterwards so the pending-changes bar picks the change up.
pub async fn put<B: Serialize, T: DeserializeOwned>(url: &str, body: &B) -> Result<T, ApiErr> {
    json(send("PUT", url, Some(serde_json::to_string(body).unwrap_or_default())).await?).await
}

/// POST to an endpoint that starts a background task (202 `TaskStarted`): the task id.
/// Most callers want [`crate::tasks::run_task`], which also follows the task to its end.
pub async fn start_task(url: &str, body: Option<String>) -> Result<String, ApiErr> {
    json::<rynas_api_types::TaskStarted>(send("POST", url, body).await?).await.map(|t| t.task_id)
}

/// One chunk of a task log (`GET /api/v1/tasks/{id}/log?offset=N`).
pub struct LogChunk {
    pub text: String,
    /// Offset to ask for next (`X-Rynas-Next-Offset`).
    pub next: u64,
    /// The task has ended and the log was read to the end (`X-Rynas-Log-Complete`).
    pub complete: bool,
}

/// Read a task log from byte `offset` (at most 64 KiB per call).
pub async fn task_log(id: &str, offset: u64) -> Result<LogChunk, ApiErr> {
    let url = format!("/api/v1/tasks/{}/log?offset={offset}&limit=65536", encode(id));
    let resp = send("GET", &url, None).await?;
    let h = resp.headers();
    let next = h.get(rynas_api_types::TASK_LOG_NEXT_OFFSET).ok().flatten().and_then(|v| v.parse().ok());
    let complete = h.get(rynas_api_types::TASK_LOG_COMPLETE).ok().flatten().is_some_and(|v| v == "true");
    let text = text(&resp).await.unwrap_or_default();
    let next = next.unwrap_or(offset + text.len() as u64);
    Ok(LogChunk { text, next, complete })
}

/// Percent-encode a path segment (task ids contain `:`).
pub fn encode(segment: &str) -> String {
    js_sys::encode_uri_component(segment).as_string().unwrap_or_default()
}

/// Wait `ms` milliseconds without blocking (timer-backed future).
pub async fn sleep(ms: u32) {
    let p = js_sys::Promise::new(&mut |resolve, _| {
        let _ = window().set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms as i32);
    });
    let _ = JsFuture::from(p).await;
}

/// DELETE with a JSON answer (e.g. 202 `TaskStarted`).
pub async fn delete<T: DeserializeOwned>(url: &str) -> Result<T, ApiErr> {
    json(send("DELETE", url, None).await?).await
}

/// DELETE whose answer has no body (204).
pub async fn delete_empty(url: &str) -> Result<(), ApiErr> {
    send("DELETE", url, None).await.map(|_| ())
}

/// GET a binary answer and hand it to the browser as a download named `file`.
pub async fn download(url: &str, file: &str) -> Result<(), ApiErr> {
    let resp = send("GET", url, None).await?;
    let blob: web_sys::Blob =
        JsFuture::from(resp.blob().map_err(|_| ApiErr::Decode)?).await.map_err(|_| ApiErr::Decode)?.unchecked_into();
    save_blob(file, &blob);
    Ok(())
}

/// Text made in the browser (recovery codes, a recovery key) saved as a file named `file`.
pub fn save_text(file: &str, text: &str) {
    let parts = js_sys::Array::of1(&JsValue::from_str(text));
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type("text/plain;charset=utf-8");
    if let Ok(blob) = web_sys::Blob::new_with_str_sequence_and_options(&parts, &opts) {
        save_blob(file, &blob);
    }
}

/// Hand a Blob to the browser as a download. The object URL lives in memory only and is
/// revoked a minute later (revoking right after `click()` can cancel the download in Firefox
/// and Safari).
pub fn save_blob(file: &str, blob: &web_sys::Blob) {
    let Ok(href) = web_sys::Url::create_object_url_with_blob(blob) else { return };
    if let Ok(a) = leptos::prelude::document().create_element("a") {
        let a: web_sys::HtmlAnchorElement = a.unchecked_into();
        a.set_href(&href);
        a.set_download(file);
        a.click();
    }
    leptos::prelude::set_timeout(
        move || {
            let _ = web_sys::Url::revoke_object_url(&href);
        },
        std::time::Duration::from_secs(60),
    );
}

