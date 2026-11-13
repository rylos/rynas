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
    // what this request opens must lie in the shares its paths name (crate::confine)
    let jail = crate::confine::Jail::enter();
    let result = route(&req, &mut out, &w.ctx, remote);
    drop(jail);
    cgi::discard_unread(req.length);
    if let Err(err) = result {
        cgi::error(&mut out, &err)?;
    }
    out.flush()
}

fn method(req: &Request, want: &str) -> Result<(), ApiError> {
    if req.method == want { Ok(()) } else { Err(ApiError::new(405, format!("use {want}"))) }
}

/// Routes that change files (all POST); every other route only reads. rynasd decides the
/// powers of a request's worker from this list too.
pub const WRITING_ROUTES: &[&str] = &["mkdir", "rename", "edit/save", "upload", "upload/abort"];

/// Refuses a writing route ([`WRITING_ROUTES`]) whose `path` or `dir` lies in a read-only
/// share ([`Share::read_only`]: a PC's backup, or a share that holds one). Paths that do not
/// resolve are left to the route (its own error); remote paths never name a share.
pub fn refuse_read_only(shares: &[Share], route: &str, param: impl Fn(&str) -> Option<String>) -> Result<(), ApiError> {
    if !WRITING_ROUTES.contains(&route) {
        return Ok(());
    }
    for key in ["path", "dir"] {
        if let Some(vpath) = param(key)
            && let Ok(r) = shares::resolve(shares, &vpath)
        {
            r.share.refuse_writes()?;
        }
    }
    Ok(())
}

/// The final guard of a writing route ([`WRITING_ROUTES`]): the folder it writes in, every
/// link followed, is never in a PC's backup folder, nor (rename) a folder that holds one
/// ([`crate::confine::refuse_pc_write`]: real path, then the identities up to `/`, so that a
/// share that is a link to a backup, a bind mount or a home holding one changes nothing).
fn guard_pc_writes(shares: &[Share], req: &Request) -> Result<(), ApiError> {
    if !WRITING_ROUTES.contains(&req.route.as_str()) {
        return Ok(());
    }
    for key in ["path", "dir"] {
        let Some(vpath) = req.param(key) else { continue };
        let Ok(r) = shares::resolve(shares, vpath) else { continue };
        let rename = req.route == "rename";
        let Ok(real) = place(&r, !rename) else { continue };
        match req.route.as_str() {
            "rename" => crate::confine::refuse_pc_write(&real, true)?,
            "edit/save" => crate::confine::refuse_pc_write(real.parent().unwrap_or(&real), false)?,
            // mkdir, upload, upload/abort: the folder named
            _ => crate::confine::refuse_pc_write(&real, false)?,
        }
    }
    Ok(())
}

/// Refuses to measure, analyse or zip a PC's whole backup folder (`/@backups/<pc>`): it holds
/// every snapshot of the PC, mostly the same data again and again, and the sum means nothing.
fn refuse_whole_backup(shares: &[Share], vpaths: &[String]) -> Result<(), ApiError> {
    for p in vpaths {
        if let Ok(r) = shares::resolve(shares, p)
            && r.share.backup.is_some()
            && r.is_root()
        {
            return Err(ApiError::invalid("path", t!("a PC's backup holds all its snapshots: open one of them")));
        }
    }
    Ok(())
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
/// The apps' folders, when the context has them, come apart in `apps`.
pub fn shares_info(ctx: &Context) -> Value {
    let list: Vec<Value> = ctx
        .shares
        .iter()
        .filter(|s| s.app.is_none() && s.backup.is_none())
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
    let mut v = json!({ "shares": list, "user": ctx.user.name, "volumes": ctx.volumes, "hostname": ctx.hostname });
    // the apps' folders are one entry of the interface (`@apps`), not shared folders
    let apps = group_folders(&ctx.shares, shares::APPS);
    if !apps.is_empty() {
        v["apps"] = json!(apps);
    }
    // and so are the PCs' backups (`@backups`)
    let backups = group_folders(&ctx.shares, shares::BACKUPS);
    if !backups.is_empty() {
        v["backups"] = json!(backups);
    }
    v
}

/// The shares of a group folder ([`shares::GROUPS`]) with their names below it.
fn members<'a>(shares: &'a [Share], group: &'a str) -> impl Iterator<Item = (&'a Share, &'a str)> + 'a {
    shares
        .iter()
        .filter(move |s| if group == shares::APPS { s.app.is_some() } else { s.backup.is_some() })
        .filter_map(move |s| Some((s, shares::member_name(s, group)?)))
}

/// What the interface knows of a group's share besides its name: the app's stack and
/// catalogue id; a PC's backup is fixed (`readonly`: never renamed, moved or deleted).
fn member_info(s: &Share, v: &mut Value) {
    if let Some(app) = &s.app {
        v["stack"] = json!(app.stack);
        if let Some(id) = &app.app {
            v["app"] = json!(id);
        }
    }
    if s.backup.is_some() {
        v["readonly"] = json!(true);
    }
}

/// The apps' folders (`@apps/<folder>`: `{name, stack, app?}` each) or the PCs' backups
/// (`@backups/<pc>`: `{name, readonly}`), by name.
fn group_folders(shares: &[Share], group: &str) -> Vec<Value> {
    members(shares, group)
        .map(|(s, name)| {
            let mut v = json!({ "name": name });
            member_info(s, &mut v);
            v
        })
        .collect()
}

/// `list` of `/@apps` or `/@backups`: one folder per app or PC that is there, never writable
/// here (each is made by its app, or by the PC's backups).
fn group_listing(shares: &[Share], group: &str) -> Value {
    use std::os::unix::fs::MetadataExt;
    let entries: Vec<Value> = members(shares, group)
        .filter_map(|(s, name)| {
            let meta = std::fs::metadata(&s.path).ok()?;
            let mut v = json!({ "name": name, "kind": "d", "size": 0, "mtime": meta.mtime(), "mode": meta.mode() & 0o7777 });
            member_info(s, &mut v);
            Some(v)
        })
        .collect();
    let mut v = json!({ "entries": entries, "writable": false, "space": null, "recycle": false });
    if group == shares::APPS {
        v["apps"] = json!(true);
    } else {
        v["backups"] = json!(true);
        v["read_only"] = json!(true);
    }
    v
}

/// A listing in a read-only share: nothing writable, every entry fixed (no rename, move or
/// delete). rynas' staging folder of a PC's backup is never listed (`confine::read_dir`).
fn read_only_listing(listing: &mut fsops::Listing) {
    listing.writable = false;
    for e in &mut listing.entries {
        e.readonly = true;
    }
}

/// `stat` of a folder that only holds other folders (`/@apps`, `/@backups`).
fn virtual_root(name: &str) -> Value {
    json!({ "name": name, "kind": "d", "size": 0, "mtime": 0, "atime": 0, "ctime": 0, "mode": 0o755, "owner": "root", "group": "root", "inode": 0, "links": 1, "target": null, "readable": true, "writable": false, "device": 0 })
}

/// `/@backups` is there only for whom it holds PCs' folders (administrators): anyone else
/// gets 404, as for any name that is not a share. (`/@apps` stays an empty list.)
fn group_shown(shares: &[Share], group: &str) -> Result<(), ApiError> {
    if group == shares::BACKUPS && members(shares, group).next().is_none() {
        return Err(ApiError::not_found(t!("no shared folder {}", group)));
    }
    Ok(())
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

/// `mkdir -p` of `rel` below the real folder `dir`: a link met on the way is followed only
/// while it stays inside the share.
fn make_dirs(dir: &Path, rel: &str) -> Result<(), ApiError> {
    nested(dir, rel)?;
    let mut at = dir.to_path_buf();
    for part in rel.split('/') {
        // the folder this level is made in (after a link met on the way): never a PC's backup
        crate::confine::refuse_pc_write(&at, false)?;
        at.push(part);
        match std::fs::create_dir(&at) {
            Ok(()) => {
                crate::confine::made_dir(&at)?;
                continue;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        let meta = std::fs::symlink_metadata(&at)?;
        if meta.file_type().is_symlink() {
            at = match crate::confine::link_target(&at) {
                Some(target) => target?,
                None => std::fs::canonicalize(&at)?,
            };
        } else if !meta.is_dir() {
            return Err(ApiError::exists(t!("an item with this name already exists")));
        }
    }
    Ok(())
}

/// The real path of a request's path ([`shares::Resolved::target`] when `follow`, else
/// [`shares::Resolved::entry`]). A path inside an archive file ("/docs/a.zip/2024/x.jpg")
/// comes back as the archive's real path with the rest joined, for [`in_archive`].
fn place(r: &shares::Resolved, follow: bool) -> Result<PathBuf, ApiError> {
    let root = r.root()?;
    match root.resolve(&r.rel, follow) {
        Ok(p) => Ok(p),
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOTDIR) | Some(libc::ENOENT)) => {
            // the archive is the longest part of the path that exists
            let parts: Vec<_> = r.rel.components().collect();
            for k in (1..parts.len()).rev() {
                if let Ok(file) = root.resolve(&parts[..k].iter().collect::<PathBuf>(), true) {
                    if archive::locate(&file).is_some_and(|(_, inner)| inner.is_empty()) {
                        return Ok(file.join(parts[k..].iter().collect::<PathBuf>()));
                    }
                    break;
                }
            }
            Err(e.into())
        }
        Err(e) => Err(e.into()),
    }
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
    /// compress: "zip" (the default) or "tar.zst"
    #[serde(default)]
    pub format: Option<String>,
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
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut add_root = |share: &Share| {
        if !roots.contains(&share.path) {
            roots.push(share.path.clone());
        }
    };
    let mut app_roots: Vec<PathBuf> = Vec::new();
    // the shares (never the apps' folders) the job writes in
    let mut write_roots: Vec<PathBuf> = Vec::new();
    let mut writes_in = |share: &Share| {
        if share.app.is_none() && !write_roots.contains(&share.path) {
            write_roots.push(share.path.clone());
        }
    };
    let mut remotes: Vec<crate::conns::Conn> = Vec::new();
    let mut private_roots: Vec<PathBuf> = Vec::new();
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
        add_root(r.share);
        if matches!(body.op, Op::Delete | Op::Move) {
            // a read-only share (a PC's backup) is copied out of, never emptied
            r.share.refuse_writes()?;
            writes_in(r.share);
        }
        // a deletion, or a move's removal of its sources, changes an app's folder
        if r.share.app.is_some() && matches!(body.op, Op::Delete | Op::Move) && !app_roots.contains(&r.share.path) {
            app_roots.push(r.share.path.clone());
        }
        // out of a PC's backup, copies keep their sources' privacy (worker side)
        if r.share.read_only && !private_roots.contains(&r.share.path) {
            private_roots.push(r.share.path.clone());
        }
        sources.push(r.real);
    }
    let mut dest_in_app = false;
    let dest = match (&body.dest, body.op) {
        (_, Op::Delete) => None,
        (Some(d), _) if crate::conns::split(d).is_some() => {
            let (path, _) = remote_path(d).expect("a remote path")?;
            if crate::conns::split(d).is_some_and(|r| r.is_ok_and(|(_, parts)| parts.is_empty())) {
                return Err(ApiError::invalid("dest", t!("choose a shared folder of the server")));
            }
            Some(path)
        }
        (Some(d), _) => {
            let r = shares::resolve(shares, d)?;
            // nothing is copied, moved, extracted or compressed into a read-only share
            r.share.refuse_writes()?;
            add_root(r.share);
            writes_in(r.share);
            if r.share.app.is_some() {
                dest_in_app = true;
                if !app_roots.contains(&r.share.path) {
                    app_roots.push(r.share.path.clone());
                }
            }
            Some(r.real)
        }
        (None, _) => return Err(ApiError::invalid("dest", t!("no destination"))),
    };
    // a move between an app's folder and a share (or a server) writes one side with the
    // powers over owners and the other as the user: the copy would succeed and the removal
    // of the sources fail half-way, leaving duplicates
    if body.op == Op::Move {
        let app_sources = body.sources.iter().filter(|p| shares::in_apps(p)).count();
        if (dest_in_app && app_sources < body.sources.len()) || (!dest_in_app && app_sources > 0) {
            return Err(ApiError::invalid(
                "dest",
                t!("items cannot be moved between an app's folder and a shared folder: copy them, then delete the originals"),
            ));
        }
    }
    if let Some(name) = &body.name {
        shares::valid_name(name)?;
    }
    if body.format.as_deref().is_some_and(|f| !matches!(f, "zip" | "tar.zst")) {
        return Err(ApiError::invalid("format", t!("unknown archive format")));
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
        format: body.format.clone(),
        into_folder: body.into_folder,
        password: body.password.clone(),
        members: body.members.iter().map(|m| valid_member(m)).collect::<Result<_, _>>()?,
        remotes,
        roots,
        // the powers to change the apps' folders only when that is where the job writes: a
        // move out of an app's folder into a share writes in the share as the user
        app_roots: if body.op == Op::Delete || dest_in_app { app_roots } else { Vec::new() },
        write_roots,
        private_roots,
        // set by the manager when an administrator's job starts
        pc_roots: None,
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
    // the real path of a parameter, every link followed (inside its share)
    let path_param = |name: &str| -> Result<PathBuf, ApiError> { place(&shares::resolve(shares, req.require(name)?)?, true) };
    let entries = |paths: &[String]| paths.iter().map(|p| place(&shares::resolve(shares, p)?, false)).collect::<Result<Vec<_>, ApiError>>();
    // nothing is written in a read-only share (rynasd refuses it first; this holds anyway)
    refuse_read_only(shares, &req.route, |k| req.param(k).map(str::to_string))?;
    // the final guard of an administrator's worker: never a write in a PC's backup folder,
    // whatever share or link leads there
    crate::confine::set_guard(ctx.pc_roots.clone());
    guard_pc_writes(shares, req)?;
    match req.route.as_str() {
        "shares" => reply(out, &shares_info(ctx)),
        "list" => {
            let vpath = req.require("path")?;
            if let Some(group) = shares::group_root(vpath) {
                group_shown(shares, group)?;
                return reply(out, &group_listing(shares, group));
            }
            let r = shares::resolve(shares, vpath)?;
            let real = place(&r, true)?;
            if let Some((_, inner, members)) = in_archive(&real)? {
                if !archive::is_dir(&members, &inner) {
                    return Err(ApiError::invalid("path", t!("not a folder")));
                }
                let (files, dirs, bytes) = archive::usage(&members, &inner);
                let total = json!({ "files": files, "dirs": dirs, "bytes": bytes });
                return reply(out, &json!({ "entries": archive::children(&members, &inner), "writable": false, "space": null, "recycle": false, "inner": inner, "total": total }));
            }
            let mut listing = fsops::list(&real)?;
            if r.share.read_only {
                read_only_listing(&mut listing);
            }
            let space = fsops::space(&real).map(|(free, total)| json!({"free": free, "total": total}));
            let mut v = json!({ "entries": listing.entries, "writable": listing.writable, "space": space, "recycle": r.share.recycle });
            if r.share.read_only {
                v["read_only"] = json!(true);
            }
            // in an app's folder: which stack (the interface offers its restart after a change)
            if let Some(app) = &r.share.app {
                v["stack"] = json!(app.stack);
            }
            reply(out, &v)
        }
        "stat" => {
            let vpath = req.require("path")?;
            if let Some(group) = shares::group_root(vpath) {
                group_shown(shares, group)?;
                return reply(out, &virtual_root(group));
            }
            let r = shares::resolve(shares, vpath)?;
            let real = place(&r, false)?;
            if let Some((file, inner, members)) = in_archive(&real)?
                && !inner.is_empty()
            {
                let m = archive::find(&members, &inner);
                let name = inner.rsplit('/').next().unwrap_or_default();
                let archive_name = file.file_name().unwrap_or_default().to_string_lossy();
                return reply(out, &json!({ "name": name, "kind": if m.is_some() { "f" } else { "d" }, "size": m.map_or(0, |m| m.size), "mtime": m.map_or(0, |m| m.mtime), "mode": m.map_or(0o755, |m| m.mode), "owner": archive_name, "group": "", "archive": true }));
            }
            let mut details = fsops::details(&real)?;
            details.writable &= !r.share.read_only;
            reply(out, &details)
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
            refuse_whole_backup(shares, &body.paths)?;
            let paths = entries(&body.paths)?;
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
            refuse_whole_backup(shares, &[req.require("path")?.to_string()])?;
            let r = shares::resolve(shares, req.require("path")?)?;
            let real = place(&r, true)?;
            if in_archive(&real)?.is_some() {
                return Err(ApiError::read_only(t!("space analysis does not work inside an archive")));
            }
            reply(out, &space::analyse(&real, r.root()?.path(), Duration::from_secs(40)))
        }
        "mkdir" => {
            method(req, "POST")?;
            let dir = path_param("path")?;
            let name = req.require("name")?;
            if req.param("parents") == Some("1") {
                // folder uploads: the whole relative path, created as needed
                make_dirs(&dir, name)?;
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
            let real = place(&r, false)?;
            fsops::refuse_snapshot(&real)?;
            fsops::rename(&real, &real.with_file_name(name))?;
            reply(out, &json!({ "ok": true }))
        }
        "search" => {
            let r = shares::resolve(shares, req.require("path")?)?;
            let root = place(&r, true)?;
            if archive::locate(&root).is_some() {
                return Err(ApiError::read_only(t!("subfolder search does not work inside an archive")));
            }
            let pattern = fsops::Pattern::new(req.require("q")?);
            let limit = req.param("limit").and_then(|l| l.parse().ok()).unwrap_or(2000).min(10000);
            let budget = req.param("budget").and_then(|b| b.parse().ok()).unwrap_or(15u64).clamp(1, 60);
            let mut found = fsops::search(&root, &pattern, limit, Duration::from_secs(budget));
            if r.share.read_only {
                found.hits.iter_mut().for_each(|h| h.entry.readonly = true);
            }
            reply(out, &found)
        }
        "download" => {
            if let Some(list) = req.param("paths") {
                let paths: Vec<String> = serde_json::from_str(list).map_err(|_| ApiError::bad_request("paths"))?;
                refuse_whole_backup(shares, &paths)?;
                if let [one] = paths.as_slice() {
                    // one file: a link to it inside the share is followed
                    let file = place(&shares::resolve(shares, one)?, true)?;
                    if send_member(out, &file, false)? {
                        return Ok(());
                    }
                    if std::fs::metadata(&file).is_ok_and(|m| m.is_file()) {
                        return transfer::send_file(out, req, &file, false);
                    }
                }
                // folders and several items: links go into the archive as links
                let real = entries(&paths)?;
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
                    let file = crate::confine::open(&path)?;
                    let size = file.metadata()?.len();
                    file.take(512 * 1024).read_to_end(&mut buf)?;
                    size
                }
            };
            let binary = buf.iter().take(8192).any(|&b| b == 0);
            reply(out, &json!({ "text": if binary { String::new() } else { String::from_utf8_lossy(&buf).into_owned() }, "binary": binary, "size": size, "truncated": size > buf.len() as u64 }))
        }
        "edit" => {
            let r = shares::resolve(shares, req.require("path")?)?;
            let path = place(&r, true)?;
            if in_archive(&path)?.is_some_and(|(_, inner, _)| !inner.is_empty()) {
                return Err(ApiError::read_only(t!("inside an archive it is read-only")));
            }
            // a read-only share opens in the viewer: the editor never offers to save
            crate::edit::open(out, &path, r.share.read_only)
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
        let dir = Path::new("/srv/vol1/Photos");
        assert_eq!(nested(dir, "2024").unwrap(), dir.join("2024"));
        assert_eq!(nested(dir, "2024/Mare/giorno 1").unwrap(), dir.join("2024/Mare/giorno 1"));
        for bad in ["", "/2024", "2024/", "a//b", "a/../b", "..", "./a", "a/.", "a/ /b", "a/b\0c"] {
            assert!(nested(dir, bad).is_err(), "{bad:?}");
        }
        assert!(nested(dir, &format!("a/{}", "x".repeat(256))).is_err());
    }

    fn with_apps() -> Vec<Share> {
        let app = |stack: &str, path: &str| Share { name: format!("{}/{stack}", shares::APPS), path: path.into(), app: Some(shares::AppFolder { stack: stack.into(), app: Some("immich".into()) }), ..Default::default() };
        vec![Share { name: "Photos".into(), path: "/srv/vol1/Photos".into(), recycle: true, ..Default::default() }, app("immich", "/srv/vol1/docker/stacks/immich"), app("db", "/srv/ssd/docker/appdata/db")]
    }

    fn job(op: &str, sources: &[&str], dest: Option<&str>) -> NewJob {
        serde_json::from_value(json!({ "op": op, "sources": sources, "dest": dest })).unwrap()
    }

    #[test]
    fn jobs_get_the_app_powers_only_where_they_write() {
        let shares = with_apps();
        let roots = |op, sources: &[&str], dest| job_spec(&shares, &[], &job(op, sources, dest)).unwrap().app_roots;
        let immich = PathBuf::from("/srv/vol1/docker/stacks/immich");
        let db = PathBuf::from("/srv/ssd/docker/appdata/db");
        // into an app's folder, from a share: only the app folder is written
        assert_eq!(roots("copy", &["/Photos/a.jpg"], Some("/@apps/immich/library")), vec![immich.clone()]);
        // within the apps: a move removes its sources there too
        assert_eq!(roots("move", &["/@apps/db/x"], Some("/@apps/immich")), [db.clone(), immich.clone()]);
        assert_eq!(roots("delete", &["/@apps/db/x", "/Photos/b.jpg"], None), vec![db.clone()]);
        // out of the apps: written as the user, no powers
        assert!(roots("copy", &["/@apps/immich/a"], Some("/Photos")).is_empty());
        // a move across the boundary would copy, then fail to remove: refused, copy instead
        for (sources, dest) in [(&["/@apps/immich/a"][..], "/Photos"), (&["/Photos/b.jpg"][..], "/@apps/immich"), (&["/@apps/db/x", "/Photos/b.jpg"][..], "/@apps/immich")] {
            let e = job_spec(&shares, &[], &job("move", sources, Some(dest))).unwrap_err();
            assert_eq!(e.field.as_deref(), Some("dest"), "{sources:?} -> {dest}");
        }
        assert!(roots("delete", &["/Photos/b.jpg"], None).is_empty());
        // an app's folder itself is like a shared folder: never copied, moved or deleted
        let e = job_spec(&shares, &[], &job("delete", &["/@apps/immich"], None)).unwrap_err();
        assert_eq!(e.code, rynas_api_types::error_codes::SHARE_ROOT);
        // and deletions there are for good: no bin
        assert!(job_spec(&shares, &[], &job("delete", &["/@apps/immich/x"], None)).unwrap().recycle.is_empty());
    }

    #[test]
    fn jobs_name_the_shares_they_write_in() {
        let mut shares = with_apps();
        shares.push(Share { name: "Docs".into(), path: "/srv/vol2/Docs".into(), ..Default::default() });
        let roots = |op, sources: &[&str], dest| job_spec(&shares, &[], &job(op, sources, dest)).unwrap().write_roots;
        let (photos, docs) = (PathBuf::from("/srv/vol1/Photos"), PathBuf::from("/srv/vol2/Docs"));
        // a copy writes in its destination only, the sources are read
        assert_eq!(roots("copy", &["/Photos/a.jpg"], Some("/Docs")), vec![docs.clone()]);
        // a move also removes its sources, a deletion only removes
        assert_eq!(roots("move", &["/Photos/a.jpg"], Some("/Docs")), [photos.clone(), docs.clone()]);
        assert_eq!(roots("delete", &["/Photos/a.jpg", "/Photos/b.jpg"], None), vec![photos.clone()]);
        assert_eq!(roots("compress", &["/Photos/a.jpg"], Some("/Photos")), vec![photos.clone()]);
        // never an app's folder: writing there takes the apps' powers (app_roots)
        assert_eq!(roots("copy", &["/@apps/immich/a"], Some("/Photos")), vec![photos.clone()]);
        assert!(roots("copy", &["/Photos/a.jpg"], Some("/@apps/immich")).is_empty());
        assert!(roots("delete", &["/@apps/immich/x"], None).is_empty());
    }

    #[test]
    fn app_folders_are_listed_apart() {
        let t = tempfile::tempdir().unwrap();
        let photos = t.path().join("Photos");
        let stack = t.path().join("immich");
        std::fs::create_dir_all(&photos).unwrap();
        std::fs::create_dir_all(&stack).unwrap();
        let shares = vec![
            Share { name: "Photos".into(), path: photos, ..Default::default() },
            Share { name: format!("{}/immich", shares::APPS), path: stack, app: Some(shares::AppFolder { stack: "immich".into(), app: Some("immich".into()) }), ..Default::default() },
            Share { name: format!("{}/gone", shares::APPS), path: t.path().join("gone"), app: Some(shares::AppFolder { stack: "gone".into(), app: None }), ..Default::default() },
        ];
        let ctx = Context { shares: shares.clone(), ..Default::default() };
        let info = shares_info(&ctx);
        let names: Vec<&str> = info["shares"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Photos"]);
        assert_eq!(info["apps"][0], json!({ "name": "immich", "stack": "immich", "app": "immich" }));
        assert_eq!(info["apps"][1], json!({ "name": "gone", "stack": "gone" }));
        // no apps' folders (a user, or the setting off): no `apps` at all
        assert!(shares_info(&Context { shares: shares[..1].to_vec(), ..Default::default() }).get("apps").is_none());
        let list = group_listing(&shares, shares::APPS);
        assert_eq!(list["writable"], false);
        let entries = list["entries"].as_array().unwrap();
        // a folder that is not there (any more) is left out
        assert_eq!(entries.len(), 1);
        assert_eq!((entries[0]["name"].as_str(), entries[0]["kind"].as_str(), entries[0]["stack"].as_str()), (Some("immich"), Some("d"), Some("immich")));
    }

    struct NoRemote;
    impl Remote for NoRemote {
        fn serve(&self, _: &Request, _: &mut dyn Write, _: &[Conn]) -> Option<Result<(), ApiError>> {
            None
        }
        fn test(&self, _: &Request, _: &mut dyn Write, _: &[Conn]) -> Result<(), ApiError> {
            Err(ApiError::new(500, "no remote"))
        }
        fn usage(&self, _: &mut dyn Write, _: &[Conn], _: &[String]) -> Result<(), ApiError> {
            Err(ApiError::new(500, "no remote"))
        }
    }

    /// One route answered as the user, in a jail as the worker answers it: the JSON body, or
    /// the route's error.
    fn call(ctx: &Context, method: &str, route: &str, query: &[(&str, &str)]) -> Result<Value, ApiError> {
        call_with(ctx, method, route, query, Vec::new())
    }

    fn call_with(ctx: &Context, method: &str, route: &str, query: &[(&str, &str)], body: Vec<u8>) -> Result<Value, ApiError> {
        let query = query.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let req = Request::in_memory(method, route, query, body);
        let mut out = Vec::new();
        let jail = crate::confine::Jail::enter();
        let result = as_user(&req, &mut out, ctx, &NoRemote);
        drop(jail);
        result?;
        let text = String::from_utf8(out).unwrap();
        Ok(serde_json::from_str(text.split("\r\n\r\n").nth(1).unwrap()).unwrap())
    }

    /// A share "Docs", the backup of the PC "laptop" (a snapshot, rynas' staging folder, a
    /// link in the snapshot that leads to the staging folder), and a share "Data" that holds
    /// the backup folder of the PC "desk" (an install from before that was refused: rynasd
    /// gives it as read-only).
    fn with_backup(t: &Path) -> Context {
        let docs = t.join("Docs");
        let pc = t.join("pc-backups/laptop");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::create_dir_all(pc.join("@home.20261113T1543/marco")).unwrap();
        std::fs::write(pc.join("@home.20261113T1543/marco/notes.txt"), "hello").unwrap();
        std::fs::create_dir_all(pc.join(shares::BACKUP_STAGING).join("x1")).unwrap();
        std::fs::write(pc.join(shares::BACKUP_STAGING).join("x1/secret-notes.txt"), "half received").unwrap();
        std::os::unix::fs::symlink("../.incoming", pc.join("@home.20261113T1543/escape")).unwrap();
        std::os::unix::fs::symlink("../.incoming/x1/secret-notes.txt", pc.join("@home.20261113T1543/escape.txt")).unwrap();
        let data = t.join("Data");
        std::fs::create_dir_all(data.join("pc-backups/desk/@.20261113T1543")).unwrap();
        // what rynasd gives an administrator's worker: every PC's folder, guarded
        let pc_roots = Some([&pc, &data.join("pc-backups/desk")].into_iter().filter_map(|p| crate::confine::PcRoot::of(p)).collect());
        let shares = vec![
            Share { name: "Docs".into(), path: docs, ..Default::default() },
            Share { name: "Data".into(), path: data, read_only: true, holds_backup: Some("desk".into()), ..Default::default() },
            Share { name: format!("{}/laptop", shares::BACKUPS), path: pc, read_only: true, backup: Some("laptop".into()), ..Default::default() },
            Share { name: format!("{}/gone", shares::BACKUPS), path: t.join("pc-backups/gone"), read_only: true, backup: Some("gone".into()), ..Default::default() },
        ];
        Context { shares, pc_roots, ..Default::default() }
    }

    #[test]
    fn the_final_guard_follows_every_path_to_a_backup() {
        let t = tempfile::tempdir().unwrap();
        let mut ctx = with_backup(t.path());
        let pc = t.path().join("pc-backups/laptop");
        // a share whose folder is a link to the PC's folder, and a home that holds a PC's
        // folder: writable shares as far as their flags go, the guard says no
        std::os::unix::fs::symlink(&pc, t.path().join("lnk")).unwrap();
        let home = t.path().join("home");
        std::fs::create_dir_all(home.join("pcs/old/@.20261101T0900")).unwrap();
        std::fs::create_dir_all(home.join("notes")).unwrap();
        ctx.pc_roots.as_mut().unwrap().push(crate::confine::PcRoot::of(&home.join("pcs/old")).unwrap());
        ctx.shares.push(Share { name: "Lnk".into(), path: t.path().join("lnk"), ..Default::default() });
        ctx.shares.push(Share { name: "home".into(), path: home.clone(), home: true, ..Default::default() });
        for (route, query) in [
            ("mkdir", vec![("path", "/Lnk"), ("name", "new")]),
            ("mkdir", vec![("path", "/Lnk/@home.20261113T1543"), ("name", "a/b"), ("parents", "1")]),
            ("upload", vec![("dir", "/Lnk/@home.20261113T1543/marco"), ("name", "u.txt")]),
            ("edit/save", vec![("path", "/Lnk/@home.20261113T1543/marco/notes.txt")]),
            ("rename", vec![("path", "/Lnk/@home.20261113T1543"), ("name", "x")]),
            ("mkdir", vec![("path", "/home/pcs/old"), ("name", "new")]),
            ("mkdir", vec![("path", "/home/pcs"), ("name", "old/@x/y"), ("parents", "1")]),
            // a folder that holds a PC's folder is not renamed away either
            ("rename", vec![("path", "/home/pcs"), ("name", "x")]),
        ] {
            let e = call(&ctx, "POST", route, &query).unwrap_err();
            assert_eq!(e.code, rynas_api_types::error_codes::READ_ONLY, "{route} {query:?}: {}", e.message);
        }
        assert!(!pc.join("new").exists() && !home.join("pcs/old/new").exists() && !home.join("pcs/old/@x").exists() && home.join("pcs").is_dir());
        // beside them, the home is the home: parents made, renamed
        call(&ctx, "POST", "mkdir", &[("path", "/home"), ("name", "a/b"), ("parents", "1")]).unwrap();
        call(&ctx, "POST", "rename", &[("path", "/home/notes"), ("name", "notes2")]).unwrap();
        assert!(home.join("a/b").is_dir() && home.join("notes2").is_dir());
        // and the staging folder is not there through the link either
        assert_eq!(call(&ctx, "GET", "list", &[("path", "/Lnk/.incoming")]).unwrap_err().status, 404);
        let names = call(&ctx, "GET", "list", &[("path", "/Lnk")]).unwrap();
        assert!(names["entries"].as_array().unwrap().iter().all(|e| e["name"] != ".incoming"));
        // non-administrators (no backups in their shares): /@backups is not there
        let user = Context { shares: ctx.shares[..1].to_vec(), ..Default::default() };
        assert_eq!(call(&user, "GET", "list", &[("path", "/@backups")]).unwrap_err().status, 404);
        assert_eq!(call(&user, "GET", "stat", &[("path", "/@backups")]).unwrap_err().status, 404);
    }

    #[test]
    fn backups_are_listed_apart_and_read_only() {
        let t = tempfile::tempdir().unwrap();
        let ctx = with_backup(t.path());
        let info = shares_info(&ctx);
        let names: Vec<&str> = info["shares"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Docs", "Data"]);
        assert_eq!(info["shares"][1]["holds_backup"], "desk");
        assert_eq!(info["backups"], json!([{ "name": "laptop", "readonly": true }, { "name": "gone", "readonly": true }]));
        assert!(info.get("apps").is_none());
        assert!(shares_info(&Context { shares: ctx.shares[..1].to_vec(), ..Default::default() }).get("backups").is_none());
        // `/@backups`: one folder per PC that is there, never writable
        let root = call(&ctx, "GET", "list", &[("path", "/@backups")]).unwrap();
        assert_eq!((root["writable"].as_bool(), root["read_only"].as_bool(), root["backups"].as_bool()), (Some(false), Some(true), Some(true)));
        let entries = root["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!((entries[0]["name"].as_str(), entries[0]["readonly"].as_bool()), (Some("laptop"), Some(true)));
        assert_eq!(call(&ctx, "GET", "stat", &[("path", "/@backups")]).unwrap()["writable"], false);
        // a PC's folder: the snapshots, not the staging folder, nothing writable
        let pc = call(&ctx, "GET", "list", &[("path", "/@backups/laptop")]).unwrap();
        assert_eq!((pc["writable"].as_bool(), pc["read_only"].as_bool()), (Some(false), Some(true)));
        let names: Vec<&str> = pc["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["@home.20261113T1543"]);
        assert!(pc["entries"].as_array().unwrap().iter().all(|e| e["readonly"] == true));
        // deeper down too, and the details of an item
        let deep = call(&ctx, "GET", "list", &[("path", "/@backups/laptop/@home.20261113T1543/marco")]).unwrap();
        assert_eq!((deep["writable"].as_bool(), deep["entries"][0]["readonly"].as_bool()), (Some(false), Some(true)));
        assert_eq!(call(&ctx, "GET", "stat", &[("path", "/@backups/laptop/@home.20261113T1543/marco/notes.txt")]).unwrap()["writable"], false);
        // the editor opens it to read only
        assert_eq!(call(&ctx, "GET", "edit", &[("path", "/@backups/laptop/@home.20261113T1543/marco/notes.txt")]).unwrap()["writable"], false);
        // reading works: the preview's text
        assert_eq!(call(&ctx, "GET", "text", &[("path", "/@backups/laptop/@home.20261113T1543/marco/notes.txt")]).unwrap()["text"], "hello");
        // a normal share is untouched
        let docs = call(&ctx, "GET", "list", &[("path", "/Docs")]).unwrap();
        assert!(docs.get("read_only").is_none());
    }

    #[test]
    fn writing_routes_are_refused_in_a_backup() {
        let t = tempfile::tempdir().unwrap();
        let ctx = with_backup(t.path());
        let snap = t.path().join("pc-backups/laptop/@home.20261113T1543/marco");
        for (route, query) in [
            ("mkdir", vec![("path", "/@backups/laptop/@home.20261113T1543"), ("name", "new")]),
            ("mkdir", vec![("path", "/@backups/laptop"), ("name", "new")]),
            ("rename", vec![("path", "/@backups/laptop/@home.20261113T1543/marco/notes.txt"), ("name", "x.txt")]),
            ("edit/save", vec![("path", "/@backups/laptop/@home.20261113T1543/marco/notes.txt")]),
            ("upload", vec![("dir", "/@backups/laptop/@home.20261113T1543/marco"), ("name", "u.txt")]),
            ("upload/abort", vec![("dir", "/@backups/laptop/@home.20261113T1543/marco"), ("name", "u.txt")]),
            // a share that holds a PC's backup folder (an older install) is read-only too
            ("mkdir", vec![("path", "/Data/pc-backups/desk"), ("name", "new")]),
            ("rename", vec![("path", "/Data/pc-backups"), ("name", "x")]),
        ] {
            let e = call(&ctx, "POST", route, &query).unwrap_err();
            assert_eq!(e.code, rynas_api_types::error_codes::READ_ONLY, "{route}");
            // and rynasd's check before the worker says the same
            let e = refuse_read_only(&ctx.shares, route, |k| query.iter().find(|(q, _)| *q == k).map(|(_, v)| v.to_string())).unwrap_err();
            assert_eq!(e.code, rynas_api_types::error_codes::READ_ONLY, "{route}");
        }
        assert!(call(&ctx, "POST", "mkdir", &[("path", "/Data"), ("name", "x")]).unwrap_err().message.contains("desk"));
        assert!(!t.path().join("pc-backups/laptop/new").exists() && snap.join("notes.txt").exists() && !snap.join("x.txt").exists());
        assert!(t.path().join("Data/pc-backups").is_dir());
        // the same routes in a share, and reading routes in the backup, pass the check
        assert!(refuse_read_only(&ctx.shares, "mkdir", |k| (k == "path").then(|| "/Docs".to_string())).is_ok());
        assert!(refuse_read_only(&ctx.shares, "list", |k| (k == "path").then(|| "/@backups/laptop".to_string())).is_ok());
        call(&ctx, "POST", "mkdir", &[("path", "/Docs"), ("name", "new")]).unwrap();
        assert!(t.path().join("Docs/new").is_dir());
    }

    #[test]
    fn jobs_never_write_in_a_backup() {
        let t = tempfile::tempdir().unwrap();
        let ctx = with_backup(t.path());
        let file = "/@backups/laptop/@home.20261113T1543/marco/notes.txt";
        let read_only = |op, sources: &[&str], dest| {
            let e = job_spec(&ctx.shares, &[], &job(op, sources, dest)).unwrap_err();
            assert_eq!(e.code, rynas_api_types::error_codes::READ_ONLY, "{op} {sources:?} -> {dest:?}");
        };
        // nothing goes in
        read_only("copy", &["/Docs/a"], Some("/@backups/laptop/@home.20261113T1543"));
        read_only("move", &["/Docs/a"], Some("/@backups/laptop"));
        read_only("copy", &[file], Some("/@backups/laptop/@home.20261113T1543"));
        read_only("compress", &[file], Some("/@backups/laptop/@home.20261113T1543/marco"));
        read_only("extract", &["/Docs/a.zip"], Some("/@backups/laptop/@home.20261113T1543"));
        // nothing goes out by removal
        read_only("delete", &[file], None);
        read_only("move", &[file], Some("/Docs"));
        // the PCs' folder itself is no destination
        read_only("copy", &["/Docs/a"], Some("/@backups"));
        // nor a share that holds a PC's backup folder
        read_only("copy", &["/Docs/a"], Some("/Data/pc-backups/desk"));
        read_only("delete", &["/Data/pc-backups"], None);
        // copying out works, and writes only in the share
        let spec = job_spec(&ctx.shares, &[], &job("copy", &[file], Some("/Docs"))).unwrap();
        assert_eq!(spec.write_roots, vec![t.path().join("Docs")]);
        assert!(spec.app_roots.is_empty());
        // the copies stay as private as the originals
        assert_eq!(spec.private_roots, vec![t.path().join("pc-backups/laptop")]);
        // a copy between shares keeps the usual modes
        assert!(job_spec(&ctx.shares, &[], &job("copy", &["/Docs/a"], Some("/Docs/b"))).unwrap().private_roots.is_empty());
        // so do archives made from a backup into a share
        assert!(job_spec(&ctx.shares, &[], &job("compress", &[file], Some("/Docs"))).is_ok());
    }

    #[test]
    fn the_top_of_a_backup_and_its_staging_folder() {
        let t = tempfile::tempdir().unwrap();
        let ctx = with_backup(t.path());
        // the PC's folder itself: not writable, never measured, analysed or zipped whole
        assert_eq!(call(&ctx, "GET", "stat", &[("path", "/@backups/laptop")]).unwrap()["writable"], false);
        assert_eq!(call(&ctx, "GET", "space", &[("path", "/@backups/laptop")]).unwrap_err().status, 400);
        assert_eq!(call(&ctx, "GET", "download", &[("paths", r#"["/@backups/laptop"]"#)]).unwrap_err().status, 400);
        assert_eq!(call_with(&ctx, "POST", "usage", &[], br#"{"paths":["/@backups/laptop"]}"#.to_vec()).unwrap_err().status, 400);
        // a snapshot is measured: its file, never what a link leads to
        let usage = call_with(&ctx, "POST", "usage", &[], br#"{"paths":["/@backups/laptop/@home.20261113T1543"]}"#.to_vec()).unwrap();
        assert_eq!(usage["bytes"], 5 + "../.incoming".len() + "../.incoming/x1/secret-notes.txt".len());
        // the staging folder by its own path: not there
        for p in ["/@backups/laptop/.incoming", "/@backups/laptop/.incoming/x1"] {
            assert_eq!(call(&ctx, "GET", "list", &[("path", p)]).unwrap_err().status, 404, "{p}");
        }
        assert_eq!(call(&ctx, "GET", "text", &[("path", "/@backups/laptop/.incoming/x1/secret-notes.txt")]).unwrap_err().status, 404);
        // …nor through a link inside a snapshot: the real path of what was opened decides
        assert_eq!(call(&ctx, "GET", "list", &[("path", "/@backups/laptop/@home.20261113T1543/escape")]).unwrap_err().status, 404);
        assert_eq!(call(&ctx, "GET", "list", &[("path", "/@backups/laptop/@home.20261113T1543/escape/x1")]).unwrap_err().status, 404);
        assert_eq!(call(&ctx, "GET", "text", &[("path", "/@backups/laptop/@home.20261113T1543/escape.txt")]).unwrap_err().status, 404);
        assert_eq!(call(&ctx, "GET", "download", &[("path", "/@backups/laptop/@home.20261113T1543/escape.txt")]).unwrap_err().status, 404);
        // nor a search from the PC's folder: the snapshot's file only
        let found = call(&ctx, "GET", "search", &[("path", "/@backups/laptop"), ("q", "notes")]).unwrap();
        let hits = found["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(call(&ctx, "GET", "search", &[("path", "/@backups/laptop"), ("q", "incoming")]).unwrap()["hits"].as_array().unwrap().is_empty());
        // search hits in a backup are fixed like its entries
        assert_eq!(hits[0]["readonly"], true);
    }
}
