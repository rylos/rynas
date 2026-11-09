//! The API for remote connections: managing them (rynasd, which owns the store:
//! [`conns`]) and browsing them (a worker, as the user: any route whose path is
//! `/:<id>/…`, and [`test`]).

use std::io::{self, Read, Seek, SeekFrom, Write};

use serde_json::{Value, json};

use super::{Conn, Connect, Entry, Fs, Kind};
use rynas_files::conn_routes::{first_of_paths, remote_param};
use crate::wire::{self as cgi, ApiError, Request};
use crate::transfer::{self, UploadStore};

fn reply(out: &mut impl Write, value: &impl serde::Serialize) -> Result<(), ApiError> {
    cgi::json(out, 200, value)?;
    Ok(())
}

fn post(req: &Request) -> Result<(), ApiError> {
    if req.method == "POST" { Ok(()) } else { Err(ApiError::new(405, "use POST")) }
}

/// `conns/test`, the worker's half: logs in to the connection rynasd prepared and lists
/// where it opens.
pub fn test(req: &Request, out: &mut impl Write, conns: &[Conn]) -> Result<(), ApiError> {
    post(req)?;
    let c = conns.first().ok_or_else(|| ApiError::bad_request("no connection to test"))?;
    let start = std::time::Instant::now();
    let (fs, home, host_key) = match c.kind {
        Kind::Smb => (c.connect().map_err(ApiError::from)?, String::new(), String::new()),
        Kind::Sftp => {
            let s = super::sftp::Sftp::connect(c).map_err(ApiError::from)?;
            let (home, key) = (s.home.clone(), s.host_key.clone());
            (Box::new(s) as Box<dyn Fs>, home, key)
        }
    };
    let parts = start_parts(c, &home);
    let items = fs.list(&parts).map_err(ApiError::from)?;
    reply(out, &json!({ "ok": true, "items": items.len(), "ms": start.elapsed().as_millis() as u64, "home": home, "host_key": host_key, "path": parts.join("/") }))
}

/// Where a connection opens, below its root.
fn start_parts(c: &Conn, home: &str) -> Vec<String> {
    let path = if c.path.is_empty() && c.kind == Kind::Sftp { home } else { &c.path };
    path.split('/').filter(|p| !p.is_empty()).map(String::from).collect()
}

/// A connection, the server opened, and the parts of the path below it.
type Opened<'a> = (&'a Conn, Box<dyn Fs>, Vec<String>);

/// Opens the connection a path names.
fn open<'a>(conns: &'a [Conn], vpath: &str) -> Result<Opened<'a>, ApiError> {
    let (id, parts) = super::split(vpath).ok_or_else(|| ApiError::bad_request("invalid path"))??;
    let conn = conns
        .iter()
        .find(|c| c.id == id)
        .ok_or_else(|| ApiError::not_found(t!("no such connection: was it deleted?")))?;
    let fs = conn.connect().map_err(ApiError::from)?;
    Ok((conn, fs, parts))
}

/// A route about a remote path, or None for the shared folders.
pub fn serve(req: &Request, out: &mut impl Write, conns: &[Conn]) -> Option<Result<(), ApiError>> {
    if req.route == "download"
        && let Some(paths) = first_of_paths(req)
    {
        return Some(download(req, out, conns, &paths));
    }
    let vpath = remote_param(req)?;
    Some(route(req, out, conns, vpath))
}

fn route(req: &Request, out: &mut impl Write, conns: &[Conn], vpath: &str) -> Result<(), ApiError> {
    match req.route.as_str() {
        "list" => {
            let (conn, fs, parts) = open(conns, vpath)?;
            let entries = fs.list(&parts)?;
            let top = fs.is_top(&parts);
            let space = if top { None } else { fs.space(&parts) }.map(|(free, total)| json!({"free": free, "total": total}));
            reply(out, &json!({ "entries": entries, "writable": !top, "space": space, "recycle": false, "remote": { "kind": conn.kind, "top": top } }))
        }
        "stat" => {
            let (conn, fs, parts) = open(conns, vpath)?;
            let e = fs.stat(&parts)?;
            let name = if parts.is_empty() { conn.name.clone() } else { e.name.clone() };
            reply(out, &json!({ "name": name, "kind": e.kind, "size": e.size, "mtime": e.mtime, "mode": e.mode, "owner": "", "group": "", "remote": true, "server": conn.host }))
        }
        "download" | "view" => {
            let (_, fs, parts) = open(conns, vpath)?;
            if fs.stat(&parts)?.is_dir() {
                return zip(out, fs.as_ref(), &[parts]);
            }
            send(out, req, fs.as_ref(), &parts, req.route == "view")
        }
        "text" => {
            let (_, fs, parts) = open(conns, vpath)?;
            let mut r = fs.reader(&parts)?;
            let size = r.len();
            let mut buf = Vec::new();
            r.by_ref().take(512 * 1024).read_to_end(&mut buf)?;
            let binary = buf.iter().take(8192).any(|&b| b == 0);
            reply(out, &json!({ "text": if binary { String::new() } else { String::from_utf8_lossy(&buf).into_owned() }, "binary": binary, "size": size, "truncated": size > buf.len() as u64 }))
        }
        "edit" => {
            let (_, fs, parts) = open(conns, vpath)?;
            let e = fs.stat(&parts)?;
            if e.is_dir() {
                return Err(ApiError::bad_request(t!("not a file")));
            }
            if e.size > crate::edit::MAX {
                return Err(crate::edit::too_big());
            }
            let mut buf = Vec::new();
            fs.reader(&parts)?.read_to_end(&mut buf)?;
            let text = crate::edit::as_text(buf)?;
            reply(out, &json!({ "text": text, "stamp": format!("{}-{}", e.mtime, e.size), "writable": true }))
        }
        "edit/save" => {
            post(req)?;
            let (_, fs, parts) = open(conns, vpath)?;
            let text = crate::edit::body(req)?;
            let e = fs.stat(&parts)?;
            if req.param("force") != Some("1") && req.param("stamp") != Some(format!("{}-{}", e.mtime, e.size).as_str()) {
                return Err(ApiError::conflict(t!("the file changed on the server since you opened it")));
            }
            // in place, so it keeps its permissions on the server
            let mut w = fs.writer(&parts, None)?;
            w.write_all(&text)?;
            w.finish()?;
            let e = fs.stat(&parts)?;
            reply(out, &json!({ "stamp": format!("{}-{}", e.mtime, e.size) }))
        }
        "mkdir" => {
            post(req)?;
            let (_, fs, parts) = open(conns, vpath)?;
            let name = req.require("name")?;
            if req.param("parents") == Some("1") {
                let mut path = parts;
                for part in name.split('/') {
                    super::valid_name(part)?;
                    path.push(part.to_string());
                    match fs.mkdir(&path) {
                        Err(e) if e.raw_os_error() != Some(libc::EEXIST) => return Err(e.into()),
                        _ => {}
                    }
                }
            } else {
                super::valid_name(name)?;
                let mut path = parts;
                path.push(name.to_string());
                fs.mkdir(&path).map_err(|e| {
                    if e.raw_os_error() == Some(libc::EEXIST) { ApiError::exists(t!("an item with this name already exists")) } else { e.into() }
                })?;
            }
            reply(out, &json!({ "ok": true }))
        }
        "rename" => {
            post(req)?;
            let (conn, fs, parts) = open(conns, vpath)?;
            if parts.is_empty() || (conn.kind == Kind::Smb && parts.len() == 1) {
                return Err(ApiError::bad_request(t!("this is renamed on the server")));
            }
            let name = req.require("name")?;
            super::valid_name(name)?;
            let mut to = parts.clone();
            *to.last_mut().unwrap() = name.to_string();
            fs.rename(&parts, &to)?;
            reply(out, &json!({ "ok": true }))
        }
        "exists" => {
            post(req)?;
            let (_, fs, parts) = open(conns, vpath)?;
            let names: Vec<String> = req.json()?;
            // one listing answers for every name
            let here: Vec<Entry> = fs.list(&parts).unwrap_or_default();
            let taken: Vec<Value> = names
                .iter()
                .filter_map(|n| here.iter().find(|e| e.name == *n))
                .map(|e| json!({ "name": e.name, "size": e.size, "mtime": e.mtime, "dir": e.is_dir() }))
                .collect();
            reply(out, &json!({ "taken": taken }))
        }
        "upload" => {
            post(req)?;
            let (_, fs, dir) = open(conns, vpath)?;
            let name = req.require("name")?;
            super::valid_name(name)?;
            let up = transfer::Upload::from_request(req)?;
            let mut store = RemoteUpload::new(fs.as_ref(), &dir, name, &up.id);
            reply(out, &transfer::receive(&mut store, &up, &mut crate::wire::body())?)
        }
        "upload/abort" => {
            post(req)?;
            let (_, fs, dir) = open(conns, vpath)?;
            let name = req.require("name")?;
            super::valid_name(name)?;
            let id = transfer::upload_id(req)?;
            RemoteUpload::new(fs.as_ref(), &dir, name, &id).remove_part();
            reply(out, &json!({ "ok": true }))
        }
        "thumb" => Err(ApiError::not_found(t!("no thumbnail"))),
        "search" | "space" => Err(ApiError::bad_request(t!("not available on remote connections"))),
        _ => Err(ApiError::not_found(format!("no route {}", req.route))),
    }
}

fn child(dir: &[String], name: &str) -> Vec<String> {
    let mut p = dir.to_vec();
    p.push(name.to_string());
    p
}

/// Sizes of remote items, for the info panel.
pub fn usage(out: &mut impl Write, conns: &[Conn], paths: &[String]) -> Result<(), ApiError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
    let (_, fs, _) = open(conns, paths.first().map(String::as_str).unwrap_or_default())?;
    let (mut bytes, mut files, mut dirs, mut unreadable, mut partial) = (0u64, 0u64, 0u64, 0u64, false);
    let mut stack = Vec::new();
    for p in paths {
        let (_, parts) = super::split(p).ok_or_else(|| ApiError::bad_request("invalid path"))??;
        match fs.stat(&parts) {
            Ok(e) if e.is_dir() => stack.push(parts),
            Ok(e) => {
                files += 1;
                bytes += e.size;
            }
            Err(_) => unreadable += 1,
        }
    }
    while let Some(dir) = stack.pop() {
        if std::time::Instant::now() > deadline {
            partial = true;
            break;
        }
        dirs += 1;
        match fs.list(&dir) {
            Ok(items) => {
                for e in items {
                    if e.is_dir() {
                        stack.push(child(&dir, &e.name));
                    } else {
                        files += 1;
                        bytes += e.size;
                    }
                }
            }
            Err(_) => unreadable += 1,
        }
    }
    reply(out, &json!({ "bytes": bytes, "files": files, "dirs": dirs, "unreadable": unreadable, "partial": partial }))
}

fn download(req: &Request, out: &mut impl Write, conns: &[Conn], paths: &[String]) -> Result<(), ApiError> {
    let (_, fs, first) = open(conns, &paths[0])?;
    let mut all = Vec::new();
    for p in paths {
        let (_, parts) = super::split(p).ok_or_else(|| ApiError::bad_request("invalid path"))??;
        all.push(parts);
    }
    if all.len() == 1 && !fs.stat(&first)?.is_dir() {
        return send(out, req, fs.as_ref(), &first, false);
    }
    zip(out, fs.as_ref(), &all)
}

/// One file, with ranges: video and PDF viewers seek.
fn send(out: &mut impl Write, req: &Request, fs: &dyn Fs, parts: &[String], inline: bool) -> Result<(), ApiError> {
    let e = fs.stat(parts)?;
    let mut r = fs.reader(parts)?;
    let size = r.len();
    let name = parts.last().cloned().unwrap_or_default();
    let content_type = if inline { transfer::mime(&name) } else { "application/octet-stream" };
    let disposition = cgi::disposition(if inline { "inline" } else { "attachment" }, &name);
    let modified = e.mtime.to_string();
    let (status, start, len) = match req.range.as_deref() {
        Some(range) => match cgi::parse_range(range, size) {
            Some((a, b)) => (206, a, b - a + 1),
            None => {
                let unsatisfied = format!("bytes */{size}");
                cgi::head(out, 416, &[("Content-Range", &unsatisfied)])?;
                return Ok(());
            }
        },
        None => (200, 0, size),
    };
    let length = len.to_string();
    let content_range = format!("bytes {start}-{}/{size}", start + len.saturating_sub(1));
    let mut headers = vec![
        ("Content-Type", content_type),
        ("Content-Disposition", disposition.as_str()),
        ("Content-Length", length.as_str()),
        ("Accept-Ranges", "bytes"),
        ("X-Mtime", modified.as_str()),
    ];
    if inline {
        headers.push(("Content-Security-Policy", "default-src 'none'; img-src 'self' data:; media-src 'self'; style-src 'unsafe-inline'; sandbox"));
    }
    if status == 206 {
        headers.push(("Content-Range", content_range.as_str()));
    }
    cgi::head(out, status, &headers)?;
    r.seek(SeekFrom::Start(start))?;
    copy(&mut r.take(len), out)?;
    Ok(())
}

/// Copies in large blocks: each read of a server is a round trip.
fn copy(r: &mut impl Read, w: &mut impl Write) -> io::Result<u64> {
    let mut buf = vec![0u8; 1024 * 1024];
    let mut n = 0;
    loop {
        let k = r.read(&mut buf)?;
        if k == 0 {
            return Ok(n);
        }
        w.write_all(&buf[..k])?;
        n += k as u64;
    }
}

/// Folders, or several items, as one .zip made on the fly.
fn zip(out: &mut impl Write, fs: &dyn Fs, roots: &[Vec<String>]) -> Result<(), ApiError> {
    let name = match roots {
        [one] => one.last().cloned().unwrap_or_else(|| "download".into()),
        many => many[0].iter().rev().nth(1).cloned().unwrap_or_else(|| "download".into()),
    };
    let disposition = cgi::disposition("attachment", &format!("{name}.zip"));
    cgi::head(out, 200, &[("Content-Type", "application/zip"), ("Content-Disposition", &disposition)])?;
    let mut zip = crate::zip::ZipWriter::new(out);
    let mut none = |_n: u64| Ok(());
    for root in roots {
        let base = root.len().saturating_sub(1);
        let Ok(top) = fs.stat(root) else { continue };
        let mut stack = vec![(root.clone(), top)];
        while let Some((path, e)) = stack.pop() {
            let rel = path[base..].join("/");
            if e.is_dir() {
                zip.add_dir(&rel, e.mtime, 0o40755)?;
                let Ok(mut items) = fs.list(&path) else { continue };
                items.sort_by(|a, b| b.name.cmp(&a.name));
                stack.extend(items.into_iter().map(|i| (child(&path, &i.name), i)));
            } else if e.kind == 'f' {
                let Ok(mut r) = fs.reader(&path) else { continue };
                zip.add_file(&rel, e.mtime, 0o100644, &mut r, crate::zip::Level::Store, &mut none)?;
            }
        }
    }
    zip.finish()?;
    Ok(())
}

/// An upload into a folder of a server: the same rules as on the NAS
/// (`transfer::receive`). Both SMB and SFTP write at any offset, past the
/// end of the file too, so chunks may come in any order.
struct RemoteUpload<'a> {
    fs: &'a dyn Fs,
    dir: Vec<String>,
    target: Vec<String>,
    part: Vec<String>,
    /// the original's time, set on the file once renamed: on the hidden
    /// file it would make it look stale to another upload's sweep
    mtime: std::cell::Cell<Option<i64>>,
    /// the old file could not be put back: the part is the only complete copy
    keep_part: std::cell::Cell<bool>,
}

impl<'a> RemoteUpload<'a> {
    fn new(fs: &'a dyn Fs, dir: &[String], name: &str, id: &str) -> Self {
        Self { fs, dir: dir.to_vec(), target: child(dir, name), part: child(dir, &transfer::upload_part_name(name, id)), mtime: Default::default(), keep_part: Default::default() }
    }
}

impl transfer::UploadStore for RemoteUpload<'_> {
    fn target(&self) -> io::Result<Option<transfer::Existing>> {
        // servers answer a missing name in their own ways: any failure is none
        Ok(self.fs.stat(&self.target).ok().map(|e| transfer::Existing { dir: e.is_dir(), file: e.kind == 'f', size: e.size, mtime: e.mtime }))
    }

    fn write(&mut self, create: bool, at: u64, body: &mut dyn Read, length: u64) -> io::Result<Option<u64>> {
        let exists = self.fs.stat(&self.part).is_ok();
        let mut w = match (exists, create) {
            (true, _) => self.fs.writer(&self.part, Some(at))?,
            // the first chunk creates it; sent again, it opens it as it is
            (false, true) => self.fs.writer(&self.part, None)?,
            (false, false) => return Ok(None),
        };
        let copied = copy(&mut body.take(length), &mut w)?;
        w.finish()?;
        Ok(Some(copied))
    }

    fn complete(&self, total: u64) -> io::Result<bool> {
        Ok(self.fs.stat(&self.part).is_ok_and(|e| e.size == total))
    }

    fn set_mtime(&self, mtime: i64) {
        self.mtime.set(Some(mtime));
    }

    fn commit(&self, replace: bool) -> io::Result<()> {
        // a server's rename never replaces
        if replace && self.fs.stat(&self.target).is_ok_and(|e| !e.is_dir()) {
            super::replace_file(self.fs, &self.part, &self.target, &self.keep_part)?;
        } else {
            self.fs.rename(&self.part, &self.target)?;
        }
        if let Some(mtime) = self.mtime.get() {
            self.fs.set_mtime(&self.target, mtime);
        }
        Ok(())
    }

    fn remove_part(&self) {
        if !self.keep_part.get() {
            let _ = self.fs.remove(&self.part, false);
        }
    }

    fn sweep(&self) {
        let Ok(items) = self.fs.list(&self.dir) else { return };
        let day_ago = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64) - 24 * 3600;
        for e in items {
            if e.kind == 'f' && e.name.starts_with('.') && e.name.ends_with(&format!(".{}", transfer::UPLOAD_TAG)) && e.mtime < day_ago {
                let _ = self.fs.remove(&child(&self.dir, &e.name), false);
            }
        }
    }
}
