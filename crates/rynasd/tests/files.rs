//! The file manager through rynasd's internal API, with the real worker binary: the worker
//! runs as the user running the tests (a rynasd that is not root cannot switch; the switch
//! itself is tested as root in `packaging/test-files.sh`).
//!
//! The worker is its own crate with its own lock file; the test builds it once into the
//! target directory (or takes `RYNAS_FILES_WORKER`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rynas_api_types::error_codes;
use rynas_auth::ManualClock;
use rynas_auth::backend::{AuthError, Authenticator, BoxFuture, Identity};
use rynas_auth::totp::{MemoryStorage, TotpManager};
use rynas_config::{Config, ConfigOptions, MemorySystem};
use rynas_services::{MemoryAccounts, MemoryShares, SHARES_KEY, SharesDomain, UsersDomain};
use rynas_tasks::{TaskConfig, TaskManager};
use rynasd::files::{Drive, Files, RemovableDrives, RemovableVolume};
use rynasd::ops::Ops;
use rynasd::roles::AdminPolicy;
use rynasd::server::{DaemonState, router};
use serde_json::{Value, json};
use tower::ServiceExt;

struct NoAuth;

impl Authenticator for NoAuth {
    fn realm(&self) -> &str {
        "none"
    }
    fn authenticate<'a>(&'a self, _: &'a str, _: &'a str) -> BoxFuture<'a, Result<Identity, AuthError>> {
        Box::pin(async { Err(AuthError::InvalidCredentials) })
    }
}

/// The worker binary: built once per test run into a target directory of its own (the
/// outer cargo may hold the lock of ours).
fn worker() -> PathBuf {
    static W: OnceLock<PathBuf> = OnceLock::new();
    W.get_or_init(|| {
        if let Ok(p) = std::env::var("RYNAS_FILES_WORKER") {
            return PathBuf::from(p);
        }
        let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("files-worker");
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../rynas-files-worker/Cargo.toml");
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "--quiet", "--manifest-path"])
            .arg(&manifest)
            .env("CARGO_TARGET_DIR", &target)
            .status()
            .expect("cargo runs");
        assert!(status.success(), "building rynas-files-worker failed");
        target.join("debug/rynas-files-worker")
    })
    .clone()
}

fn me() -> String {
    rynas_files::identity::current().expect("the test user has a passwd entry").name
}

async fn call(app: &axum::Router, method: &str, uri: &str, body: Body) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
    let len = to_bytes_len(&body);
    let mut req = Request::builder().method(method).uri(uri).header(rynas_auth::ipc::USER_HEADER, me());
    if let Some(n) = len {
        req = req.header(header::CONTENT_LENGTH, n);
    }
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), 64 << 20).await.unwrap().to_vec();
    (status, bytes, headers)
}

fn to_bytes_len(body: &Body) -> Option<u64> {
    use axum::body::HttpBody;
    body.size_hint().exact()
}

async fn json_call(app: &axum::Router, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    let (s, b, _) = call(app, method, uri, Body::from(serde_json::to_vec(&body).unwrap())).await;
    (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let (s, b, _) = call(app, "GET", uri, Body::empty()).await;
    (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

/// rynasd with the file manager and `shares` applied (each `[name, path]`), the test user an
/// administrator.
fn app_with(dir: &Path, list: &[(&str, &Path)]) -> (axum::Router, Arc<Ops>) {
    app_full(dir, list, true, None)
}

/// [`app_with`], the test user an administrator or not, with a removable-drives module.
fn app_full(dir: &Path, list: &[(&str, &Path)], admin: bool, drives: Option<Arc<dyn RemovableDrives>>) -> (axum::Router, Arc<Ops>) {
    let worker = worker();
    let root = dir.join("root");
    std::fs::create_dir_all(&root).unwrap();
    let sys = Arc::new(MemorySystem::new("nas"));
    let acc = Arc::new(MemoryAccounts::new());
    let host = Arc::new(MemoryShares::new());
    for (_, path) in list {
        host.add_dir(path.to_str().unwrap(), None);
    }
    let config = Arc::new(
        Config::open(
            ConfigOptions::new(dir.join("state"), &root),
            sys,
            vec![Arc::new(UsersDomain::new(acc.clone())), Arc::new(SharesDomain::new(host, acc))],
        )
        .unwrap(),
    );
    let shares: Vec<Value> = list.iter().map(|(name, path)| json!({ "name": name, "path": path, "description": "Documents" })).collect();
    config.apply_value("test", SHARES_KEY, Some(json!({ "shares": shares })), "shares", &|_| {}).unwrap();
    let tasks = TaskManager::new(dir.join("tasks"), "nas", TaskConfig::default()).unwrap();
    let mut ops = Ops::new(config, tasks);
    ops.admins = if admin {
        AdminPolicy::new(vec!["rynas-admin".into()], |_| Some(vec!["rynas-admin".into()]))
    } else {
        AdminPolicy::new(vec!["rynas-admin".into()], |_| Some(Vec::new()))
    };
    let ops = Arc::new(ops);
    let mut files = Files::new(ops.clone(), worker, &dir.join("state/files"), &dir.join("cache"));
    if let Some(d) = drives {
        files = files.with_drives(d);
    }
    let files = Arc::new(files);
    let clock = Arc::new(ManualClock::new(1_800_000_000_000));
    let totp = Arc::new(TotpManager::new(MemoryStorage::default(), clock.clone(), "rynas"));
    let mut st = DaemonState::new(Arc::new(NoAuth), totp, clock);
    st.ops = Some(ops.clone());
    st.files = Some(files);
    (router(st), ops)
}

#[tokio::test]
async fn the_file_manager_end_to_end() {
    // not below /tmp: shares there are refused
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let docs = dir.path().join("docs");
    std::fs::create_dir_all(docs.join("old")).unwrap();
    std::fs::write(docs.join("old/notes.txt"), "line one\r\nline two\r\n").unwrap();
    let (app, ops) = app_with(dir.path(), &[("docs", &docs)]);

    // init: rynasd's part and the shares as the user sees them
    let (s, v) = get(&app, "/v1/files/init").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["whoami"]["user"], me());
    assert_eq!(v["whoami"]["admin"], true);
    let share = v["shares"]["shares"].as_array().unwrap().iter().find(|s| s["name"] == "docs").cloned().unwrap();
    assert_eq!(share["comment"], "Documents");
    assert_eq!(share["recycle"], true);
    assert_eq!(share["writable"], true);
    assert!(share.get("path").is_none(), "real paths never reach the interface: {share}");

    // list, and no escaping the share
    let (s, v) = get(&app, "/v1/files/list?path=/docs").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["entries"][0]["name"], "old");
    let (s, v) = get(&app, "/v1/files/list?path=/docs/../root").await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::BAD_REQUEST, Some(error_codes::INVALID_VALUE)));
    let (s, v) = get(&app, "/v1/files/list?path=/nope").await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::NOT_FOUND, Some(error_codes::NOT_FOUND)));

    // a body without Content-Length (chunked) is refused, never buffered in rynasd
    let chunks = futures_util::stream::iter([Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"abc"))]);
    let (s, b, _) = call(&app, "POST", "/v1/files/upload?path=/docs&name=x.txt", Body::from_stream(chunks)).await;
    assert_eq!(s, StatusCode::LENGTH_REQUIRED, "{}", String::from_utf8_lossy(&b));

    // mkdir, a name taken, a bad name
    let (s, v) = json_call(&app, "POST", "/v1/files/mkdir?path=/docs&name=new", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = json_call(&app, "POST", "/v1/files/mkdir?path=/docs&name=new", json!({})).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::CONFLICT, Some(error_codes::ALREADY_EXISTS)));
    let (s, v) = json_call(&app, "POST", "/v1/files/mkdir?path=/docs&name=..", json!({})).await;
    assert_eq!((s, v["field"].as_str()), (StatusCode::BAD_REQUEST, Some("name")));

    // an upload in two chunks, then the download with a range
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let (a, b) = data.split_at(200_000);
    let (s, v1, _) = call(&app, "POST", "/v1/files/upload?dir=/docs/new&name=blob.bin&id=abcdefgh12&offset=0&total=300000", Body::from(a.to_vec())).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&v1));
    let (s, v2, _) = call(&app, "POST", "/v1/files/upload?dir=/docs/new&name=blob.bin&id=abcdefgh12&offset=200000&total=300000", Body::from(b.to_vec())).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&v2));
    assert_eq!(std::fs::read(docs.join("new/blob.bin")).unwrap(), data);
    assert!(std::fs::read_dir(docs.join("new")).unwrap().count() == 1, "no .rynas-upload left");
    let (s, got, h) = call(&app, "GET", "/v1/files/download?path=/docs/new/blob.bin", Body::empty()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got, data);
    assert!(h[header::CONTENT_DISPOSITION].to_str().unwrap().contains("blob.bin"));
    let req = Request::builder()
        .uri("/v1/files/view?path=/docs/new/blob.bin")
        .header(rynas_auth::ipc::USER_HEADER, me())
        .header(header::RANGE, "bytes=10-19")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(to_bytes(resp.into_body(), 1 << 20).await.unwrap().to_vec(), data[10..20].to_vec());
    // a folder as a zip stream
    let (s, zip, h) = call(&app, "GET", "/v1/files/download?paths=%5B%22%2Fdocs%2Fold%22%5D", Body::empty()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h[header::CONTENT_TYPE], "application/zip");
    assert!(zip.starts_with(b"PK"));

    // the editor saves the text as the interface keeps it (line endings included) and
    // refuses a stale stamp
    let (s, v) = get(&app, "/v1/files/edit?path=/docs/old/notes.txt").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let stamp = v["stamp"].as_str().unwrap().to_string();
    let uri = format!("/v1/files/edit/save?path=/docs/old/notes.txt&stamp={stamp}");
    let (s, b, _) = call(&app, "POST", &uri, Body::from("line one\r\nline 2\r\n")).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    assert_eq!(std::fs::read_to_string(docs.join("old/notes.txt")).unwrap(), "line one\r\nline 2\r\n");
    let (s, b, _) = call(&app, "POST", &uri, Body::from("again\n")).await;
    let v: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!((s, v["code"].as_str()), (StatusCode::CONFLICT, Some(error_codes::CONFLICT)), "{v}");

    // search
    let (s, v) = get(&app, "/v1/files/search?path=/docs&q=notes").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["hits"][0]["name"], "notes.txt");

    // preferences
    let (s, _) = json_call(&app, "POST", "/v1/files/prefs", json!({ "view": "grid" })).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(get(&app, "/v1/files/prefs").await.1["view"], "grid");

    // a deletion into the recycle bin, as a job and a rynas task
    let (s, v) = json_call(&app, "POST", "/v1/files/jobs/new", json!({ "op": "delete", "sources": ["/docs/old/notes.txt"] })).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let deadline = Instant::now() + Duration::from_secs(20);
    let job = loop {
        let (_, v) = get(&app, "/v1/files/jobs").await;
        let job = v["jobs"][0].clone();
        if job["state"] == "done" || Instant::now() > deadline {
            break job;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(job["state"], "done", "{job}");
    assert!(docs.join("#recycle/old/notes.txt").is_file());
    assert!(!docs.join("old/notes.txt").exists());
    let task_id = job["task_id"].as_str().unwrap().to_string();
    let task = ops.tasks.wait(&task_id).await.unwrap();
    assert_eq!(task.kind, "files.delete");
    assert_eq!(task.status, rynas_api_types::TaskStatus::Ok);
    // a share root is not a job's source
    let (s, v) = json_call(&app, "POST", "/v1/files/jobs/new", json!({ "op": "delete", "sources": ["/docs"] })).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::BAD_REQUEST, Some(error_codes::SHARE_ROOT)));

    // without the user header nothing happens
    let resp = app.clone().oneshot(Request::builder().uri("/v1/files/list?path=/docs").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Samba's `wide links = no`: links are followed inside their share, never out of it (to the
/// system, to another share), whatever the route.
#[tokio::test]
async fn symbolic_links_stay_inside_their_share() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let (docs, other, sys) = (base.join("docs"), base.join("other"), base.join("sys"));
    for d in [docs.join("real/sub"), other.clone(), sys.join("ssl")] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(sys.join("shadow"), "root:secret").unwrap();
    std::fs::write(other.join("o.txt"), "other share").unwrap();
    std::fs::write(docs.join("real/note.txt"), "inside").unwrap();
    let link = |target: &Path, name: &str| std::os::unix::fs::symlink(target, docs.join(name)).unwrap();
    link(&sys, "etc");
    link(Path::new("../sys/shadow"), "up");
    link(Path::new("real/../../sys"), "dots");
    link(&other, "to-other");
    link(Path::new("etc"), "chain");
    link(Path::new("real"), "inside");
    link(&docs.join("real/note.txt"), "abs-inside.txt");
    let (app, _) = app_with(dir.path(), &[("docs", &docs), ("other", &other)]);
    let refused = |(s, v): (StatusCode, Value)| {
        assert_eq!((s, v["code"].as_str()), (StatusCode::FORBIDDEN, Some(error_codes::OUTSIDE_SHARE)), "{v}");
    };

    // listed as links, the ones out of the share marked and never looked into
    let (s, v) = get(&app, "/v1/files/list?path=/docs").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let entry = |name: &str| v["entries"].as_array().unwrap().iter().find(|e| e["name"] == name).cloned().unwrap();
    for name in ["etc", "up", "dots", "to-other", "chain"] {
        assert_eq!((entry(name)["kind"].as_str(), entry(name)["outside"].as_bool()), (Some("l"), Some(true)), "{name}");
    }
    assert_eq!(entry("inside")["link_dir"], true);
    assert!(entry("inside").get("outside").is_none());
    let (s, v) = get(&app, "/v1/files/stat?path=/docs/etc").await;
    assert_eq!((s, v["outside"].as_bool(), v["readable"].as_bool()), (StatusCode::OK, Some(true), Some(false)), "{v}");

    // followed inside the share
    let (s, v) = get(&app, "/v1/files/list?path=/docs/inside").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, got, _) = call(&app, "GET", "/v1/files/view?path=/docs/abs-inside.txt", Body::empty()).await;
    assert_eq!((s, got.as_slice()), (StatusCode::OK, b"inside".as_slice()));

    // refused out of it, whatever the route
    for uri in [
        "/v1/files/list?path=/docs/etc",
        "/v1/files/list?path=/docs/etc/ssl",
        "/v1/files/list?path=/docs/to-other",
        "/v1/files/list?path=/docs/chain",
        "/v1/files/stat?path=/docs/etc/shadow",
        "/v1/files/view?path=/docs/up",
        "/v1/files/view?path=/docs/dots/shadow",
        "/v1/files/download?path=/docs/etc/shadow",
        "/v1/files/download?paths=%5B%22%2Fdocs%2Fup%22%5D",
        "/v1/files/text?path=/docs/up",
        "/v1/files/edit?path=/docs/up",
        "/v1/files/media?path=/docs/up",
        "/v1/files/space?path=/docs/etc",
        "/v1/files/search?path=/docs/etc&q=shadow",
    ] {
        refused(get(&app, uri).await);
    }
    let (s, _, _) = call(&app, "GET", "/v1/files/thumb?path=/docs/etc/shadow", Body::empty()).await;
    assert_ne!(s, StatusCode::OK);
    refused(json_call(&app, "POST", "/v1/files/usage", json!({ "paths": ["/docs/etc/ssl"] })).await);
    refused(json_call(&app, "POST", "/v1/files/mkdir?path=/docs/etc&name=x", json!({})).await);
    refused(json_call(&app, "POST", "/v1/files/mkdir?path=/docs&name=etc/x&parents=1", json!({})).await);
    let (s, b, _) = call(&app, "POST", "/v1/files/upload?dir=/docs/etc&name=x.txt", Body::from("x")).await;
    refused((s, serde_json::from_slice(&b).unwrap_or(Value::Null)));
    let (s, b, _) = call(&app, "POST", "/v1/files/edit/save?path=/docs/up&stamp=x&force=1", Body::from("pwned")).await;
    refused((s, serde_json::from_slice(&b).unwrap_or(Value::Null)));
    assert_eq!(std::fs::read_dir(&sys).unwrap().count(), 2, "nothing made out of the share");
    assert_eq!(std::fs::read_to_string(sys.join("shadow")).unwrap(), "root:secret");

    // a folder with such a link downloads it as a link, not what it points to
    let (s, zip, _) = call(&app, "GET", "/v1/files/download?paths=%5B%22%2Fdocs%22%5D", Body::empty()).await;
    assert_eq!(s, StatusCode::OK);
    assert!(!zip.windows(11).any(|w| w == b"root:secret"));
    let (s, tar, _) = call(&app, "GET", "/v1/files/download?paths=%5B%22%2Fdocs%22%5D&format=tar", Body::empty()).await;
    assert_eq!(s, StatusCode::OK);
    assert!(!tar.windows(11).any(|w| w == b"root:secret"));
    // the search does not go down links, the usage counts them as items
    let (s, v) = get(&app, "/v1/files/search?path=/docs&q=shadow").await;
    assert_eq!((s, v["hits"].as_array().map(Vec::len)), (StatusCode::OK, Some(0)), "{v}");
    let (s, v) = json_call(&app, "POST", "/v1/files/usage", json!({ "paths": ["/docs"] })).await;
    // docs, real, real/sub; note.txt and the seven links (never what they point to)
    assert_eq!((s, v["dirs"].as_u64(), v["files"].as_u64()), (StatusCode::OK, Some(3), Some(8)), "{v}");
    let (s, v) = get(&app, "/v1/files/space?path=/docs").await;
    assert_eq!(s, StatusCode::OK, "{v}");

    // the editor saves the target of a link inside the share; the link stays
    let (s, v) = get(&app, "/v1/files/edit?path=/docs/abs-inside.txt").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let uri = format!("/v1/files/edit/save?path=/docs/abs-inside.txt&stamp={}", v["stamp"].as_str().unwrap());
    let (s, b, _) = call(&app, "POST", &uri, Body::from("saved")).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    assert_eq!(std::fs::read_to_string(docs.join("real/note.txt")).unwrap(), "saved");
    assert!(std::fs::symlink_metadata(docs.join("abs-inside.txt")).unwrap().file_type().is_symlink());

    // the link itself can be renamed (it is an item of the share)
    let (s, v) = json_call(&app, "POST", "/v1/files/rename?path=/docs/etc&name=etc2", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(std::fs::read_link(docs.join("etc2")).unwrap(), sys);

    // a job through a link: the source is refused, nothing copied
    let (s, v) = json_call(&app, "POST", "/v1/files/jobs/new", json!({ "op": "copy", "sources": ["/docs/etc2/shadow"], "dest": "/docs/real" })).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let deadline = Instant::now() + Duration::from_secs(20);
    let job = loop {
        let (_, list) = get(&app, "/v1/files/jobs").await;
        let job = list["jobs"][0].clone();
        if job["state"] == "done" || job["state"] == "failed" || Instant::now() > deadline {
            break job;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(job["errors"][0]["code"], error_codes::OUTSIDE_SHARE, "{job}");
    assert!(!docs.join("real/shadow").exists());
}

/// One mounted removable volume; its access group is `gid`.
struct FakeDrives {
    mount: PathBuf,
    gid: Option<u32>,
}

impl RemovableDrives for FakeDrives {
    fn drive_of(&self, _: &Path) -> Option<Drive> {
        None
    }
    fn mounted(&self) -> Vec<RemovableVolume> {
        vec![RemovableVolume { mount_point: self.mount.clone(), label: "STICK".into(), drive: Drive { id: "sdc".into(), name: "SanDisk".into() } }]
    }
    fn access_gid(&self) -> Option<u32> {
        self.gid
    }
    fn safely_remove(&self, _: &str, _: &str) -> Result<(), rynasd::ops::Error> {
        Ok(())
    }
}

/// Removable volumes: administrators and the members of the access group, as for SMB.
#[tokio::test]
async fn removable_volumes_for_admins_and_the_access_group() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let docs = dir.path().join("docs");
    let stick = dir.path().join("media/STICK");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::create_dir_all(&stick).unwrap();
    let my_gid = rynas_files::identity::current().unwrap().gid;
    let names = |v: &Value| -> Vec<String> { v["shares"]["shares"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap().to_string()).collect() };
    for (admin, gid, sees) in [(true, None, true), (false, None, false), (false, Some(my_gid.wrapping_add(12345)), false), (false, Some(my_gid), true)] {
        let drives: Arc<dyn RemovableDrives> = Arc::new(FakeDrives { mount: stick.clone(), gid });
        let (app, _) = app_full(dir.path(), &[("docs", &docs)], admin, Some(drives));
        let (s, v) = get(&app, "/v1/files/init").await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(names(&v).contains(&"STICK".to_string()), sees, "admin {admin}, group {gid:?}: {v}");
        let (s, _) = get(&app, "/v1/files/list?path=/STICK").await;
        assert_eq!(s == StatusCode::OK, sees, "admin {admin}, group {gid:?}");
    }
}

/// No program runs (the Docker service never reaches a tool in these tests).
struct NoRun;

impl rynas_docker::Runner for NoRun {
    fn run(&self, _: &rynas_tasks::Cmd) -> Result<rynas_tasks::CmdOutput, String> {
        Err("cannot start: No such file or directory".into())
    }
}

/// A Docker service on the fake root `root` (stacks on `vol1`), with the setting
/// `files_app_data` as given; the sender changes the settings later.
fn docker_for(ops: &Arc<Ops>, dir: &Path, root: &Path, on: bool) -> tokio::sync::watch::Sender<rynas_api_types::docker::DockerSettings> {
    let (domain, settings) = rynasd::docker::DockerDomain::new(true);
    domain.tx.send_replace(rynas_api_types::docker::DockerSettings { volume: Some("vol1".into()), files_app_data: on, ..Default::default() });
    let svc = rynasd::docker::DockerSvc::with_host(
        ops.clone(),
        Arc::new(rynas_notify::MemoryNotifier::default()),
        root.to_path_buf(),
        true,
        Some(dir.join("no-docker.sock").to_string_lossy().into_owned()),
        dir.join("state/docker"),
        settings,
        dir.join("host"),
        Arc::new(NoRun),
    );
    let _ = ops.docker.set(svc);
    domain.tx
}

#[tokio::test]
async fn app_data_for_administrators_when_the_setting_is_on() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let docs = dir.path().join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    // a fake root: an app stack, a custom stack, an app folder on another place, a link
    let droot = dir.path().join("droot");
    let stacks = droot.join("vol1/docker/stacks");
    std::fs::create_dir_all(stacks.join("immich/library")).unwrap();
    std::fs::write(stacks.join("immich/compose.yaml"), "services: {}\nx-rynas:\n  app: immich\n  version: 1.0.0\n  app_version: \"2.0\"\n  catalog: \"1\"\n").unwrap();
    std::fs::write(stacks.join("immich/library/config.yml"), "a: 1\n").unwrap();
    std::fs::create_dir_all(stacks.join("custom")).unwrap();
    std::fs::write(stacks.join("custom/compose.yaml"), "services: {}\n").unwrap();
    // no compose file: not a stack
    std::fs::create_dir_all(stacks.join("notastack")).unwrap();

    let (app, ops) = app_with(dir.path(), &[("docs", &docs)]);
    let tx = docker_for(&ops, dir.path(), &droot, true);

    let (s, v) = get(&app, "/v1/files/init").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let names: Vec<&str> = v["shares"]["shares"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert!(!names.iter().any(|n| n.starts_with('@')), "the apps' folders are not shared folders: {names:?}");
    assert_eq!(v["shares"]["apps"], json!([{ "name": "custom", "stack": "custom" }, { "name": "immich", "stack": "immich", "app": "immich" }]));

    let (s, v) = get(&app, "/v1/files/list?path=/@apps").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!((v["apps"].as_bool(), v["writable"].as_bool()), (Some(true), Some(false)));
    let listed: Vec<&str> = v["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(listed, ["custom", "immich"]);
    let (s, v) = get(&app, "/v1/files/list?path=/@apps/immich").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["stack"], "immich");
    assert!(v["entries"].as_array().unwrap().iter().any(|e| e["name"] == "library"));
    // nothing is made in the folder of the apps itself, nor out of an app's folder
    let (s, v) = json_call(&app, "POST", "/v1/files/mkdir?path=/@apps&name=x", json!({})).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::CONFLICT, Some(error_codes::READ_ONLY)), "{v}");
    let (s, _) = get(&app, "/v1/files/list?path=/@apps/immich/../custom").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // an app's folder is like a shared folder: not renamed, moved or deleted
    let (s, v) = json_call(&app, "POST", "/v1/files/rename?path=/@apps/immich&name=x", json!({})).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::BAD_REQUEST, Some(error_codes::SHARE_ROOT)), "{v}");
    // inside: an ordinary folder (the worker here is the test user, who owns the files)
    let (s, v) = json_call(&app, "POST", "/v1/files/mkdir?path=/@apps/immich/library&name=new", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(stacks.join("immich/library/new").is_dir());
    let (s, v) = get(&app, "/v1/files/edit?path=/@apps/immich/library/config.yml").await;
    assert_eq!((s, v["text"].as_str()), (StatusCode::OK, Some("a: 1\n")), "{v}");

    // the setting off: gone, for the interface and for every route
    tx.send_replace(rynas_api_types::docker::DockerSettings { volume: Some("vol1".into()), files_app_data: false, ..Default::default() });
    let (_, v) = get(&app, "/v1/files/shares").await;
    assert!(v.get("apps").is_none(), "{v}");
    let (s, _) = get(&app, "/v1/files/list?path=/@apps/immich").await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // a user who is no administrator never sees them, setting on or off
    let dir2 = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (app2, ops2) = app_full(dir2.path(), &[("docs", &docs)], false, None);
    let _tx2 = docker_for(&ops2, dir2.path(), &droot, true);
    let (_, v) = get(&app2, "/v1/files/shares").await;
    assert!(v.get("apps").is_none(), "{v}");
    let (s, _) = get(&app2, "/v1/files/list?path=/@apps/immich").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, v) = json_call(&app2, "POST", "/v1/files/jobs/new", json!({ "op": "delete", "sources": ["/@apps/immich/library/config.yml"] })).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    assert!(stacks.join("immich/library/config.yml").exists());
}
