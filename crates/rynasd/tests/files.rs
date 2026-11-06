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
use rynasd::files::Files;
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

#[tokio::test]
async fn the_file_manager_end_to_end() {
    let worker = worker();
    // not below /tmp: shares there are refused
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let docs = dir.path().join("docs");
    std::fs::create_dir_all(docs.join("old")).unwrap();
    std::fs::write(docs.join("old/notes.txt"), "line one\r\nline two\r\n").unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let sys = Arc::new(MemorySystem::new("nas"));
    let acc = Arc::new(MemoryAccounts::new());
    let host = Arc::new(MemoryShares::new());
    host.add_dir(docs.to_str().unwrap(), None);
    let config = Arc::new(
        Config::open(
            ConfigOptions::new(dir.path().join("state"), &root),
            sys,
            vec![Arc::new(UsersDomain::new(acc.clone())), Arc::new(SharesDomain::new(host, acc))],
        )
        .unwrap(),
    );
    let shares = json!({ "shares": [{ "name": "docs", "path": docs, "description": "Documents" }] });
    config.apply_value("test", SHARES_KEY, Some(shares), "a share", &|_| {}).unwrap();
    let tasks = TaskManager::new(dir.path().join("tasks"), "nas", TaskConfig::default()).unwrap();
    let mut ops = Ops::new(config, tasks);
    ops.admins = AdminPolicy::new(vec!["rynas-admin".into()], |_| Some(vec!["rynas-admin".into()]));
    let ops = Arc::new(ops);
    let files = Arc::new(Files::new(ops.clone(), worker, &dir.path().join("state/files"), &dir.path().join("cache")));
    let clock = Arc::new(ManualClock::new(1_800_000_000_000));
    let totp = Arc::new(TotpManager::new(MemoryStorage::default(), clock.clone(), "rynas"));
    let mut st = DaemonState::new(Arc::new(NoAuth), totp, clock);
    st.ops = Some(ops.clone());
    st.files = Some(files);
    let app = router(st);

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
