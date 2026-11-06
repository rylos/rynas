//! The worker's side of one HTTP request.
//!
//! rynasd answers `/v1/files/...` by starting `rynas-files-worker serve` already switched to
//! the user (see [`crate::identity`]). It writes one JSON line on the worker's stdin — the
//! request ([`WorkerRequest`]: method, route, query, body length, range, and the context
//! rynasd knows: who the user is, the shares, the volumes) — then the request body. The
//! worker answers on stdout in the classic gateway form (`Status: 200 OK`, headers, an empty
//! line, the body), which rynasd turns into the HTTP response and streams on: downloads
//! never sit whole in memory.
//!
//! Errors are rynas's `ApiError` body (`{code, message, field?}`) with the real status code.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use rynas_api_types::error_codes;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Largest JSON body accepted: a selection of a few thousand paths.
const MAX_JSON: usize = 4 * 1024 * 1024;

/// Whether anything read the body yet.
static BODY_TOUCHED: AtomicBool = AtomicBool::new(false);
/// Largest unread body read away before answering: an upload chunk.
const MAX_DISCARD: u64 = 80 * 1024 * 1024;

/// The request body. Every reader takes it from here, so that an answer
/// given without it (a refused chunk) can read it away first: it counts
/// as read from the first read, not from being handed over.
pub fn body() -> Body {
    Body
}

pub struct Body;

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        BODY_TOUCHED.store(true, Ordering::Relaxed);
        io::stdin().lock().read(buf)
    }
}

/// Reads away a body nobody read. A worker that answers and exits with the body still
/// unsent would make rynasd see a broken pipe and the browser a cut connection instead of
/// the answer ("already exists"). A body partly read is left alone: its rest may never come.
pub fn discard_unread(length: u64) {
    if length > 0 && length <= MAX_DISCARD && !BODY_TOUCHED.swap(true, Ordering::Relaxed) {
        let _ = io::copy(&mut io::stdin().lock().take(length), &mut io::sink());
    }
}

/// The first line rynasd writes on a `serve` worker's stdin.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkerRequest {
    pub method: String,
    /// Path below `/v1/files/`, without the leading slash: "list", "edit/save"...
    pub route: String,
    #[serde(default)]
    pub query: HashMap<String, String>,
    /// Bytes of body that follow the line.
    #[serde(default)]
    pub length: u64,
    #[serde(default)]
    pub range: Option<String>,
    pub ctx: crate::Context,
}

pub struct Request {
    pub method: String,
    /// Path below the API, without the leading slash: "list", "edit/save"...
    pub route: String,
    pub query: HashMap<String, String>,
    pub length: u64,
    pub range: Option<String>,
    /// The body, when it is already in memory (rynasd answering a route itself); `None`:
    /// it is on stdin.
    pub body: Option<Vec<u8>>,
}

impl Request {
    /// A request rynasd answers in its own process, its small body already read.
    pub fn in_memory(method: &str, route: &str, query: HashMap<String, String>, body: Vec<u8>) -> Self {
        Self { method: method.into(), route: route.trim_matches('/').into(), query, length: body.len() as u64, range: None, body: Some(body) }
    }

    pub fn new(w: &WorkerRequest) -> Self {
        Self {
            method: w.method.clone(),
            route: w.route.trim_matches('/').to_string(),
            query: w.query.clone(),
            length: w.length,
            range: w.range.clone().filter(|r| !r.is_empty()),
            body: None,
        }
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.query.get(name).map(String::as_str)
    }

    pub fn require(&self, name: &str) -> Result<&str, ApiError> {
        self.param(name).ok_or_else(|| ApiError::bad_request(format!("missing parameter {name}")).field(name))
    }

    /// The body as JSON, for the small requests.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, ApiError> {
        let length = usize::try_from(self.length).unwrap_or(usize::MAX);
        if length > MAX_JSON {
            return Err(ApiError::new(413, "request body too large"));
        }
        let body = match &self.body {
            Some(b) => b.clone(),
            None => {
                let mut body = vec![0; length];
                self::body().read_exact(&mut body)?;
                body
            }
        };
        serde_json::from_slice(&body).map_err(|e| ApiError::new(400, format!("invalid request: {e}")).code(error_codes::INVALID_JSON))
    }
}

pub fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(high), Some(low)) => {
                    out.push(high << 4 | low);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(digit: u8) -> Option<u8> {
    (digit as char).to_digit(16).map(|d| d as u8)
}

/// RFC 5987 encoding for the filename* parameter of Content-Disposition.
pub fn percent_encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// An error answered as rynas's `ApiError` JSON with an HTTP status.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    /// A constant of [`error_codes`].
    pub code: &'static str,
    /// English, for logs and tooltips; the interface translates `code`.
    pub message: String,
    /// The request parameter the error is about, when known.
    pub field: Option<String>,
}

/// The code of a status when nothing more precise was said.
fn code_for(status: u16) -> &'static str {
    match status {
        507 => error_codes::INSUFFICIENT_STORAGE,
        _ => error_codes::for_status(status),
    }
}

impl ApiError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self { status, code: code_for(status), message: message.into(), field: None }
    }
    /// A more precise code than the status gives.
    pub fn code(mut self, code: &'static str) -> Self {
        self.code = code;
        self
    }
    pub fn field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }
    /// 400 `invalid_value` about the parameter `field`.
    pub fn invalid(field: &str, message: impl Into<String>) -> Self {
        Self::new(400, message).code(error_codes::INVALID_VALUE).field(field)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, message)
    }
    /// 409 `already_exists`: the name is taken.
    pub fn exists(message: impl Into<String>) -> Self {
        Self::new(409, message).code(error_codes::ALREADY_EXISTS)
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(409, message)
    }
    /// 409 `read_only`: inside an archive, a read-only snapshot.
    pub fn read_only(message: impl Into<String>) -> Self {
        Self::new(409, message).code(error_codes::READ_ONLY)
    }
    /// 400 `share_root`: a shared folder itself cannot be renamed, moved or deleted here.
    pub fn share_root(message: impl Into<String>) -> Self {
        Self::new(400, message).code(error_codes::SHARE_ROOT)
    }
}

impl From<io::Error> for ApiError {
    fn from(e: io::Error) -> Self {
        let code = crate::fsops::io_code(&e);
        let status = match code {
            error_codes::NOT_FOUND => 404,
            error_codes::PERMISSION_DENIED => 403,
            error_codes::INVALID_VALUE => 400,
            error_codes::ALREADY_EXISTS | error_codes::READ_ONLY | error_codes::CONFLICT => 409,
            // a full volume or quota: retrying will not help
            error_codes::INSUFFICIENT_STORAGE => 507,
            error_codes::TIMEOUT => 504,
            error_codes::REMOTE_UNREACHABLE => 502,
            _ => 500,
        };
        Self { status, code, message: crate::fsops::describe(&e), field: None }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        206 => "Partial Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        507 => "Insufficient Storage",
        _ => "Internal Server Error",
    }
}

pub fn head(out: &mut impl Write, status: u16, headers: &[(&str, &str)]) -> io::Result<()> {
    write!(out, "Status: {status} {}\r\n", reason(status))?;
    for (name, value) in headers {
        write!(out, "{name}: {value}\r\n")?;
    }
    // API answers are per-user and change constantly
    write!(out, "Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\r\n")
}

pub fn json(out: &mut impl Write, status: u16, value: &impl Serialize) -> io::Result<()> {
    let body = serde_json::to_vec(value).map_err(io::Error::other)?;
    let length = body.len().to_string();
    head(out, status, &[("Content-Type", "application/json"), ("Content-Length", &length)])?;
    out.write_all(&body)
}

pub fn error(out: &mut impl Write, err: &ApiError) -> io::Result<()> {
    let body = rynas_api_types::ApiError { code: err.code.into(), message: err.message.clone(), field: err.field.clone() };
    json(out, err.status, &body)
}

pub fn disposition(kind: &str, filename: &str) -> String {
    // ASCII fallback for old clients, the exact name in filename*
    let fallback: String = filename
        .chars()
        .map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' { c } else { '_' })
        .collect();
    format!("{kind}; filename=\"{fallback}\"; filename*=UTF-8''{}", percent_encode(filename))
}

/// The byte range a `Range: bytes=a-b` header asks of a body of `size`
/// bytes: only single ranges, which is what players and PDF viewers send.
pub fn parse_range(header: &str, size: u64) -> Option<(u64, u64)> {
    let spec = header.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let (start, end) = match (start.trim(), end.trim()) {
        ("", "") => return None,
        ("", suffix) => {
            let n: u64 = suffix.parse().ok()?;
            (size.saturating_sub(n), size.checked_sub(1)?)
        }
        (s, "") => (s.parse().ok()?, size.checked_sub(1)?),
        (s, e) => (s.parse().ok()?, e.parse::<u64>().ok()?.min(size.checked_sub(1)?)),
    };
    (start <= end && end < size).then_some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_queries() {
        let q = parse_query("path=%2FDocumenti%2Fa+b%2F%C3%A8&bad=%zz&x");
        assert_eq!(q["path"], "/Documenti/a b/è");
        assert_eq!(q["bad"], "%zz");
        assert_eq!(q["x"], "");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%4"), "%4");
    }

    #[test]
    fn io_errors_have_codes() {
        let e = |n| ApiError::from(io::Error::from_raw_os_error(n));
        assert_eq!((e(libc::ENOSPC).status, e(libc::ENOSPC).code), (507, error_codes::INSUFFICIENT_STORAGE));
        assert_eq!(e(libc::EDQUOT).status, 507);
        assert_eq!((e(libc::EIO).status, e(libc::EIO).code), (500, error_codes::INTERNAL));
        assert_eq!((e(libc::ENOENT).status, e(libc::ENOENT).code), (404, error_codes::NOT_FOUND));
        assert_eq!((e(libc::EACCES).status, e(libc::EACCES).code), (403, error_codes::PERMISSION_DENIED));
        assert_eq!((e(libc::EEXIST).status, e(libc::EEXIST).code), (409, error_codes::ALREADY_EXISTS));
        assert_eq!(e(libc::EROFS).code, error_codes::READ_ONLY);
    }

    #[test]
    fn errors_are_api_errors_with_the_real_status() {
        let mut out = Vec::new();
        error(&mut out, &ApiError::new(507, "full")).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("Status: 507 "), "{text}");
        let body = text.split("\r\n\r\n").nth(1).unwrap();
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["code"], error_codes::INSUFFICIENT_STORAGE);
        assert_eq!(v["message"], "full");
        assert!(v.get("field").is_none());
        let mut out = Vec::new();
        error(&mut out, &ApiError::invalid("name", "bad")).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("\"field\":\"name\""));
    }

    #[test]
    fn encodes_filenames() {
        assert_eq!(percent_encode("café è.txt"), "caf%C3%A9%20%C3%A8.txt");
    }

    #[test]
    fn parses_ranges() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=900-", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=0-5000", 1000), Some((0, 999)));
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=0-1,5-6", 1000), None);
        assert_eq!(parse_range("bytes=0-", 0), None);
    }
}
