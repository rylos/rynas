//! The routes a worker answers, already running as the user (`rynas-files-worker serve`).
//!
//! rynasd handles itself what belongs to the user's stores and to the transfer jobs
//! (`whoami`, `prefs`, `conns`, `conns/save|delete|order|key`, `jobs`, `jobs/new`,
//! `jobs/control`); everything that touches files comes here, so the kernel decides what
//! the user may read and change. Route names and JSON shapes are SynoFile's (see
//! `docs/plugins/files.md`).

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::Context;
use crate::conns::Conn;
use crate::archive;
use crate::jobs::protocol::{Op, Policy, RecycleRoot, Spec};
use crate::shares::{self, Share};
use crate::wire::{self as cgi, ApiError, Request, WorkerRequest};
use crate::{fsops, space, transfer};

/// The routes about remote connections, which live in the worker (it has the SMB and SFTP
/// clients; rynasd does not).
pub trait Remote {
    /// A route whose path is on a server (`/:<id>/…`), or `None` for the shared folders.
    fn serve(&self, req: &Request, out: &mut dyn Write, conns: &[Conn]) -> Option<Result<(), ApiError>>;
    /// `conns/test`: logs in to `conns[0]`.
    fn test(&self, req: &Request, out: &mut dyn Write, conns: &[Conn]) -> Result<(), ApiError>;
    /// `usage` of paths on a server.
    fn usage(&self, out: &mut dyn Write, conns: &[Conn], paths: &[String]) -> Result<(), ApiError>;
}

/// Answers one request: the [`WorkerRequest`] line and the body on stdin, the answer on
/// stdout.
pub fn serve(remote: &dyn Remote) -> io::Result<()> {
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    let w: WorkerRequest = match serde_json::from_str(&line) {
        Ok(w) => w,
        Err(e) => {
            let mut out = io::stdout().lock();
            cgi::error(&mut out, &ApiError::new(500, format!("bad worker request: {e}")))?;
            return out.flush();
        }
    };
    let req = Request::new(&w);
    let mut out = io::BufWriter::with_capacity(256 * 1024, io::stdout().lock());
    let result = route(&req, &mut out, &w.ctx, remote);
    cgi::discard_unread(req.length);
    if let Err(err) = result {
        cgi::error(&mut out, &err)?;
    }
    out.flush()
}

fn method(req: &Request, want: &str) -> Result<(), ApiError> {
    if req.method == want { Ok(()) } else { Err(ApiError::new(405, format!("use {want}"))) }
}

fn route(req: &Request, out: &mut impl Write, ctx: &Context, remote: &dyn Remote) -> Result<(), ApiError> {
    match req.route.as_str() {
        "init" => {
            // what the interface needs to start, in one request: rynasd's part (whoami,
            // prefs, connections) and the shares as the user sees them
            let extra = &ctx.extra;
            reply(out, &json!({ "whoami": extra["whoami"], "prefs": extra["prefs"], "shares": shares_info(ctx), "conns": extra["conns"] }))
        }
        "conns/test" => remote.test(req, out, &ctx.conns),
        _ => {
            if let Some(result) = remote.serve(req, out, &ctx.conns) {
                return result;
            }
            as_user(req, out, ctx, remote)
        }
    }
}

/// The shared folders with their space and what the user may do there: run as the user.
pub fn shares_info(ctx: &Context) -> Value {
    let list: Vec<Value> = ctx
        .shares
        .iter()
        .map(|s| {
            let (free, total) = fsops::space(&s.path).unwrap_or((0, 0));
            let mut v = s.public();
            v["free"] = json!(free);
            v["total"] = json!(total);
            v["readable"] = json!(fsops::access(&s.path, libc::R_OK | libc::X_OK));
            v["writable"] = json!(fsops::access(&s.path, libc::W_OK | libc::X_OK));
            v
        })
        .collect();
    json!({ "shares": list, "user": ctx.user.name, "volumes": ctx.volumes, "hostname": ctx.hostname })
}

/// `rel` ("Photos/2024/Sea") below `dir`, each part a valid name.
fn nested(dir: &Path, rel: &str) -> Result<PathBuf, ApiError> {
    let mut path = dir.to_path_buf();
    for part in rel.split('/') {
        shares::valid_name(part)?;
        path.push(part);
    }
    Ok(path)
}

fn reply(out: &mut impl Write, value: &impl serde::Serialize) -> Result<(), ApiError> {
    cgi::json(out, 200, value)?;
    Ok(())
}

/// The body of `jobs/new`.
#[derive(Debug, Clone, Deserialize)]
pub struct NewJob {
    pub op: Op,
    pub sources: Vec<String>,
    #[serde(default)]
    pub dest: Option<String>,
    #[serde(default)]
    pub conflict: Policy,
    #[serde(default)]
    pub verify: bool,
    #[serde(default)]
    pub limit: u64,
    #[serde(default)]
    pub permanent: bool,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub level: Option<String>,
    #[serde(default)]
    pub into_folder: bool,
    #[serde(default)]
    pub password: Option<String>,
    /// extract: only these items (paths inside the archive), each into dest
    #[serde(default)]
    pub members: Vec<String>,
}

/// The worker's job from the interface's request: virtual paths resolved against the
/// user's shares, remote ones kept as they are (the worker connects), the recycle bins of
/// the shares a deletion touches. Run by rynasd before it starts the job.
pub fn job_spec(shares: &[Share], conns: &[crate::conns::Conn], body: &NewJob) -> Result<Spec, ApiError> {
    if body.sources.is_empty() {
        return Err(ApiError::invalid("sources", t!("nothing selected")));
    }
    if body.op == Op::Cleanup {
        return Err(ApiError::invalid("op", "invalid operation"));
    }
    let mut sources = Vec::new();
    let mut recycle = Vec::new();
    let mut remotes: Vec<crate::conns::Conn> = Vec::new();
    // a remote path stays as it is, the worker connects: (path, whether it
    // is the server or one of its shares)
    let mut remote_path = |vpath: &str| -> Option<Result<(PathBuf, bool), ApiError>> {
        let split = crate::conns::split(vpath)?;
        Some(split.and_then(|(id, parts)| {
            let conn = conns.iter().find(|c| c.id == id).ok_or_else(|| ApiError::not_found(t!("no such connection: was it deleted?")))?;
            let top = parts.is_empty() || (conn.kind == crate::conns::Kind::Smb && parts.len() == 1);
            if !remotes.iter().any(|c| c.id == id) {
                remotes.push(conn.clone());
            }
            Ok((crate::conns::join(&id, &parts), top))
        }))
    };
    if matches!(body.op, Op::Compress | Op::Extract) && body.sources.iter().chain(&body.dest).any(|p| crate::conns::split(p).is_some()) {
        return Err(ApiError::invalid("sources", t!("archives: copy the files to the NAS first")));
    }
    for vpath in &body.sources {
        if let Some(found) = remote_path(vpath) {
            let (path, top) = found?;
            if top {
                return Err(ApiError::share_root(t!("a shared folder cannot be copied, moved or deleted from here")));
            }
            sources.push(path);
            continue;
        }
        let r = shares::resolve(shares, vpath)?;
        if r.is_root() {
            return Err(ApiError::share_root(t!("a shared folder cannot be copied, moved or deleted from here")));
        }
        if body.op == Op::Delete && !body.permanent && r.share.recycle {
            let root = RecycleRoot { name: r.share.name.clone(), path: r.share.path.clone() };
            if !recycle.iter().any(|x: &RecycleRoot| x.name == root.name) {
                recycle.push(root);
            }
        }
        sources.push(r.real);
    }
    let dest = match (&body.dest, body.op) {
        (_, Op::Delete) => None,
        (Some(d), _) if crate::conns::split(d).is_some() => {
            let (path, _) = remote_path(d).expect("a remote path")?;
            if crate::conns::split(d).is_some_and(|r| r.is_ok_and(|(_, parts)| parts.is_empty())) {
                return Err(ApiError::invalid("dest", t!("choose a shared folder of the server")));
            }
            Some(path)
        }
        (Some(d), _) => Some(shares::resolve(shares, d)?.real),
        (None, _) => return Err(ApiError::invalid("dest", t!("no destination"))),
    };
    if let Some(name) = &body.name {
        shares::valid_name(name)?;
    }
    if body.op == Op::Extract && sources.len() != 1 {
        return Err(ApiError::invalid("sources", t!("one archive at a time")));
    }
    Ok(Spec {
        op: body.op,
        sources,
        dest,
        conflict: body.conflict,
        verify: body.verify,
        limit: body.limit,
        recycle,
        name: body.name.clone(),
        level: body.level.clone(),
        into_folder: body.into_folder,
        password: body.password.clone(),
        members: body.members.iter().map(|m| valid_member(m)).collect::<Result<_, _>>()?,
        remotes,
    })
}

/// A path inside an archive, as the client sends it: plain components only.
fn valid_member(m: &str) -> Result<String, ApiError> {
    let parts: Vec<&str> = m.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() || parts.iter().any(|p| *p == "." || *p == ".." || p.contains('\0')) {
        return Err(ApiError::invalid("members", "invalid path"));
    }
    Ok(parts.join("/"))
}

/// A path that goes inside an archive file: the archive, the path inside
/// it, and what the archive holds.
fn in_archive(real: &std::path::Path) -> Result<Option<(PathBuf, String, Vec<archive::Member>)>, ApiError> {
    match archive::locate(real) {
        Some((file, inner)) => {
            let members = archive::members(&file)?;
            Ok(Some((file, inner, members)))
        }
        None => Ok(None),
    }
}

/// One file out of an archive, to show or to download.
fn send_member(out: &mut impl Write, real: &std::path::Path, inline: bool) -> Result<bool, ApiError> {
    let Some((file, inner, members)) = in_archive(real)? else { return Ok(false) };
    if inner.is_empty() {
        return Ok(false);
    }
    let m = archive::find(&members, &inner).ok_or_else(|| ApiError::not_found(t!("item not found in the archive")))?;
    transfer::send_command(out, archive::cat(&file, m), &inner, m.size, m.mtime, inline)?;
    Ok(true)
}

#[derive(Deserialize)]
struct Paths {
    paths: Vec<String>,
}

fn as_user(req: &Request, out: &mut impl Write, ctx: &Context, remote: &dyn Remote) -> Result<(), ApiError> {
    let (shares, conns) = (&ctx.shares[..], &ctx.conns[..]);
    let path_param = |name: &str| -> Result<PathBuf, ApiError> { Ok(shares::resolve(shares, req.require(name)?)?.real) };
    match req.route.as_str() {
        "shares" => reply(out, &shares_info(ctx)),
        "list" => {
            let r = shares::resolve(shares, req.require("path")?)?;
            if let Some((_, inner, members)) = in_archive(&r.real)? {
                if !archive::is_dir(&members, &inner) {
                    return Err(ApiError::invalid("path", t!("not a folder")));
                }
                let (files, dirs, bytes) = archive::usage(&members, &inner);
                let total = json!({ "files": files, "dirs": dirs, "bytes": bytes });
                return reply(out, &json!({ "entries": archive::children(&members, &inner), "writable": false, "space": null, "recycle": false, "inner": inner, "total": total }));
            }
            let listing = fsops::list(&r.real)?;
            let space = fsops::space(&r.real).map(|(free, total)| json!({"free": free, "total": total}));
            reply(
                out,
                &json!({ "entries": listing.entries, "writable": listing.writable, "space": space, "recycle": r.share.recycle }),
            )
        }
        "stat" => {
            let real = path_param("path")?;
            if let Some((file, inner, members)) = in_archive(&real)?
                && !inner.is_empty()
            {
                let m = archive::find(&members, &inner);
                let name = inner.rsplit('/').next().unwrap_or_default();
                let archive_name = file.file_name().unwrap_or_default().to_string_lossy();
                return reply(out, &json!({ "name": name, "kind": if m.is_some() { "f" } else { "d" }, "size": m.map_or(0, |m| m.size), "mtime": m.map_or(0, |m| m.mtime), "mode": m.map_or(0o755, |m| m.mode), "owner": archive_name, "group": "", "archive": true }));
            }
            reply(out, &fsops::details(&real)?)
        }
        "media" => {
            let real = path_param("path")?;
            // inside an archive there is no file to read: nothing to tell
            if in_archive(&real)?.is_some_and(|(_, inner, _)| !inner.is_empty()) {
                return reply(out, &json!({}));
            }
            reply(out, &crate::media::describe(&real))
        }
        "usage" => {
            method(req, "POST")?;
            let body: Paths = req.json()?;
            if body.paths.first().is_some_and(|p| crate::conns::split(p).is_some()) {
                return remote.usage(out, conns, &body.paths);
            }
            let paths = body.paths.iter().map(|p| Ok(shares::resolve(shares, p)?.real)).collect::<Result<Vec<_>, ApiError>>()?;
            // folders inside an archive are counted from its listing
            if let [one] = paths.as_slice()
                && let Some((_, inner, members)) = in_archive(one)?
                && !inner.is_empty()
            {
                let (files, dirs, bytes) = archive::usage(&members, &inner);
                return reply(out, &json!({ "bytes": bytes, "files": files, "dirs": dirs + 1, "unreadable": 0, "partial": false }));
            }
            reply(out, &fsops::usage(&paths, Duration::from_secs(25)))
        }
        "space" => {
            let r = shares::resolve(shares, req.require("path")?)?;
            if in_archive(&r.real)?.is_some() {
                return Err(ApiError::read_only(t!("space analysis does not work inside an archive")));
            }
            reply(out, &space::analyse(&r.real, &r.share.path, Duration::from_secs(40)))
        }
        "mkdir" => {
            method(req, "POST")?;
            let dir = path_param("path")?;
            let name = req.require("name")?;
            if req.param("parents") == Some("1") {
                // folder uploads: the whole relative path, created as needed
                std::fs::create_dir_all(nested(&dir, name)?)?;
            } else {
                shares::valid_name(name)?;
                fsops::mkdir(&dir.join(name))?;
            }
            reply(out, &json!({ "ok": true }))
        }
        "rename" => {
            method(req, "POST")?;
            let r = shares::resolve(shares, req.require("path")?)?;
            if r.is_root() {
                return Err(ApiError::share_root(t!("a shared folder is renamed in its settings (Shared folders)")));
            }
            let name = req.require("name")?;
            shares::valid_name(name)?;
            let target = r.real.with_file_name(name);
            fsops::rename(&r.real, &target)?;
            reply(out, &json!({ "ok": true }))
        }
        "search" => {
            let root = path_param("path")?;
            if archive::locate(&root).is_some() {
                return Err(ApiError::read_only(t!("subfolder search does not work inside an archive")));
            }
            let pattern = fsops::Pattern::new(req.require("q")?);
            let limit = req.param("limit").and_then(|l| l.parse().ok()).unwrap_or(2000).min(10000);
            let budget = req.param("budget").and_then(|b| b.parse().ok()).unwrap_or(15u64).clamp(1, 60);
            reply(out, &fsops::search(&root, &pattern, limit, Duration::from_secs(budget)))
        }
        "download" => {
            if let Some(list) = req.param("paths") {
                let paths: Vec<String> = serde_json::from_str(list).map_err(|_| ApiError::bad_request("paths"))?;
                let real = paths.iter().map(|p| Ok(shares::resolve(shares, p)?.real)).collect::<Result<Vec<_>, ApiError>>()?;
                if real.len() == 1 && send_member(out, &real[0], false)? {
                    return Ok(());
                }
                if real.len() == 1 && std::fs::metadata(&real[0]).is_ok_and(|m| m.is_file()) {
                    return transfer::send_file(out, req, &real[0], false);
                }
                let name = match real.as_slice() {
                    [one] => one.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                    many => many[0]
                        .parent()
                        .and_then(|p| p.file_name())
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "download".into()),
                };
                if req.param("format") == Some("tar") {
                    return transfer::send_tar(out, &real, &format!("{name}.tar"));
                }
                return transfer::send_zip(out, &real, &format!("{name}.zip"));
            }
            let real = path_param("path")?;
            if send_member(out, &real, false)? {
                return Ok(());
            }
            transfer::send_file(out, req, &real, false)
        }
        "view" => {
            let real = path_param("path")?;
            if send_member(out, &real, true)? {
                return Ok(());
            }
            transfer::send_file(out, req, &real, true)
        }
        "thumb" => {
            let file = path_param("path")?;
            let data = crate::thumbs::get(&file, req.param("size") == Some("big"), ctx.thumbs.as_deref()).ok_or_else(|| ApiError::not_found(t!("no thumbnail")))?;
            let length = data.len().to_string();
            write!(
                out,
                "Status: 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {length}\r\nCache-Control: private, max-age=86400\r\n\r\n"
            )?;
            out.write_all(&data)?;
            Ok(())
        }
        "text" => {
            // the first part of a text file, for the preview
            let path = path_param("path")?;
            let mut buf = Vec::new();
            use std::io::Read;
            let size = match in_archive(&path)? {
                Some((file, inner, members)) if !inner.is_empty() => {
                    let m = archive::find(&members, &inner).ok_or_else(|| ApiError::not_found(t!("item not found in the archive")))?;
                    let mut child = archive::cat(&file, m).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn()?;
                    child.stdout.take().unwrap().take(512 * 1024).read_to_end(&mut buf)?;
                    let _ = child.kill();
                    let _ = child.wait();
                    m.size
                }
                _ => {
                    std::fs::File::open(&path)?.take(512 * 1024).read_to_end(&mut buf)?;
                    std::fs::metadata(&path)?.len()
                }
            };
            let binary = buf.iter().take(8192).any(|&b| b == 0);
            reply(out, &json!({ "text": if binary { String::new() } else { String::from_utf8_lossy(&buf).into_owned() }, "binary": binary, "size": size, "truncated": size > buf.len() as u64 }))
        }
        "edit" => {
            let path = path_param("path")?;
            if in_archive(&path)?.is_some_and(|(_, inner, _)| !inner.is_empty()) {
                return Err(ApiError::read_only(t!("inside an archive it is read-only")));
            }
            crate::edit::open(out, &path)
        }
        "edit/save" => {
            method(req, "POST")?;
            let path = path_param("path")?;
            let text = crate::edit::body(req)?;
            crate::edit::save(out, &path, &text, req.param("stamp").unwrap_or_default(), req.param("force") == Some("1"))
        }
        "upload" => {
            method(req, "POST")?;
            let dir = path_param("dir")?;
            let name = req.require("name")?;
            shares::valid_name(name)?;
            reply(out, &transfer::upload(req, &dir, name)?)
        }
        "upload/abort" => {
            method(req, "POST")?;
            let dir = path_param("dir")?;
            let name = req.require("name")?;
            shares::valid_name(name)?;
            let id = transfer::upload_id(req)?;
            let _ = std::fs::remove_file(transfer::LocalUpload::new(&dir, name, &id).part());
            reply(out, &json!({ "ok": true }))
        }
        "exists" => {
            // which of these names are taken in a folder: the upload asks before sending
            method(req, "POST")?;
            let dir = path_param("path")?;
            let names: Vec<String> = req.json()?;
            let taken: Vec<Value> = names
                .iter()
                .filter(|n| shares::valid_name(n).is_ok())
                .filter_map(|n| {
                    let m = std::fs::symlink_metadata(dir.join(n)).ok()?;
                    use std::os::unix::fs::MetadataExt;
                    Some(json!({ "name": n, "size": m.len(), "mtime": m.mtime(), "dir": m.is_dir() }))
                })
                .collect();
            reply(out, &json!({ "taken": taken }))
        }
        _ => Err(ApiError::not_found(format!("no route {}", req.route))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_folders_for_uploads() {
        let dir = Path::new("/volume1/Foto");
        assert_eq!(nested(dir, "2024").unwrap(), dir.join("2024"));
        assert_eq!(nested(dir, "2024/Mare/giorno 1").unwrap(), dir.join("2024/Mare/giorno 1"));
        for bad in ["", "/2024", "2024/", "a//b", "a/../b", "..", "./a", "a/.", "a/ /b", "a/b\0c"] {
            assert!(nested(dir, bad).is_err(), "{bad:?}");
        }
        assert!(nested(dir, &format!("a/{}", "x".repeat(256))).is_err());
    }
}
