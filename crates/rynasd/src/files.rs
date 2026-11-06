//! The file manager (`/v1/files/...`, the routes of SynoFile under a new base).
//!
//! Every route that touches files runs in `rynas-files-worker`, started for the request
//! already switched to the user rynas-web names in `x-rynas-user` (groups, gid, uid,
//! `no_new_privs`, no capability but `CAP_DAC_READ_SEARCH` for administrators): the kernel
//! applies the user's permissions and POSIX ACLs, as for SMB. The worker gets the request
//! and the context (user, shares, volumes, the connections the request names) as one JSON
//! line, then the body, and answers in the gateway form, streamed back as it comes — an
//! upload never sits whole in rynasd's memory, nor does a download.
//!
//! rynasd answers itself what belongs to the user's stores and to the transfer jobs:
//! `whoami`, `prefs`, `conns`, `conns/save|delete|order|key`, `jobs`, `jobs/new`,
//! `jobs/control`, `eject`, `health`. Transfer jobs are rynas tasks (`files.copy`…), see
//! [`rynas_files::jobs::manager`]. A fork and exec per request costs about a millisecond
//! on the reference box (measured: see `docs/plugins/files.md`), so no worker is kept warm.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context as TaskCx, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use futures_util::{Stream, StreamExt};
use rynas_api_types::error_codes;
use rynas_api_types::SharesConfig;
use rynas_files::Context;
use rynas_files::identity::User;
use rynas_files::jobs::manager::{Config as JobsConfig, Control, Hooks, Info, JobTask, Manager, State as JobState};
use rynas_files::shares::Share;
use rynas_files::wire::{self, ApiError, WorkerRequest};
use rynas_services::SHARES_KEY;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdout};

use crate::ops::{Error, Ops, Res};

/// Largest body rynasd reads into memory for the routes it answers itself.
const MAX_OWN_BODY: usize = 4 << 20;
/// Largest response head a worker may write.
const MAX_HEAD: usize = 64 << 10;
/// How long a worker may take to start answering: the slowest reads (space analysis 40 s,
/// a search 60 s at most) fit. Uploads and saves also have to receive their body first.
const HEAD_DEADLINE: Duration = Duration::from_secs(120);
const UPLOAD_DEADLINE: Duration = Duration::from_secs(600);

pub struct Files {
    ops: Arc<Ops>,
    pub manager: Manager,
    worker: PathBuf,
    /// Root of the per-user thumbnail caches (`<root>/<uid>`).
    thumbs: PathBuf,
    /// Request workers running at once, all users (a grid of photos asks for dozens of
    /// thumbnails together; each worker counts against rynasd's cgroup limits). The
    /// others wait their turn.
    slots: Arc<tokio::sync::Semaphore>,
    /// Removable drives (see [`RemovableDrives`]).
    drives: Arc<dyn RemovableDrives>,
}

/// See [`Files::slots`].
const MAX_WORKERS: usize = 12;

impl Files {
    /// `state` = `/var/lib/rynas/files`, `cache` = `/var/cache/rynas/files`.
    pub fn new(ops: Arc<Ops>, worker: PathBuf, state: &Path, cache: &Path) -> Self {
        rynas_files::store::init(state);
        rynas_files::conns::set_ssh(Box::new(WorkerSsh { worker: worker.clone() }));
        let manager_cell: Arc<OnceLock<Manager>> = Arc::new(OnceLock::new());
        let hooks = Arc::new(TaskHooks {
            tasks: ops.tasks.clone(),
            handle: tokio::runtime::Handle::current(),
            manager: manager_cell.clone(),
        });
        let manager = Manager::new(JobsConfig { worker: worker.clone(), journal: state.join("journal"), hooks });
        let _ = manager_cell.set(manager.clone());
        Self { ops, manager, worker, thumbs: cache.join("thumbs"), slots: Arc::new(tokio::sync::Semaphore::new(MAX_WORKERS)), drives: Arc::new(NoDrives) }
    }

    /// Wires the removable-drives module (shares on removable drives, `eject`).
    pub fn with_drives(mut self, drives: Arc<dyn RemovableDrives>) -> Self {
        self.drives = drives;
        self
    }
}

type FilesState = Option<Arc<Files>>;

pub fn router(st: FilesState) -> Router {
    Router::new().route("/v1/files/{*route}", any(dispatch)).with_state(st)
}

impl From<ApiError> for Error {
    fn from(e: ApiError) -> Self {
        let status = StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut err = Error::new(status, e.code, e.message);
        if let Some(f) = e.field {
            err = err.field(f);
        }
        err
    }
}

fn unconfigured() -> Error {
    Error::new(StatusCode::SERVICE_UNAVAILABLE, error_codes::BACKEND_UNAVAILABLE, "the file manager is not enabled in this rynasd".into())
}

/// Runs `f` on a thread of its own (it may wait on a server or a worker for seconds: not on
/// rynasd's small blocking pool, guidelines S7).
async fn off_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Res<T> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("rynasd-files".into())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .map_err(Error::internal)?;
    rx.await.map_err(Error::internal)
}

/// Who asks, and what they may see: the user's identity (passwd/group), whether an
/// administrator, the shares with their real paths, the volumes' names.
struct Who {
    user: User,
    admin: bool,
}

fn who(files: &Files, name: &str) -> Res<Who> {
    let user = rynas_files::identity::lookup(name).map_err(|e| Error::new(StatusCode::FORBIDDEN, error_codes::FORBIDDEN, format!("{name}: {e}")))?;
    if user.uid == 0 {
        return Err(Error::new(StatusCode::FORBIDDEN, error_codes::FORBIDDEN, "the file manager does not run as root".into()));
    }
    let admin = files.ops.admins.is_admin(name);
    Ok(Who { user, admin })
}

/// The shares of the applied configuration (`shares` key) and the user's home, as the
/// worker sees them; the display names of their volumes.
fn shares_for(files: &Files, user: &User) -> Res<(Vec<Share>, BTreeMap<String, String>)> {
    let doc: SharesConfig = crate::services::from_value(files.ops.config.applied(SHARES_KEY)?)?;
    let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    let mut volumes = BTreeMap::new();
    let mut out: Vec<Share> = doc
        .shares
        .iter()
        .filter(|s| !s.path.is_empty())
        .map(|s| {
            let path = PathBuf::from(&s.path);
            let mount = mount_of(&mounts, &path).unwrap_or_default();
            let volume = mount.to_string_lossy().into_owned();
            if !volume.is_empty() {
                let name = s.subvolume.as_ref().map(|sv| sv.volume.clone()).unwrap_or_else(|| default_volume_name(&mount));
                volumes.entry(volume.clone()).or_insert(name);
            }
            let drive = files.drives.drive_of(&mount);
            Share {
                name: s.name.clone(),
                removable: drive.is_some(),
                device: drive.as_ref().map(|d| d.id.clone()),
                device_name: drive.map(|d| d.name),
                path,
                volume,
                comment: s.description.clone(),
                // the same bins the cleanup task empties (smb.recycle-cleanup)
                recycle: s.smb.enabled && s.smb.recycle.enabled,
                hidden: !s.smb.browseable,
                encrypted: false,
                home: false,
            }
        })
        .collect();
    // mounted removable volumes no share covers: one entry each, named after the mount point
    for v in files.drives.mounted() {
        if out.iter().any(|s| v.mount_point.starts_with(&s.path) || s.path.starts_with(&v.mount_point)) {
            continue;
        }
        let base = v.mount_point.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if base.is_empty() || base.starts_with('.') {
            continue;
        }
        let mut name = base.clone();
        let mut n = 2;
        while out.iter().any(|s| s.name.eq_ignore_ascii_case(&name)) {
            name = format!("{base}-{n}");
            n += 1;
        }
        let volume = v.mount_point.to_string_lossy().into_owned();
        volumes.entry(volume.clone()).or_insert_with(|| if v.label.is_empty() { base.clone() } else { v.label.clone() });
        out.push(Share {
            name,
            path: v.mount_point,
            volume,
            removable: true,
            device: Some(v.drive.id),
            device_name: Some(v.drive.name),
            ..Default::default()
        });
    }
    out.sort_by_key(|s| s.name.to_lowercase());
    if user.home.is_dir() && user.home != Path::new("/") && !out.iter().any(|s| s.name == rynas_files::shares::HOME) {
        let mount = mount_of(&mounts, &user.home).unwrap_or_default();
        let volume = mount.to_string_lossy().into_owned();
        if !volume.is_empty() {
            volumes.entry(volume.clone()).or_insert_with(|| default_volume_name(&mount));
        }
        out.insert(0, Share { name: rynas_files::shares::HOME.into(), path: user.home.clone(), volume, home: true, ..Default::default() });
    }
    Ok((out, volumes))
}

/// "/" for the system disk, else the mount point's last component ("/srv/vol1" → "vol1").
fn default_volume_name(mount: &Path) -> String {
    match mount.file_name() {
        Some(n) => n.to_string_lossy().into_owned(),
        None => "system".into(),
    }
}

/// The mount a path lives on: the longest mount point that holds it.
fn mount_of(mounts: &str, path: &Path) -> Option<PathBuf> {
    mounts
        .lines()
        .filter_map(|line| {
            let mut f = line.split(' ');
            let (_, point) = (f.next()?, f.next()?);
            let point = PathBuf::from(point.replace("\\040", " "));
            path.starts_with(&point).then_some(point)
        })
        .max_by_key(|p| p.as_os_str().len())
}

/// The seam to rynas's removable-drives module: which shares are on a removable drive, and
/// safe removal. The file manager does no detection nor unmounting of its own; until the
/// module is wired ([`Files::with_drives`]), [`NoDrives`] lists nothing and `eject` answers
/// 501 `not_available`.
pub trait RemovableDrives: Send + Sync {
    /// The removable drive holding `mount` (a mount point): its id (shares of one drive
    /// share it; what [`RemovableDrives::safely_remove`] takes) and its display name.
    fn drive_of(&self, mount: &Path) -> Option<Drive>;
    /// The mounted removable volumes: each is offered as a shared folder of its own (named
    /// after its mount point's last component) when no rynas share already covers it.
    fn mounted(&self) -> Vec<RemovableVolume> {
        Vec::new()
    }
    /// Starts the safe removal of the drive (every volume unmounted, the drive powered off)
    /// as the user `who`; refusals (busy, unknown drive) keep their status and code.
    fn safely_remove(&self, who: &str, drive: &str) -> Res<()>;
}

/// A removable drive, as the file manager names it.
pub struct Drive {
    pub id: String,
    pub name: String,
}

/// A mounted removable volume.
pub struct RemovableVolume {
    pub mount_point: PathBuf,
    /// Volume label, empty when none.
    pub label: String,
    pub drive: Drive,
}

/// No removable-drives module.
pub struct NoDrives;

impl RemovableDrives for NoDrives {
    fn drive_of(&self, _: &Path) -> Option<Drive> {
        None
    }
    fn safely_remove(&self, _: &str, _: &str) -> Res<()> {
        Err(Error::new(StatusCode::NOT_IMPLEMENTED, error_codes::NOT_AVAILABLE, "removable drives are not managed by this rynasd".into()))
    }
}

/// The full context of a worker request.
fn context(files: &Files, w: &Who, conns: bool) -> Res<Context> {
    let (shares, volumes) = shares_for(files, &w.user)?;
    let conns = if conns {
        rynas_files::conns::load(&w.user.name).into_iter().map(|c| rynas_files::conns::with_user_key(&w.user.name, c)).collect()
    } else {
        Vec::new()
    };
    Ok(Context {
        user: w.user.clone(),
        admin: w.admin,
        shares,
        volumes,
        hostname: hostname(),
        conns,
        extra: Value::Null,
        thumbs: thumbs_dir(&files.thumbs, &w.user),
    })
}

/// The user's thumbnail cache, made on first use: `<root>/<uid>`, owned by the user, 0700.
fn thumbs_dir(root: &Path, user: &User) -> Option<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let dir = root.join(user.uid.to_string());
    if let Ok(m) = std::fs::symlink_metadata(&dir) {
        return (m.is_dir() && (m.uid() == user.uid || !nix::unistd::Uid::effective().is_root())).then_some(dir);
    }
    std::fs::DirBuilder::new().recursive(true).mode(0o755).create(root).ok()?;
    std::fs::DirBuilder::new().mode(0o700).create(&dir).ok()?;
    if nix::unistd::Uid::effective().is_root() {
        std::os::unix::fs::lchown(&dir, Some(user.uid), Some(user.gid)).ok()?;
    }
    Some(dir)
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").map(|h| h.trim().to_string()).unwrap_or_default()
}

fn query_map(q: Option<&str>) -> HashMap<String, String> {
    q.map(wire::parse_query).unwrap_or_default()
}

/// Every `/v1/files/<route>`.
async fn dispatch(State(st): State<FilesState>, req: Request) -> Response {
    match handle(st, req).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn handle(st: FilesState, req: Request) -> Res<Response> {
    let files = st.ok_or_else(unconfigured)?;
    let (parts, body) = req.into_parts();
    let route = parts.uri.path().strip_prefix("/v1/files/").unwrap_or_default().trim_matches('/').to_string();
    let query = query_map(parts.uri.query());
    if route == "health" {
        return Ok(json_response(&json!({ "version": rynas_files::VERSION, "worker": files.worker.is_file() })));
    }
    let user = crate::ops::user(&parts.headers)?;
    let f = files.clone();
    let name = user.clone();
    let w = off_thread(move || who(&f, &name)).await??;
    let method = parts.method.as_str().to_string();
    match route.as_str() {
        "whoami" => Ok(json_response(&whoami(&w))),
        "prefs" => {
            if parts.method == Method::POST {
                let body = own_body(body).await?;
                let value: Value = serde_json::from_slice(&body).map_err(|e| ApiError::new(400, format!("invalid request: {e}")).code(error_codes::INVALID_JSON))?;
                let name = w.user.name.clone();
                off_thread(move || rynas_files::store::write_prefs(&name, &value)).await?.map_err(|e| {
                    if e.kind() == std::io::ErrorKind::InvalidInput { Error::from(ApiError::new(413, t_prefs())) } else { Error::internal(e) }
                })?;
                Ok(json_response(&json!({ "ok": true })))
            } else {
                let name = w.user.name.clone();
                Ok(json_response(&off_thread(move || rynas_files::store::read_prefs(&name)).await?))
            }
        }
        "conns" | "conns/save" | "conns/delete" | "conns/order" | "conns/key" => {
            let body = own_body(body).await?;
            let name = w.user.name.clone();
            let out = off_thread(move || {
                let r = wire::Request::in_memory(&method, &route, query, body.to_vec());
                let mut out = Vec::new();
                match rynas_files::conn_routes::conns(&r, &mut out, &name) {
                    Ok(()) => Ok(out),
                    Err(e) => Err(e),
                }
            })
            .await??;
            buffered_response(&out)
        }
        "jobs" => {
            let since = query.get("since").and_then(|s| s.parse().ok()).unwrap_or(0);
            let m = files.manager.clone();
            let name = w.user.name.clone();
            Ok(json_response(&off_thread(move || m.list(&name, since)).await?))
        }
        "jobs/new" => {
            need_post(&parts.method)?;
            let body = own_body(body).await?;
            let job: rynas_files::api::NewJob = serde_json::from_slice(&body).map_err(|e| ApiError::new(400, format!("invalid request: {e}")).code(error_codes::INVALID_JSON))?;
            let remote = job.sources.iter().chain(&job.dest).any(|p| rynas_files::conns::split(p).is_some());
            let f = files.clone();
            let answer = off_thread(move || -> Res<Value> {
                let ctx = context(&f, &w, remote)?;
                let spec = rynas_files::api::job_spec(&ctx.shares, &ctx.conns, &job)?;
                let info = Info { sources: job.sources.clone(), dest: job.dest.clone(), permanent: job.permanent };
                Ok(f.manager.submit(w.user, w.admin, spec, info)?)
            })
            .await??;
            Ok(json_response(&answer))
        }
        "jobs/control" => {
            need_post(&parts.method)?;
            let body = own_body(body).await?;
            let c: Control = serde_json::from_slice(&body).map_err(|e| ApiError::new(400, format!("invalid request: {e}")).code(error_codes::INVALID_JSON))?;
            let m = files.manager.clone();
            let name = w.user.name.clone();
            Ok(json_response(&off_thread(move || m.control(&name, c)).await??))
        }
        "eject" => {
            need_post(&parts.method)?;
            // `{ "share": name }` (the interface) or `?path=/share`
            let body = own_body(body).await?;
            let named = serde_json::from_slice::<Value>(&body).ok().and_then(|v| v["share"].as_str().map(|s| format!("/{s}")));
            let f = files.clone();
            let share = named.or_else(|| query.get("path").cloned()).unwrap_or_default();
            off_thread(move || eject(&f, &w, &share)).await??;
            Ok(json_response(&json!({ "ok": true })))
        }
        "conns/test" => {
            let body = own_body(body).await?;
            let f = files.clone();
            let (ctx, w) = off_thread(move || -> Res<(Context, Who)> {
                let r = wire::Request::in_memory(&method, "conns/test", HashMap::new(), body.to_vec());
                let conn = rynas_files::conn_routes::test_conn(&r, &w.user.name)?;
                let mut ctx = context(&f, &w, false)?;
                ctx.conns = vec![conn];
                Ok((ctx, w))
            })
            .await??;
            let _ = w;
            let wr = WorkerRequest { method: "POST".into(), route: "conns/test".into(), query: HashMap::new(), length: 0, range: None, ctx };
            run_worker(&files, wr, Body::empty(), HEAD_DEADLINE).await
        }
        _ => {
            let probe = wire::Request::in_memory(&method, &route, query.clone(), Vec::new());
            let wants = rynas_files::conn_routes::wants_conns(&probe);
            let f = files.clone();
            let init = route == "init";
            let ctx = off_thread(move || -> Res<Context> {
                let mut ctx = context(&f, &w, wants)?;
                if init {
                    ctx.extra = json!({
                        "whoami": whoami(&w),
                        "prefs": rynas_files::store::read_prefs(&w.user.name),
                        "conns": rynas_files::conn_routes::list(&w.user.name),
                    });
                }
                Ok(ctx)
            })
            .await??;
            let length = parts.headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
            let (body, length) = match length {
                Some(n) => (body, n),
                None if parts.method == Method::GET || parts.method == Method::HEAD => (Body::empty(), 0),
                // no length (chunked): refused, never buffered — reading it whole here, before
                // a worker slot, let any signed-in user pile up 80 MiB per request in rynasd
                // (browsers always send Content-Length with a fetch body)
                None => return Err(ApiError::new(411, "Content-Length required").code(error_codes::BAD_REQUEST).into()),
            };
            let range = parts.headers.get(header::RANGE).and_then(|v| v.to_str().ok()).map(String::from);
            let deadline = if route == "upload" || route == "edit/save" { UPLOAD_DEADLINE } else { HEAD_DEADLINE };
            let wr = WorkerRequest { method, route, query, length, range, ctx };
            run_worker(&files, wr, body, deadline).await
        }
    }
}

fn t_prefs() -> String {
    "preferences too large".into()
}

fn need_post(m: &Method) -> Res<()> {
    if *m == Method::POST { Ok(()) } else { Err(ApiError::new(405, "use POST").into()) }
}

fn whoami(w: &Who) -> Value {
    json!({ "user": w.user.name, "version": rynas_files::VERSION, "asset": null, "setup": true, "daemon": true, "admin": w.admin })
}

async fn own_body(body: Body) -> Res<Bytes> {
    axum::body::to_bytes(body, MAX_OWN_BODY).await.map_err(|_| ApiError::new(413, "request body too large").into())
}

fn json_response(v: &impl serde::Serialize) -> Response {
    let body = serde_json::to_vec(v).unwrap_or_default();
    let mut r = Response::new(Body::from(body));
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// A response written in the gateway form into memory (the routes rynasd answers itself).
fn buffered_response(out: &[u8]) -> Res<Response> {
    let end = out.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| Error::internal("malformed answer"))?;
    let (status, headers) = parse_head(&String::from_utf8_lossy(&out[..end]))?;
    let mut r = Response::new(Body::from(out[end + 4..].to_vec()));
    *r.status_mut() = status;
    *r.headers_mut() = headers;
    Ok(r)
}

/// `Status: 200 OK` and the header lines of a worker's answer.
fn parse_head(text: &str) -> Res<(StatusCode, HeaderMap)> {
    let mut status = StatusCode::OK;
    let mut headers = HeaderMap::new();
    for line in text.split("\r\n").filter(|l| !l.is_empty()) {
        let (k, v) = line.split_once(':').ok_or_else(|| Error::internal(format!("malformed worker header {line:?}")))?;
        let v = v.trim();
        if k.eq_ignore_ascii_case("status") {
            let code = v.split(' ').next().and_then(|c| c.parse::<u16>().ok()).ok_or_else(|| Error::internal("malformed worker status"))?;
            status = StatusCode::from_u16(code).map_err(Error::internal)?;
        } else if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(k.trim().as_bytes()), HeaderValue::from_str(v)) {
            headers.append(name, value);
        }
    }
    Ok((status, headers))
}

/// The body of a worker's answer: its stdout, and the worker itself, killed if the client
/// goes away before the end.
struct WorkerBody {
    stream: tokio_util::io::ReaderStream<BufReader<ChildStdout>>,
    child: Child,
    /// the worker's slot, given back when the answer ends
    _slot: tokio::sync::OwnedSemaphorePermit,
}

impl Stream for WorkerBody {
    type Item = std::io::Result<Bytes>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskCx<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        let next = this.stream.poll_next_unpin(cx);
        if let Poll::Ready(None) = next {
            // reap it (kill_on_drop covers the client going away)
            let _ = this.child.try_wait();
        }
        next
    }
}

/// Starts a worker as the request's user, sends it the request line and the body, and
/// streams its answer back.
async fn run_worker(files: &Files, wr: WorkerRequest, body: Body, deadline: Duration) -> Res<Response> {
    let slot = tokio::time::timeout(HEAD_DEADLINE, files.slots.clone().acquire_owned())
        .await
        .map_err(|_| Error::new(StatusCode::SERVICE_UNAVAILABLE, error_codes::BUSY, "the file manager is busy: try again".into()))?
        .map_err(Error::internal)?;
    let std_cmd = rynas_files::jobs::manager::worker_command(&files.worker, &wr.ctx.user, wr.ctx.admin).map_err(Error::internal)?;
    let mut cmd = tokio::process::Command::from(std_cmd);
    cmd.arg("serve").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::inherit()).kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| Error::internal(format!("{}: {e}", files.worker.display())))?;
    let mut stdin = child.stdin.take().ok_or_else(|| Error::internal("no worker stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| Error::internal("no worker stdout"))?;
    let mut line = serde_json::to_vec(&wr).map_err(Error::internal)?;
    line.push(b'\n');
    let length = wr.length;
    // the body goes on while the answer is awaited: a worker may answer before reading it
    // all (a refused chunk), and then reads the rest away itself
    tokio::spawn(async move {
        if stdin.write_all(&line).await.is_err() {
            return;
        }
        let mut sent = 0u64;
        let mut data = body.into_data_stream();
        while let Some(chunk) = data.next().await {
            let Ok(chunk) = chunk else { return };
            let room = length.saturating_sub(sent);
            let take = (chunk.len() as u64).min(room) as usize;
            if take == 0 || stdin.write_all(&chunk[..take]).await.is_err() {
                return;
            }
            sent += take as u64;
        }
        let _ = stdin.shutdown().await;
    });
    let mut reader = BufReader::new(stdout);
    let head = tokio::time::timeout(deadline, read_head(&mut reader)).await;
    let head = match head {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            // no answer at all: the worker refused to start (look at rynasd's log)
            let _ = child.start_kill();
            return Err(Error::internal(format!("file manager worker: {e}")));
        }
        Err(_) => {
            let _ = child.start_kill();
            return Err(Error::new(StatusCode::GATEWAY_TIMEOUT, error_codes::TIMEOUT, "the file manager worker did not answer in time".into()));
        }
    };
    let (status, headers) = parse_head(&head)?;
    let stream = WorkerBody { stream: tokio_util::io::ReaderStream::with_capacity(reader, 256 << 10), child, _slot: slot };
    let mut r = Response::new(Body::from_stream(stream));
    *r.status_mut() = status;
    *r.headers_mut() = headers;
    Ok(r)
}

async fn read_head(reader: &mut BufReader<ChildStdout>) -> std::io::Result<String> {
    let mut head = String::new();
    loop {
        let mut line = String::new();
        let n = (&mut *reader).take((MAX_HEAD - head.len()) as u64).read_line(&mut line).await?;
        if n == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the worker ended without an answer"));
        }
        if line == "\r\n" || line == "\n" {
            return Ok(head);
        }
        head.push_str(&line);
        if head.len() >= MAX_HEAD {
            return Err(std::io::Error::other("answer head too large"));
        }
    }
}

/// `eject {share}`: safe removal of the removable drive a share is on, through
/// [`RemovableDrives`]. Administrators only; refused while the user's transfers run.
fn eject(files: &Files, w: &Who, vpath: &str) -> Res<()> {
    if !w.admin {
        return Err(Error::new(StatusCode::FORBIDDEN, error_codes::FORBIDDEN, "eject: administrators only".into()));
    }
    let (shares, _) = shares_for(files, &w.user)?;
    let r = rynas_files::shares::resolve(&shares, vpath)?;
    let Some(drive) = r.share.device.clone() else {
        return Err(Error::new(StatusCode::NOT_IMPLEMENTED, error_codes::NOT_AVAILABLE, "eject: not on a removable drive rynas manages".into()));
    };
    if files.manager.busy(&w.user.name) {
        return Err(Error::new(StatusCode::CONFLICT, error_codes::BUSY, "transfers are still running".into()));
    }
    files.drives.safely_remove(&w.user.name, &drive)?;
    tracing::info!("{} ejected drive {drive} ({vpath})", w.user.name);
    Ok(())
}

/// SSH chores in a worker running as the user (rynasd links no SSH library).
struct WorkerSsh {
    worker: PathBuf,
}

impl WorkerSsh {
    fn call(&self, user: &str, request: Value) -> std::io::Result<Value> {
        use std::io::{BufRead, Write};
        let u = rynas_files::identity::lookup(user)?;
        let mut cmd = rynas_files::jobs::manager::worker_command(&self.worker, &u, false)?;
        cmd.arg("call").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::inherit());
        let mut child = cmd.spawn()?;
        let mut line = serde_json::to_vec(&request).map_err(std::io::Error::other)?;
        line.push(b'\n');
        child.stdin.take().ok_or_else(|| std::io::Error::other("no stdin"))?.write_all(&line)?;
        let mut answer = String::new();
        std::io::BufReader::new(child.stdout.take().ok_or_else(|| std::io::Error::other("no stdout"))?).read_line(&mut answer)?;
        let _ = child.wait();
        let v: Value = serde_json::from_str(&answer).map_err(std::io::Error::other)?;
        match v.get("error").and_then(Value::as_str) {
            Some(e) => Err(std::io::Error::other(e.to_string())),
            None => Ok(v),
        }
    }
}

impl rynas_files::conns::Ssh for WorkerSsh {
    fn server_key(&self, user: &str, conn: &rynas_files::conns::Conn) -> std::io::Result<String> {
        let v = self.call(user, json!({ "op": "server_key", "conn": conn }))?;
        Ok(v["key"].as_str().unwrap_or_default().to_string())
    }
    fn new_key(&self, user: &str, comment: &str) -> std::io::Result<(String, String)> {
        let v = self.call(user, json!({ "op": "new_key", "comment": comment }))?;
        Ok((v["private"].as_str().unwrap_or_default().into(), v["public"].as_str().unwrap_or_default().into()))
    }
    fn import_key(&self, user: &str, text: &str, passphrase: Option<&str>) -> Result<(String, String), String> {
        let v = self.call(user, json!({ "op": "import_key", "text": text, "passphrase": passphrase })).map_err(|e| e.to_string())?;
        Ok((v["private"].as_str().unwrap_or_default().into(), v["info"].as_str().unwrap_or_default().into()))
    }
}

/// Transfer jobs as rynas tasks.
struct TaskHooks {
    tasks: rynas_tasks::TaskManager,
    handle: tokio::runtime::Handle,
    manager: Arc<OnceLock<Manager>>,
}

enum Msg {
    Progress(u8),
    Log(String),
}

struct Task {
    id: String,
    tx: tokio::sync::mpsc::Sender<Msg>,
    done: Option<tokio::sync::oneshot::Sender<(JobState, String)>>,
}

impl JobTask for Task {
    fn id(&self) -> String {
        self.id.clone()
    }
    fn progress(&self, pct: u8) {
        // a full channel only skips a step of the bar
        let _ = self.tx.try_send(Msg::Progress(pct));
    }
    fn log(&self, line: &str) {
        let _ = self.tx.try_send(Msg::Log(line.to_string()));
    }
    fn finished(mut self: Box<Self>, state: JobState, msg: &str) {
        if let Some(done) = self.done.take() {
            let _ = done.send((state, msg.to_string()));
        }
    }
}

impl Hooks for TaskHooks {
    fn job_started(&self, kind: &str, owner: &str) -> Option<Box<dyn JobTask>> {
        // bounded (M2): progress and log lines past it are dropped, never the end
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Msg>(256);
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<(JobState, String)>();
        let manager = self.manager.clone();
        let tasks = self.tasks.clone();
        let _guard = self.handle.enter();
        let id = self
            .tasks
            .spawn(kind, owner, move |ctx| async move {
                let mut asked = false;
                loop {
                    tokio::select! {
                        m = rx.recv() => match m {
                            Some(Msg::Progress(p)) => ctx.progress(p),
                            Some(Msg::Log(l)) => ctx.log(&l),
                            None => {}
                        },
                        d = &mut done_rx => {
                            while let Ok(m) = rx.try_recv() {
                                if let Msg::Log(l) = m { ctx.log(&l) }
                            }
                            let (state, msg) = d.unwrap_or((JobState::Failed, "the job went away".into()));
                            return match state {
                                JobState::Done => Ok(()),
                                JobState::Cancelled => {
                                    // cancelled from the transfer dialog: the task says so too
                                    let _ = tasks.cancel(ctx.id());
                                    Err("cancelled".into())
                                }
                                _ => Err(msg),
                            };
                        }
                        _ = ctx.token().cancelled(), if !asked => {
                            asked = true;
                            // cancelled from the task panel: the job too
                            if let Some(m) = manager.get().cloned() {
                                let id = ctx.id().to_string();
                                std::thread::spawn(move || m.cancel_task(&id));
                            }
                        }
                    }
                }
            })
            .ok()?;
        Some(Box::new(Task { id, tx, done: Some(done_tx) }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_heads_become_responses() {
        let (s, h) = parse_head("Status: 206 Partial Content\r\nContent-Type: video/mp4\r\nContent-Range: bytes 0-1/10").unwrap();
        assert_eq!(s, StatusCode::PARTIAL_CONTENT);
        assert_eq!(h[header::CONTENT_TYPE], "video/mp4");
        assert_eq!(h[header::CONTENT_RANGE], "bytes 0-1/10");
        assert!(parse_head("garbage").is_err());
        let r = buffered_response(b"Status: 409 Conflict\r\nContent-Type: application/json\r\n\r\n{\"code\":\"already_exists\"}").unwrap();
        assert_eq!(r.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn mounts_and_volumes() {
        let mounts = "/dev/md2 / btrfs rw 0 0\n/dev/mapper/vol1 /srv/vol1 btrfs rw 0 0\n/dev/sdc1 /media/usb\\040disk vfat rw 0 0\n";
        assert_eq!(mount_of(mounts, Path::new("/srv/vol1/Documents")), Some(PathBuf::from("/srv/vol1")));
        assert_eq!(mount_of(mounts, Path::new("/home/alice")), Some(PathBuf::from("/")));
        assert_eq!(mount_of(mounts, Path::new("/media/usb disk/x")), Some(PathBuf::from("/media/usb disk")));
        assert_eq!(default_volume_name(Path::new("/srv/vol1")), "vol1");
        assert_eq!(default_volume_name(Path::new("/")), "system");
        assert!(NoDrives.drive_of(Path::new("/media/usb disk")).is_none());
    }

    #[test]
    fn job_states_finish_tasks() {
        // the manager's states that end a job are the ones a task ends on
        assert!(JobState::Done.finished() && JobState::Cancelled.finished() && JobState::Failed.finished());
        assert!(!JobState::Paused.finished());
    }
}
