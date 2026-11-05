//! HTTP pieces shared by `rynas-web` and `rynasd`, so both answer a bad request the same way.
//!
//! - [`json_rejection`], [`from_slice`], [`from_value`]: a JSON body that does not
//!   deserialize becomes a stable [`error_codes`] code, the request field it is about
//!   ([`ApiError::field`], spelled as in the body: `smb.veto_files`, `members[2]`) and a
//!   short English message. serde's own text ("invalid type: string \"x\", expected u32 at
//!   line 1 column 9") never reaches the client (guidelines A10, A13); it is logged at debug.
//! - [`json_errors`]: the middleware that turns every other non-JSON error answer (axum's
//!   query string, path, 405, 413 rejections) into an [`ApiError`] with a generic message.
//!
//! Codes: `invalid_json` (not JSON at all), `missing_field`, `unknown_field`, `invalid_type`
//! (a string where a number goes), `invalid_value` (a value outside the accepted set or
//! range), `unsupported_media_type`, `payload_too_large`, `bad_request` (query string, path).

use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use rynas_api_types::{ApiError, error_codes};
use serde::de::DeserializeOwned;

/// A rejected request: the status and the body to answer with.
pub type Rejected = (StatusCode, ApiError);

type PathError = serde_path_to_error::Error<serde_json::Error>;

fn rejected(status: StatusCode, code: &str, message: String, field: Option<String>) -> Rejected {
    (
        status,
        ApiError {
            code: code.into(),
            message,
            field,
        },
    )
}

/// serde_json's message without its " at line L column C" suffix.
fn without_location(m: &str) -> &str {
    match m.rfind(" at line ") {
        Some(i) if m[i..].contains(" column ") => &m[..i],
        _ => m,
    }
}

/// The name between the first pair of backticks after `prefix`, as in serde's
/// "missing field \`name\`".
fn backticked<'a>(m: &'a str, prefix: &str) -> Option<&'a str> {
    m.strip_prefix(prefix)?.strip_prefix('`')?.split('`').next()
}

/// `path` (serde_path_to_error's, `.` for the root) as a field name; `None` at the root.
fn field_of(path: &str) -> Option<String> {
    match path {
        "" | "." => None,
        p => Some(p.to_string()),
    }
}

/// `name` inside the object at `path`.
fn join(path: &str, name: &str) -> String {
    match path {
        "" | "." => name.to_string(),
        p if p == name || p.ends_with(&format!(".{name}")) => p.to_string(),
        p => format!("{p}.{name}"),
    }
}

/// A body that is JSON but not of the expected shape, from the path and serde's message.
fn data_error(path: &str, message: &str) -> Rejected {
    let m = without_location(message);
    let bad = StatusCode::BAD_REQUEST;
    let about = |f: &Option<String>| f.clone().unwrap_or_else(|| "the body".into());
    if let Some(name) = backticked(m, "missing field ") {
        let f = join(path, name);
        return rejected(
            bad,
            error_codes::MISSING_FIELD,
            format!("{f} is required"),
            Some(f),
        );
    }
    if let Some(name) = backticked(m, "unknown field ") {
        let f = join(path, name);
        return rejected(
            bad,
            error_codes::UNKNOWN_FIELD,
            format!("{f} is not a known field"),
            Some(f),
        );
    }
    let field = field_of(path);
    if m.starts_with("invalid type: ") {
        let msg = format!("{} has the wrong type", about(&field));
        return rejected(bad, error_codes::INVALID_TYPE, msg, field);
    }
    if let Some(name) = backticked(m, "duplicate field ") {
        let f = join(path, name);
        return rejected(
            bad,
            error_codes::INVALID_VALUE,
            format!("{f} is given twice"),
            Some(f),
        );
    }
    // unknown variant, invalid value or length, a custom check of a type
    let msg = format!("{} has an invalid value", about(&field));
    rejected(bad, error_codes::INVALID_VALUE, msg, field)
}

fn invalid_json() -> Rejected {
    rejected(
        StatusCode::BAD_REQUEST,
        error_codes::INVALID_JSON,
        "the body is not valid JSON".into(),
        None,
    )
}

/// A deserialization error with its path.
pub fn path_error(e: &PathError) -> Rejected {
    tracing::debug!("rejected JSON body: {e}");
    use serde_json::error::Category;
    let path = e.path().to_string();
    match e.inner().classify() {
        Category::Data => data_error(&path, &e.inner().to_string()),
        // serde_json reports a value of the wrong kind for an enum (`"level": 3`) as a syntax
        // error ("expected value") at that field: name the field rather than "not JSON"
        Category::Syntax if field_of(&path).is_some() => {
            let f = field_of(&path);
            let msg = format!(
                "{} has an invalid value",
                f.as_deref().unwrap_or("the body")
            );
            rejected(StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE, msg, f)
        }
        Category::Syntax | Category::Eof | Category::Io => invalid_json(),
    }
}

/// A JSON body rejected by axum's `Json` extractor.
pub fn json_rejection(e: JsonRejection) -> Rejected {
    match e {
        JsonRejection::JsonDataError(d) => {
            // JsonDataError → axum_core::Error → the serde_path_to_error error
            let mut src = std::error::Error::source(&d);
            while let Some(s) = src {
                if let Some(p) = s.downcast_ref::<PathError>() {
                    return path_error(p);
                }
                src = s.source();
            }
            tracing::debug!("rejected JSON body: {}", d.body_text());
            rejected(
                StatusCode::BAD_REQUEST,
                error_codes::INVALID_VALUE,
                "the body has an invalid value".into(),
                None,
            )
        }
        JsonRejection::JsonSyntaxError(s) => {
            let mut src = std::error::Error::source(&s);
            while let Some(e) = src {
                if let Some(p) = e.downcast_ref::<PathError>() {
                    return path_error(p);
                }
                src = e.source();
            }
            tracing::debug!("rejected JSON body: {}", s.body_text());
            invalid_json()
        }
        JsonRejection::MissingJsonContentType(_) => rejected(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            error_codes::UNSUPPORTED_MEDIA_TYPE,
            "expected Content-Type: application/json".into(),
            None,
        ),
        e if e.status() == StatusCode::PAYLOAD_TOO_LARGE => rejected(
            StatusCode::PAYLOAD_TOO_LARGE,
            error_codes::PAYLOAD_TOO_LARGE,
            "the request body is too large".into(),
            None,
        ),
        e => {
            tracing::debug!("rejected body: {}", e.body_text());
            rejected(
                StatusCode::BAD_REQUEST,
                error_codes::BAD_REQUEST,
                "could not read the request body".into(),
                None,
            )
        }
    }
}

/// `T` from a JSON body (for handlers that read the bytes themselves, e.g. an optional body).
pub fn from_slice<T: DeserializeOwned>(body: &[u8]) -> Result<T, Rejected> {
    let mut de = serde_json::Deserializer::from_slice(body);
    let v = serde_path_to_error::deserialize(&mut de).map_err(|e| path_error(&e))?;
    de.end().map_err(|_| invalid_json())?;
    Ok(v)
}

/// `T` from a JSON value already parsed (a body read as `serde_json::Value` first).
pub fn from_value<T: DeserializeOwned>(v: serde_json::Value) -> Result<T, Rejected> {
    serde_path_to_error::deserialize(v).map_err(|e| path_error(&e))
}

/// Prefix of axum's query string rejection text.
const QUERY_PREFIX: &str = "Failed to deserialize query string: ";

/// Code, field and message of a framework rejection given as plain text (axum's default
/// bodies), without passing that text on.
fn plain_rejection(status: StatusCode, text: &str) -> ApiError {
    if !text.is_empty() {
        tracing::debug!("rejected request ({status}): {text}");
    }
    let code = error_codes::for_status(status.as_u16());
    if status == StatusCode::BAD_REQUEST
        && let Some(rest) = text.strip_prefix(QUERY_PREFIX)
    {
        // "limit: invalid digit found in string"; no field when the error is about the whole
        let field = rest
            .split_once(": ")
            .map(|(f, _)| f)
            .filter(|f| {
                !f.is_empty()
                    && f.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
            })
            .map(str::to_string);
        let message = match &field {
            Some(f) => format!("invalid query parameter {f}"),
            None => "invalid query string".into(),
        };
        return ApiError {
            code: code.into(),
            message,
            field,
        };
    }
    let message = match status {
        StatusCode::BAD_REQUEST => "malformed request",
        StatusCode::NOT_FOUND => "not found",
        StatusCode::METHOD_NOT_ALLOWED => "method not allowed",
        StatusCode::PAYLOAD_TOO_LARGE => "the request body is too large",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported content type",
        s => s.canonical_reason().unwrap_or("error"),
    };
    ApiError::new(code, message)
}

/// Largest error body read back to wrap it (framework rejections are one line).
const MAX_ERROR_BODY: usize = 4 << 10;

fn is_json(r: &Response) -> bool {
    r.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"))
}

/// Middleware: any error answer that is not JSON yet becomes an [`ApiError`] whose code
/// follows the status ([`error_codes::for_status`]) with a generic message (the original
/// text goes to the debug log). Other headers (`Allow` on 405, `Retry-After`) are kept.
pub async fn json_errors(req: Request, next: Next) -> Response {
    let r = next.run(req).await;
    let status = r.status();
    if !(status.is_client_error() || status.is_server_error()) || is_json(&r) {
        return r;
    }
    let (mut parts, body) = r.into_parts();
    let text = to_bytes(body, MAX_ERROR_BODY)
        .await
        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
        .unwrap_or_default();
    let body = serde_json::to_vec(&plain_rejection(status, &text)).unwrap_or_default();
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.remove(header::CONTENT_ENCODING);
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Response::from_parts(parts, Body::from(body))
}

#[cfg(test)]
mod tests;
