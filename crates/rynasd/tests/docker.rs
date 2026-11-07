//! Docker through the internal API on a machine without Docker (a fake host tree for the
//! detection, no engine socket, `--dry-system` behaviour): status, the clear error of every
//! other route, admin checks, the `docker` key (validation, apply of the engine files, the
//! plan), the install task as a dry run, and stack files of a rynas stack.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rynas_api_types::docker::{DockerSettingsState, DockerStatus, EnginePlan, StackFiles};
use rynas_api_types::{ApiError, ApplyRequest, TaskInfo, TaskStarted, TaskStatus, error_codes};
use rynas_auth::ManualClock;
use rynas_auth::backend::{AuthError, Authenticator, BoxFuture, Identity};
use rynas_auth::totp::{MemoryStorage, TotpManager};
use rynas_config::{Applier, Config, ConfigOptions, MemorySystem};
use rynas_docker::Runner;
use rynas_notify::MemoryNotifier;
use rynas_tasks::{Cmd, CmdOutput, TaskConfig, TaskManager};
use rynasd::docker::{DockerDomain, DockerSvc};
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
    fn authenticate<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, Result<Identity, AuthError>> {
        Box::pin(async { Err(AuthError::InvalidCredentials) })
    }
}

/// Only `dpkg --print-architecture` answers; every other program is missing.
struct Host(Mutex<Vec<String>>);

impl Runner for Host {
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, String> {
        let line = cmd.display();
        self.0.lock().unwrap().push(line.clone());
        if line == "dpkg --print-architecture" {
            return Ok(CmdOutput {
                code: Some(0),
                stdout: "amd64\n".into(),
                stderr: String::new(),
            });
        }
        Err("cannot start: No such file or directory".into())
    }
}

struct Env {
    dir: tempfile::TempDir,
    app: axum::Router,
    ops: Arc<Ops>,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("vol1")).unwrap();
    let host = dir.path().join("host");
    std::fs::create_dir_all(host.join("etc")).unwrap();
    std::fs::write(
        host.join("etc/os-release"),
        "ID=debian\nVERSION_CODENAME=trixie\n",
    )
    .unwrap();
    let (domain, settings) = DockerDomain::new(true);
    let domains: Vec<Arc<dyn Applier>> = vec![Arc::new(domain)];
    let config = Arc::new(
        Config::open(
            ConfigOptions::new(dir.path().join("state"), &root),
            Arc::new(MemorySystem::new("nas")),
            domains,
        )
        .unwrap(),
    );
    let tasks = TaskManager::new(dir.path().join("tasks"), "nas", TaskConfig::default()).unwrap();
    let mut ops = Ops::new(config, tasks);
    ops.admins = AdminPolicy::new(vec!["rynas-admin".into()], |u| match u {
        "admin" => Some(vec!["rynas-admin".into()]),
        "bob" => Some(vec!["users".into()]),
        _ => None,
    });
    let ops = Arc::new(ops);
    let svc = DockerSvc::with_host(
        ops.clone(),
        Arc::new(MemoryNotifier::default()),
        root,
        true,
        Some(
            dir.path()
                .join("no-docker.sock")
                .to_string_lossy()
                .into_owned(),
        ),
        dir.path().join("state/docker"),
        settings,
        host,
        Arc::new(Host(Mutex::default())),
    );
    let _ = ops.docker.set(svc.clone());
    let clock = Arc::new(ManualClock::new(1_800_000_000_000));
    let totp = Arc::new(TotpManager::new(
        MemoryStorage::default(),
        clock.clone(),
        "rynas",
    ));
    let mut st = DaemonState::new(Arc::new(NoAuth), totp, clock);
    st.ops = Some(ops.clone());
    st.docker = Some(svc);
    Env {
        dir,
        app: router(st),
        ops,
    }
}

async fn call_as(
    e: &Env,
    who: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-rynas-user", who)
        .body(body.map(|b| Body::from(b.to_string())).unwrap_or_default())
        .unwrap();
    let r = e.app.clone().oneshot(req).await.unwrap();
    let s = r.status();
    (s, to_bytes(r.into_body(), 4 << 20).await.unwrap().to_vec())
}

async fn call(e: &Env, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Vec<u8>) {
    call_as(e, "admin", method, uri, body).await
}

fn err(b: &[u8]) -> ApiError {
    serde_json::from_slice(b)
        .unwrap_or_else(|_| panic!("not an ApiError: {}", String::from_utf8_lossy(b)))
}

async fn wait_task(e: &Env, id: &str) -> TaskInfo {
    for _ in 0..500 {
        if let Some(t) = e.ops.tasks.get(id)
            && t.status.is_finished()
        {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task {id} still running");
}

fn log(e: &Env, id: &str) -> String {
    e.ops.tasks.read_log(id, 0, usize::MAX).unwrap().text
}

#[tokio::test]
async fn without_docker_only_status_answers() {
    let e = env();
    let (s, b) = call(&e, "GET", "/v1/docker/status", None).await;
    assert_eq!(s, StatusCode::OK);
    let st: DockerStatus = serde_json::from_slice(&b).unwrap();
    assert!(!st.installed && !st.running);
    assert_eq!(
        st.install_block, None,
        "trixie/amd64 without an engine: install offered"
    );
    assert!(st.warnings.is_empty());
    for uri in [
        "/v1/docker/containers",
        "/v1/docker/stacks",
        "/v1/docker/images",
        "/v1/docker/volumes",
        "/v1/docker/networks",
        "/v1/docker/df",
        "/v1/docker/stats",
        "/v1/docker/containers/web/logs",
    ] {
        let (s, b) = call(&e, "GET", uri, None).await;
        assert_eq!(
            (s, err(&b).code.as_str()),
            (StatusCode::CONFLICT, error_codes::DOCKER_NOT_INSTALLED),
            "{uri}"
        );
    }
    let (s, b) = call(&e, "POST", "/v1/docker/containers/web/restart", None).await;
    assert_eq!(
        (s, err(&b).code.as_str()),
        (StatusCode::CONFLICT, error_codes::DOCKER_NOT_INSTALLED)
    );
    // never a generic proxy: unknown actions and paths are 404
    let (s, _) = call(&e, "POST", "/v1/docker/containers/web/exec", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    for uri in [
        "/v1/docker/containers/json",
        "/v1/docker/info",
        "/v1/docker/version",
    ] {
        let (s, _) = call(&e, "GET", uri, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{uri}");
    }
    let (s, b) = call(&e, "POST", "/v1/docker/containers/..%2Fx/start", None).await;
    assert_eq!(
        (s, err(&b).code.as_str()),
        (StatusCode::BAD_REQUEST, error_codes::BAD_REQUEST)
    );
}

#[tokio::test]
async fn changes_need_an_administrator() {
    let e = env();
    let (s, b) = call_as(&e, "bob", "POST", "/v1/docker/install", None).await;
    assert_eq!(
        (s, err(&b).code.as_str()),
        (StatusCode::FORBIDDEN, error_codes::FORBIDDEN)
    );
    let (s, _) = call_as(&e, "bob", "PUT", "/v1/docker/settings", Some(json!({}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call_as(&e, "bob", "POST", "/v1/docker/containers/web/stop", None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call_as(&e, "bob", "GET", "/v1/docker/stacks/app/files", None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, ".env holds secrets");
}

#[tokio::test]
async fn settings_apply_writes_the_engine_files() {
    let e = env();
    let (s, b) = call(
        &e,
        "PUT",
        "/v1/docker/settings",
        Some(json!({"address_pool": {"base": "10.0.0.1/16", "size": 24}})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(err(&b).field.as_deref(), Some("address_pool.base"));
    let (s, b) = call(
        &e,
        "PUT",
        "/v1/docker/settings",
        Some(json!({"volume": "No Such"})),
    )
    .await;
    assert_eq!(
        (s, err(&b).field.as_deref()),
        (StatusCode::BAD_REQUEST, Some("volume"))
    );

    // what an apply would write
    let (s, b) = call(
        &e,
        "POST",
        "/v1/docker/plan/engine",
        Some(json!({"volume": "vol1"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let plan: EnginePlan = serde_json::from_slice(&b).unwrap();
    assert_eq!(plan.files.len(), 4);
    assert!(plan.restart);
    assert!(plan.create.contains(&"/vol1/docker/engine".to_string()));

    let (s, b) = call(
        &e,
        "PUT",
        "/v1/docker/settings",
        Some(json!({"volume": "vol1"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let st: DockerSettingsState = serde_json::from_slice(&b).unwrap();
    assert_eq!(
        st.pending.as_ref().and_then(|p| p.volume.as_deref()),
        Some("vol1")
    );
    assert_eq!(st.applied.volume, None);

    let (s, b) = call(
        &e,
        "POST",
        "/v1/config/apply",
        Some(serde_json::to_value(ApplyRequest::default()).unwrap()),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let t: TaskStarted = serde_json::from_slice(&b).unwrap();
    let info = wait_task(&e, &t.task_id).await;
    assert_eq!(info.status, TaskStatus::Ok, "{}", log(&e, &t.task_id));

    let root = e.dir.path().join("root");
    let dj: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("etc/docker/daemon.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(dj["data-root"], "/vol1/docker/engine");
    assert_eq!(dj["live-restore"], true);
    let toml = std::fs::read_to_string(root.join("etc/containerd/config.toml")).unwrap();
    assert!(toml.contains("root = \"/vol1/docker/containerd\""));
    let dropin =
        std::fs::read_to_string(root.join("etc/systemd/system/docker.service.d/rynas-mounts.conf"))
            .unwrap();
    assert!(dropin.contains("RequiresMountsFor=/vol1/docker/engine /vol1/docker/containerd"));
    for d in ["engine", "containerd", "stacks"] {
        assert!(root.join("vol1/docker").join(d).is_dir(), "{d}");
    }
    let (_, b) = call(&e, "GET", "/v1/docker/settings", None).await;
    let st: DockerSettingsState = serde_json::from_slice(&b).unwrap();
    assert_eq!(st.applied.volume.as_deref(), Some("vol1"));
    assert_eq!(st.stacks_dir.as_deref(), Some("/vol1/docker/stacks"));
    assert!(st.pending.is_none());
    // an edit by hand is drift
    let (_, b) = call(&e, "GET", "/v1/config/drift", None).await;
    assert!(!String::from_utf8_lossy(&b).contains("daemon.json"));
    std::fs::write(root.join("etc/docker/daemon.json"), "{\"debug\": true}\n").unwrap();
    let (s, b) = call(&e, "GET", "/v1/config/drift", None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(String::from_utf8_lossy(&b).contains("daemon.json"));

    // a rynas stack folder: its files are readable, an unknown one is 404
    let fake = Host(Mutex::default());
    let fs = rynas_docker::stacks::StackFs {
        root: &root,
        runner: &fake,
        subvolumes: false,
        owner: None,
    };
    fs.create_stack(
        &rynas_docker::Layout::of_label("vol1"),
        "app",
        "services:\n  a:\n    image: nginx\n",
        "A=1\n",
        &[],
        &|_| {},
    )
    .unwrap();
    let (s, b) = call(&e, "GET", "/v1/docker/stacks/app/files", None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let f: StackFiles = serde_json::from_slice(&b).unwrap();
    assert_eq!(
        (f.compose_file.as_str(), f.env.as_str()),
        ("compose.yaml", "A=1\n")
    );
    let (s, _) = call(&e, "GET", "/v1/docker/stacks/nope/files", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&e, "GET", "/v1/docker/stacks/Bad%20Name/files", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // snapshots of a plain folder: none, and taking one is refused
    let (s, b) = call(&e, "GET", "/v1/docker/stacks/app/snapshots", None).await;
    assert_eq!((s, b.as_slice()), (StatusCode::OK, b"[]".as_slice()));
    let (s, _) = call(&e, "POST", "/v1/docker/stacks/app/snapshots", None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // deleting the folder needs the confirmation
    let (s, b) = call(
        &e,
        "POST",
        "/v1/docker/stacks/app/remove",
        Some(json!({"delete_files": true})),
    )
    .await;
    assert_eq!(
        (s, err(&b).code.as_str()),
        (StatusCode::BAD_REQUEST, error_codes::CONFIRMATION_REQUIRED)
    );
    // a dry-run compose task: logged, not run
    let (s, b) = call(&e, "POST", "/v1/docker/stacks/app/down", None).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let t: TaskStarted = serde_json::from_slice(&b).unwrap();
    let info = wait_task(&e, &t.task_id).await;
    assert_eq!(info.status, TaskStatus::Ok);
    let l = log(&e, &t.task_id);
    assert!(
        l.contains("docker compose -p app --project-directory")
            && l.contains(" down (dry system: not run)"),
        "{l}"
    );
    assert!(Path::new(&root.join("vol1/docker/stacks/app/compose.yaml")).is_file());
}

#[tokio::test]
async fn install_dry_run_with_a_volume() {
    let e = env();
    let (s, b) = call(
        &e,
        "POST",
        "/v1/docker/install",
        Some(json!({"volume": "vol1"})),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let t: TaskStarted = serde_json::from_slice(&b).unwrap();
    // one at a time
    let (s, _) = call(&e, "POST", "/v1/docker/install", None).await;
    assert!(s == StatusCode::CONFLICT || s == StatusCode::ACCEPTED);
    let info = wait_task(&e, &t.task_id).await;
    let l = log(&e, &t.task_id);
    assert_eq!(info.status, TaskStatus::Ok, "{l}");
    assert!(
        l.contains("apt-get install -y -o Dpkg::Options::=--force-confdef"),
        "{l}"
    );
    assert!(l.contains("(dry system: not run)"));
    assert!(l.contains("groupadd --system docker-apps"));
    let root = e.dir.path().join("root");
    let src = std::fs::read_to_string(root.join("etc/apt/sources.list.d/docker.sources")).unwrap();
    assert!(src.contains("Suites: trixie") && src.contains("Architectures: amd64"));
    // the engine configuration was applied before the packages
    assert!(root.join("etc/docker/daemon.json").is_file());
    let (_, b) = call(&e, "GET", "/v1/docker/settings", None).await;
    let st: DockerSettingsState = serde_json::from_slice(&b).unwrap();
    assert_eq!(st.applied.volume.as_deref(), Some("vol1"));
}
