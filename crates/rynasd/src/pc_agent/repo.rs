//! The restic REST protocol (v1 and v2 listings) for the agents, with per-PC rules
//! (`docs/phase1/plan.md` "API v1: PC backup", agent table).
//!
//! - Who: a device token reaches only its own group's repository and may write; a restore
//!   session only reads, only its devices' groups.
//! - Append-only: `config` and `keys` are never written by a PC, no file is ever overwritten
//!   (a name is the SHA-256 of its content, checked on upload: the same name twice is the same
//!   file), nothing is deleted except lock files.
//! - The group's quota is counted before an upload starts (Content-Length) and while it runs.
//! - While the NAS maintains the repository every write answers 423 (rustic ignores locks).
//! - On disk the restic local layout (`data/<2 hex>/<name>`), so the worker and stock restic
//!   open the folder directly. Every file is written to a temporary name in its folder,
//!   synced, then linked into place (`link` fails when the name exists: no race can replace a
//!   file) and the folder synced.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use rynas_api_types::error_codes;
use rynas_api_types::pcagent::{REST_V2, RestEntry};
use sha2::{Digest, Sha256};

use super::{PaState, PcAgent, bearer, pa};
use crate::ops::{Error, Res};

/// Largest single upload (the PC profile caps packs at 512 MiB).
const MAX_UPLOAD: u64 = 1 << 30;
const TYPES: &[&str] = &["data", "keys", "locks", "snapshots", "index"];

/// What a request may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    /// A paired PC: read and append.
    Write { device: String },
    /// A restore session: read only.
    Read,
}

/// The parsed path below `/repo/<group>/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Config,
    List(&'static str),
    Object(&'static str, String),
}

pub fn parse(rest: &str) -> Option<Target> {
    if rest == "config" {
        return Some(Target::Config);
    }
    let (t, name) = match rest.split_once('/') {
        Some((t, n)) => (t, n),
        None => (rest, ""),
    };
    let t = *TYPES.iter().find(|x| **x == t)?;
    if name.is_empty() {
        return Some(Target::List(t));
    }
    if name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
        return Some(Target::Object(t, name.to_string()));
    }
    None
}

/// The file of an object inside the repository folder.
pub fn object_path(repo: &Path, t: &str, name: &str) -> PathBuf {
    if t == "data" {
        repo.join("data").join(&name[..2]).join(name)
    } else {
        repo.join(t).join(name)
    }
}

fn refuse(status: StatusCode, msg: &str) -> Error {
    let code = match status {
        StatusCode::UNAUTHORIZED => error_codes::UNAUTHENTICATED,
        StatusCode::FORBIDDEN => error_codes::FORBIDDEN,
        StatusCode::NOT_FOUND => error_codes::NOT_FOUND,
        StatusCode::PAYLOAD_TOO_LARGE => error_codes::PAYLOAD_TOO_LARGE,
        StatusCode::LOCKED => error_codes::BUSY,
        StatusCode::RANGE_NOT_SATISFIABLE => error_codes::INVALID_VALUE,
        _ => error_codes::BAD_REQUEST,
    };
    Error::new(status, code, msg.to_string())
}

fn access(svc: &PcAgent, h: &HeaderMap, group: &str) -> Res<Access> {
    let Some(t) = bearer(h) else {
        return Err(refuse(StatusCode::UNAUTHORIZED, "no token"));
    };
    if let Some((device, g)) = svc.device_of_token(&t) {
        if g == group {
            return Ok(Access::Write { device });
        }
        return Err(refuse(StatusCode::FORBIDDEN, "not this PC's group"));
    }
    if svc.session_groups(&t).is_some_and(|g| g.iter().any(|x| x == group)) {
        return Ok(Access::Read);
    }
    Err(refuse(StatusCode::UNAUTHORIZED, "unknown token"))
}

pub async fn create_refused() -> Response {
    refuse(StatusCode::FORBIDDEN, "the NAS creates the repositories").into_response()
}

pub async fn handle(
    State(st): State<PaState>,
    UrlPath((group, rest)): UrlPath<(String, String)>,
    method: Method,
    h: HeaderMap,
    body: Body,
) -> Response {
    match serve(st, group, rest, method, h, body).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn serve(st: PaState, group: String, rest: String, method: Method, h: HeaderMap, body: Body) -> Res<Response> {
    let svc = pa(&st)?;
    let acc = access(&svc, &h, &group)?;
    let target = parse(&rest).ok_or_else(|| refuse(StatusCode::NOT_FOUND, "no such repository file"))?;
    let repo = PathBuf::from(svc.group(&group)?.repo);
    match (method.clone(), &target) {
        (Method::GET | Method::HEAD, Target::Config) => {
            serve_file(&repo.join("config"), &h, method == Method::HEAD).await
        }
        (Method::GET | Method::HEAD, Target::Object(t, n)) => {
            serve_file(&object_path(&repo, t, n), &h, method == Method::HEAD).await
        }
        (Method::GET, Target::List(t)) => {
            let v2 = h
                .get(header::ACCEPT)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.contains(REST_V2));
            let t = *t;
            let list = tokio::task::spawn_blocking(move || list(&repo, t))
                .await
                .map_err(Error::internal)?
                .map_err(Error::internal)?;
            let (body, ct) = if v2 {
                (serde_json::to_vec(&list).map_err(Error::internal)?, REST_V2)
            } else {
                let names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
                (serde_json::to_vec(&names).map_err(Error::internal)?, "application/vnd.x.restic.rest.v1")
            };
            Ok(([(header::CONTENT_TYPE, ct)], body).into_response())
        }
        (Method::POST, Target::Object(t, n)) => {
            let Access::Write { device } = acc else {
                return Err(refuse(StatusCode::FORBIDDEN, "a restore session only reads"));
            };
            if *t == "keys" {
                return Err(refuse(StatusCode::FORBIDDEN, "the keys are the NAS' to change"));
            }
            upload(&svc, &group, &device, &repo, t, n, &h, body).await
        }
        (Method::POST, Target::Config) => Err(refuse(StatusCode::FORBIDDEN, "the config is the NAS' to change")),
        (Method::DELETE, Target::Object("locks", n)) => {
            if acc == Access::Read {
                return Err(refuse(StatusCode::FORBIDDEN, "a restore session only reads"));
            }
            let p = object_path(&repo, "locks", n);
            match tokio::fs::remove_file(&p).await {
                Ok(()) => Ok(StatusCode::OK.into_response()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(refuse(StatusCode::NOT_FOUND, "no such lock")),
                Err(e) => Err(Error::internal(e)),
            }
        }
        (Method::DELETE, _) => Err(refuse(StatusCode::FORBIDDEN, "append-only: only lock files can be deleted")),
        _ => Err(Error::new(
            StatusCode::METHOD_NOT_ALLOWED,
            error_codes::METHOD_NOT_ALLOWED,
            "method not allowed".into(),
        )),
    }
}

/// `bytes=a-b`, `bytes=a-`, `bytes=-n` → (start, length) inside `size`.
pub fn parse_range(v: &str, size: u64) -> Option<(u64, u64)> {
    let r = v.trim().strip_prefix("bytes=")?;
    if r.contains(',') {
        return None;
    }
    let (a, b) = r.split_once('-')?;
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        if n == 0 {
            return None;
        }
        (size.saturating_sub(n), size.checked_sub(1)?)
    } else {
        let s: u64 = a.parse().ok()?;
        let e = if b.is_empty() { size.checked_sub(1)? } else { b.parse::<u64>().ok()?.min(size.checked_sub(1)?) };
        (s, e)
    };
    if start > end || start >= size {
        return None;
    }
    Some((start, end - start + 1))
}

async fn serve_file(p: &Path, h: &HeaderMap, head: bool) -> Res<Response> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(p);
    let f = match f {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(refuse(StatusCode::NOT_FOUND, "no such file")),
        Err(e) => return Err(Error::internal(e)),
    };
    let size = f.metadata().map_err(Error::internal)?.len();
    let range = h.get(header::RANGE).and_then(|v| v.to_str().ok());
    let (status, start, len) = match range {
        Some(r) => match parse_range(r, size) {
            Some((s, l)) => (StatusCode::PARTIAL_CONTENT, s, l),
            None => return Err(refuse(StatusCode::RANGE_NOT_SATISFIABLE, "bad range")),
        },
        None => (StatusCode::OK, 0, size),
    };
    let mut resp = if head {
        Response::new(Body::empty())
    } else {
        let mut f = tokio::fs::File::from_std(f);
        if start > 0 {
            f.seek(std::io::SeekFrom::Start(start)).await.map_err(Error::internal)?;
        }
        let stream = tokio_util::io::ReaderStream::with_capacity(f.take(len), 256 << 10);
        Response::new(Body::from_stream(stream))
    };
    *resp.status_mut() = status;
    let hd = resp.headers_mut();
    hd.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    hd.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    // also keeps the compression layers away from repository files
    hd.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if status == StatusCode::PARTIAL_CONTENT
        && let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{}/{size}", start + len - 1))
    {
        hd.insert(header::CONTENT_RANGE, v);
    }
    Ok(resp)
}

fn list(repo: &Path, t: &str) -> std::io::Result<Vec<RestEntry>> {
    let mut out = Vec::new();
    let dir = repo.join(t);
    let mut push_dir = |d: &Path| -> std::io::Result<()> {
        let rd = match std::fs::read_dir(d) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        for e in rd {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            if name.len() != 64 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            let m = e.metadata()?;
            if m.is_file() {
                out.push(RestEntry { name, size: m.len() });
            }
        }
        Ok(())
    };
    if t == "data" {
        for i in 0..=255u8 {
            push_dir(&dir.join(format!("{i:02x}")))?;
        }
    } else {
        push_dir(&dir)?;
    }
    Ok(out)
}

/// Frees the write slot of an upload, however it ends.
struct Writing {
    svc: Arc<PcAgent>,
    group: String,
}

impl Drop for Writing {
    fn drop(&mut self) {
        let mut live = self.svc.live.lock().unwrap_or_else(|p| p.into_inner());
        let l = live.entry(self.group.clone()).or_default();
        l.writes = l.writes.saturating_sub(1);
        l.last_write = Some(Instant::now());
    }
}

#[allow(clippy::too_many_arguments)]
async fn upload(
    svc: &Arc<PcAgent>,
    group: &str,
    device: &str,
    repo: &Path,
    t: &str,
    name: &str,
    h: &HeaderMap,
    body: Body,
) -> Res<Response> {
    let declared = h
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > MAX_UPLOAD) {
        return Err(refuse(StatusCode::PAYLOAD_TOO_LARGE, "file over 1 GiB"));
    }
    let quota = u64::from(svc.group(group)?.settings.quota_gib) << 30;
    let used = super::maint::used_bytes(svc, group, repo);
    // the slot and the gate under one lock: a maintenance cannot start between them
    let _slot = {
        let mut live = svc.live.lock().unwrap_or_else(|p| p.into_inner());
        let l = live.entry(group.to_string()).or_default();
        if l.maintaining {
            return Err(refuse(StatusCode::LOCKED, "the NAS is maintaining this repository: try again later"));
        }
        if quota > 0 && used + declared.unwrap_or(0) > quota {
            return Err(refuse(StatusCode::PAYLOAD_TOO_LARGE, "the group's quota is used up"));
        }
        l.writes += 1;
        Writing {
            svc: svc.clone(),
            group: group.to_string(),
        }
    };
    let final_path = object_path(repo, t, name);
    let dir = final_path.parent().map(Path::to_path_buf).ok_or_else(|| Error::internal("no folder"))?;
    let limit = if quota > 0 { (quota.saturating_sub(used)).min(MAX_UPLOAD) } else { MAX_UPLOAD };
    let (tx, mut rx) = tokio::sync::mpsc::channel::<axum::body::Bytes>(16);
    let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), &super::new_token()?[..16]));
    let (tmp2, dir2) = (tmp.clone(), dir.clone());
    // the writer: a blocking thread that hashes and writes as the chunks arrive
    let writer = tokio::task::spawn_blocking(move || -> std::io::Result<(String, u64)> {
        std::fs::DirBuilder::new().recursive(true).create(&dir2)?;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(0o600)
            .open(&tmp2)?;
        let mut sha = Sha256::new();
        let mut n = 0u64;
        while let Some(chunk) = rx.blocking_recv() {
            sha.update(&chunk);
            f.write_all(&chunk)?;
            n += chunk.len() as u64;
        }
        f.sync_all()?;
        let d = sha.finalize();
        Ok((d.iter().map(|b| format!("{b:02x}")).collect(), n))
    });
    let mut stream = body.into_data_stream();
    let mut got = 0u64;
    let mut fail: Option<Error> = None;
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(c) => {
                got += c.len() as u64;
                if got > limit {
                    fail = Some(refuse(StatusCode::PAYLOAD_TOO_LARGE, "over the quota or the size limit"));
                    break;
                }
                if tx.send(c).await.is_err() {
                    break;
                }
            }
            Err(e) => {
                fail = Some(refuse(StatusCode::BAD_REQUEST, &format!("upload cut short: {e}")));
                break;
            }
        }
    }
    drop(tx);
    let written = writer.await.map_err(Error::internal)?;
    let cleanup = |p: &Path| {
        let _ = std::fs::remove_file(p);
    };
    if let Some(e) = fail {
        cleanup(&tmp);
        return Err(e);
    }
    let (sha, size) = match written {
        Ok(x) => x,
        Err(e) => {
            cleanup(&tmp);
            return Err(Error::internal(format!("writing {}: {e}", final_path.display())));
        }
    };
    if declared.is_some_and(|d| d != size) {
        cleanup(&tmp);
        return Err(refuse(StatusCode::BAD_REQUEST, "the body is not as long as Content-Length"));
    }
    if sha != name {
        cleanup(&tmp);
        return Err(refuse(StatusCode::BAD_REQUEST, "the name is not the SHA-256 of the content"));
    }
    let (tmp3, fin) = (tmp.clone(), final_path.clone());
    let placed = tokio::task::spawn_blocking(move || -> std::io::Result<bool> {
        let r = std::fs::hard_link(&tmp3, &fin);
        let _ = std::fs::remove_file(&tmp3);
        match r {
            Ok(()) => {
                let _ = std::fs::set_permissions(&fin, std::fs::Permissions::from_mode(0o600));
                if let Some(d) = fin.parent() {
                    std::fs::File::open(d)?.sync_all()?;
                }
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // the same name is the same content (checked above): already there
                let m = std::fs::symlink_metadata(&fin)?;
                if m.is_file() && m.len() == size {
                    Ok(false)
                } else {
                    Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "a different file has this name"))
                }
            }
            Err(e) => Err(e),
        }
    })
    .await
    .map_err(Error::internal)?;
    match placed {
        Ok(true) => {
            super::maint::add_used(svc, group, device, size);
            Ok(StatusCode::OK.into_response())
        }
        Ok(false) => Ok(StatusCode::OK.into_response()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(refuse(StatusCode::FORBIDDEN, "append-only: the file exists")),
        Err(e) => Err(Error::internal(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        let n = "a".repeat(64);
        assert_eq!(parse("config"), Some(Target::Config));
        assert_eq!(parse("data/"), Some(Target::List("data")));
        assert_eq!(parse("locks"), Some(Target::List("locks")));
        assert_eq!(parse(&format!("data/{n}")), Some(Target::Object("data", n.clone())));
        assert_eq!(parse(&format!("data/{}", "A".repeat(64))), None);
        assert_eq!(parse("data/../config"), None);
        assert_eq!(parse("other/"), None);
        assert_eq!(
            object_path(Path::new("/r"), "data", &n),
            PathBuf::from(format!("/r/data/aa/{n}"))
        );
        assert_eq!(object_path(Path::new("/r"), "keys", &n), PathBuf::from(format!("/r/keys/{n}")));
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-9", 100), Some((0, 10)));
        assert_eq!(parse_range("bytes=90-", 100), Some((90, 10)));
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 10)));
        assert_eq!(parse_range("bytes=95-200", 100), Some((95, 5)));
        assert_eq!(parse_range("bytes=100-", 100), None);
        assert_eq!(parse_range("bytes=5-1", 100), None);
        assert_eq!(parse_range("bytes=0-1,3-4", 100), None);
    }
}
