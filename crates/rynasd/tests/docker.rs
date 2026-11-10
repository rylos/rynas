//! Docker through the internal API on a machine without Docker (a fake host tree for the
//! detection, no engine socket, `--dry-system` behaviour): status, the clear error of every
//! other route, admin checks, the `docker` key (validation, apply of the engine files, the
//! plan), the install task as a dry run, and stack files of a rynas stack.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rynas_api_types::docker::{
    AutoOutcome, DockerSettingsState, DockerStatus, EnginePlan, StackFiles, StackUpdate,
    UpdatePolicy, UpdateReport, UpdateVerdict,
};
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
    svc: Arc<DockerSvc>,
}

fn env() -> Env {
    env_with(true, Arc::new(Host(Mutex::default())), None)
}

/// `dry`: `--dry-system`; `runner`: detection (and, with `dry` off, every task command);
/// `volume`: the applied Docker volume.
fn env_with(dry: bool, runner: Arc<dyn Runner>, volume: Option<&str>) -> Env {
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
    if let Some(v) = volume {
        domain.tx.send_replace(rynas_api_types::docker::DockerSettings {
            volume: Some(v.into()),
            ..Default::default()
        });
    }
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
        dry,
        Some(
            dir.path()
                .join("no-docker.sock")
                .to_string_lossy()
                .into_owned(),
        ),
        dir.path().join("state/docker"),
        settings,
        host,
        runner,
    );
    if !dry {
        svc.exec_via_runner
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // the docker-apps account with the caller's ids (folders can be given to it)
        let (u, g) = my_ids();
        let etc = dir.path().join("root/etc");
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::write(
            etc.join("passwd"),
            format!("docker-apps:x:{u}:{g}::/nonexistent:/usr/sbin/nologin\n"),
        )
        .unwrap();
        std::fs::write(etc.join("group"), format!("docker-apps:x:{g}:\n")).unwrap();
    }
    let _ = ops.docker.set(svc.clone());
    let clock = Arc::new(ManualClock::new(1_800_000_000_000));
    let totp = Arc::new(TotpManager::new(
        MemoryStorage::default(),
        clock.clone(),
        "rynas",
    ));
    let mut st = DaemonState::new(Arc::new(NoAuth), totp, clock);
    st.ops = Some(ops.clone());
    st.docker = Some(svc.clone());
    Env {
        dir,
        app: router(st),
        ops,
        svc,
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
        l.contains("apt-get -o DPkg::Lock::Timeout=120 install -y -o Dpkg::Options::=--force-confdef"),
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

/// Answers task commands by program: `du` reports `du_bytes`, `cp` and `docker compose
/// pull` fail, everything else succeeds; every line is recorded.
struct Scripted {
    calls: Mutex<Vec<String>>,
    du_bytes: u64,
}

impl Runner for Scripted {
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, String> {
        let line = cmd.display();
        self.calls.lock().unwrap().push(line.clone());
        let ok = |out: &str| CmdOutput {
            code: Some(0),
            stdout: out.into(),
            stderr: String::new(),
        };
        let fail = |err: &str| CmdOutput {
            code: Some(1),
            stdout: String::new(),
            stderr: err.into(),
        };
        Ok(match line.split_whitespace().next().unwrap_or("") {
            "dpkg" => ok("amd64\n"),
            "du" => ok(&format!("{}\t/var/lib/docker\n", self.du_bytes)),
            "cp" => fail("cp: error writing: No space left on device\n"),
            "docker" if line.contains(" pull") => {
                fail("Error response from daemon: manifest unknown\n")
            }
            "docker" if line.contains(" config") => {
                ok(r#"{"services":{"web":{"image":"nginx:1.27.3"}}}"#)
            }
            _ => ok(""),
        })
    }
}

async fn run_migration(e: &Env) -> (TaskInfo, String) {
    let s = e.svc.clone();
    let id = e
        .ops
        .tasks
        .spawn("docker.migrate", "admin", move |ctx| async move {
            rynasd::docker::migrate_task(&s, &ctx, "admin", "vol1").await
        })
        .unwrap();
    let t = wait_task(e, &id).await;
    (t, log(e, &id))
}

/// Review 2026-11-07, M4: a migration whose copy fails starts the engine again on its old
/// folders and leaves the configuration alone; one that does not fit never stops it.
#[tokio::test]
async fn failed_migration_restarts_the_engine() {
    let r = Arc::new(Scripted {
        calls: Mutex::default(),
        du_bytes: 4096,
    });
    let e = env_with(false, r.clone(), None);
    std::fs::create_dir_all(e.dir.path().join("root/var/lib/docker/image")).unwrap();
    let (t, log) = run_migration(&e).await;
    assert_eq!(t.status, TaskStatus::Error, "{log}");
    assert!(log.contains("No space left on device"), "{log}");
    assert!(log.contains("started again"), "{log}");
    let calls = r.calls.lock().unwrap().clone();
    let pos = |p: &str| calls.iter().position(|c| c.starts_with(p));
    let (stop, cp, start) = (
        pos("systemctl stop").expect("stopped"),
        pos("cp -a").expect("copied"),
        pos("systemctl start containerd.service docker.socket docker.service").expect("started"),
    );
    assert!(stop < cp && cp < start, "{calls:?}");
    assert_eq!(e.svc.settings().volume, None, "the configuration did not move");

    // too large for the volume: refused before anything stops
    let r = Arc::new(Scripted {
        calls: Mutex::default(),
        du_bytes: u64::MAX / 4,
    });
    let e = env_with(false, r.clone(), None);
    std::fs::create_dir_all(e.dir.path().join("root/var/lib/docker")).unwrap();
    let (t, log) = run_migration(&e).await;
    assert_eq!(t.status, TaskStatus::Error);
    assert!(log.contains("not enough space"), "{log}");
    assert!(
        !r.calls.lock().unwrap().iter().any(|c| c.starts_with("systemctl")),
        "nothing stopped"
    );

    // an unmounted volume: nothing at all
    let r = Arc::new(Scripted {
        calls: Mutex::default(),
        du_bytes: 1,
    });
    let e = env_with(false, r.clone(), None);
    std::fs::remove_dir(e.dir.path().join("root/vol1")).unwrap();
    let (t, log) = run_migration(&e).await;
    assert_eq!(t.status, TaskStatus::Error);
    assert!(log.contains("not mounted"), "{log}");
    assert!(r.calls.lock().unwrap().iter().all(|c| !c.starts_with("systemctl")));
}

/// A failing `docker compose pull` fails `docker.stack.update` (it used to be ignored, so
/// a broken update never rolled back) and `up` never runs after it.
#[tokio::test]
async fn failed_update_pull_fails_the_task() {
    let r = Arc::new(Scripted {
        calls: Mutex::default(),
        du_bytes: 1,
    });
    let e = env_with(false, r.clone(), Some("vol1"));
    let dir = e.dir.path().join("root/vol1/docker/stacks/web");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("compose.yaml"), "services:\n  web:\n    image: x\n").unwrap();
    let (s, b) = call(
        &e,
        "POST",
        "/v1/docker/stacks/web/update",
        Some(json!({"snapshot": false})),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let id = serde_json::from_slice::<TaskStarted>(&b).unwrap().task_id;
    let t = wait_task(&e, &id).await;
    let log = log(&e, &id);
    assert_eq!(t.status, TaskStatus::Error, "{log}");
    assert!(log.contains("manifest unknown"), "{log}");
    let calls = r.calls.lock().unwrap().clone();
    assert!(calls.iter().any(|c| c.contains(" pull")), "{calls:?}");
    assert!(!calls.iter().any(|c| c.contains(" up ")), "{calls:?}");
}

/// Detection answers: `dpkg-query` prints `dpkg`, the architecture is amd64, every other
/// program succeeds silently; with `accounts` set (a root), `groupadd`/`useradd` write the
/// account there with the caller's ids.
struct Dpkg {
    dpkg: &'static str,
    accounts: Mutex<Option<std::path::PathBuf>>,
    calls: Mutex<Vec<String>>,
}

impl Dpkg {
    fn new(dpkg: &'static str) -> Self {
        Self {
            dpkg,
            accounts: Mutex::default(),
            calls: Mutex::default(),
        }
    }
}

fn my_ids() -> (u32, u32) {
    // SAFETY: getuid/getgid cannot fail
    unsafe { (libc::getuid(), libc::getgid()) }
}

impl Runner for Dpkg {
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, String> {
        let line = cmd.display();
        self.calls.lock().unwrap().push(line.clone());
        let out = match line.split_whitespace().next().unwrap_or("") {
            "dpkg-query" => self.dpkg,
            "dpkg" if line.contains("--print-architecture") => "amd64\n",
            "docker" if line.contains(" config") => r#"{"services":{"web":{"image":"x"}}}"#,
            p @ ("groupadd" | "useradd") => {
                if let Some(root) = self.accounts.lock().unwrap().as_ref() {
                    let (u, g) = my_ids();
                    let (file, text) = if p == "groupadd" {
                        ("etc/group", format!("docker-apps:x:{g}:\n"))
                    } else {
                        (
                            "etc/passwd",
                            format!("docker-apps:x:{u}:{g}::/nonexistent:/usr/sbin/nologin\n"),
                        )
                    };
                    std::fs::create_dir_all(root.join("etc")).unwrap();
                    let mut old = std::fs::read_to_string(root.join(file)).unwrap_or_default();
                    old.push_str(&text);
                    std::fs::write(root.join(file), old).unwrap();
                }
                ""
            }
            _ => "",
        };
        Ok(CmdOutput {
            code: Some(0),
            stdout: out.into(),
            stderr: String::new(),
        })
    }
}

/// `dpkg-query` after the first real install died inside dpkg.
const HALF_DONE: &str = "docker-ce\t5:29.0.1-1~debian.13~trixie\tiHR\ndocker-ce-cli\t5:29.0.1-1~debian.13~trixie\tii \ncontainerd.io\t2.1.5-1~debian.13~trixie\tiU \n";

/// A failed install left packages half-done: no engine (no "already installed: adopted"),
/// the packages listed, and the install repairs dpkg before anything else and finishes.
#[tokio::test]
async fn half_installed_engine_is_repaired_by_the_install() {
    let e = env_with(true, Arc::new(Dpkg::new(HALF_DONE)), None);
    // its unpacked binaries are there
    let host = e.dir.path().join("host");
    std::fs::create_dir_all(host.join("usr/bin")).unwrap();
    std::fs::write(host.join("usr/bin/docker"), "").unwrap();
    let (_, b) = call(&e, "GET", "/v1/docker/status", None).await;
    let st: DockerStatus = serde_json::from_slice(&b).unwrap();
    assert!(!st.installed, "{st:?}");
    assert_eq!(st.install_block, None);
    assert_eq!(
        st.install_incomplete
            .iter()
            .map(|p| (p.name.as_str(), p.status.as_str()))
            .collect::<Vec<_>>(),
        [("docker-ce", "iHR"), ("containerd.io", "iU")]
    );
    let (s, b) = call(&e, "POST", "/v1/docker/install", None).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let t: TaskStarted = serde_json::from_slice(&b).unwrap();
    let info = wait_task(&e, &t.task_id).await;
    let l = log(&e, &t.task_id);
    assert_eq!(info.status, TaskStatus::Ok, "{l}");
    let pos = |p: &str| l.find(p).unwrap_or_else(|| panic!("{p} missing: {l}"));
    let repair = pos("-- dpkg --configure -a --force-confdef --force-confold");
    let fix = pos(
        "-- apt-get -o DPkg::Lock::Timeout=120 install -f -y --no-remove -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold",
    );
    let sim = pos("$ apt-get -s install -f");
    assert!(repair < sim && sim < fix, "the dry run is logged first: {l}");
    let update = pos("-- apt-get -o DPkg::Lock::Timeout=120 update");
    let install = pos(
        "apt-get -o DPkg::Lock::Timeout=120 install -y -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold docker-ce",
    );
    let account = pos("groupadd --system docker-apps");
    let enable = pos("systemctl enable --now containerd.service docker.service");
    assert!(
        repair < fix && fix < update && update < install && install < account && account < enable,
        "{l}"
    );
    assert!(
        l.contains("repairing an interrupted install: docker-ce 5:29.0.1-1~debian.13~trixie (iHR)"),
        "{l}"
    );
    // the repair runs through systemd-run, outside the sandbox, like apt
    assert!(l.contains("systemd-run --wait --pipe"), "{l}");

    // run again (a resume after any failure): accepted and finished again
    let (s, b) = call(&e, "POST", "/v1/docker/install", None).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let t: TaskStarted = serde_json::from_slice(&b).unwrap();
    let info = wait_task(&e, &t.task_id).await;
    assert_eq!(info.status, TaskStatus::Ok, "{}", log(&e, &t.task_id));
}

/// An install rynas started whose engine is complete but whose last steps never ran (no pin,
/// no docker-apps account) is offered again and finished; a finished one is adopted.
#[tokio::test]
async fn unfinished_install_is_resumed() {
    let e = env_with(
        true,
        Arc::new(Dpkg::new(
            "docker-ce\t5:29.0.1-1~debian.13~trixie\tii \ndocker-ce-cli\t5:29.0.1-1~debian.13~trixie\tii \n",
        )),
        None,
    );
    let host = e.dir.path().join("host");
    std::fs::create_dir_all(host.join("etc/apt/sources.list.d")).unwrap();
    std::fs::write(host.join("etc/apt/sources.list.d/docker.sources"), "x").unwrap();
    let (_, b) = call(&e, "GET", "/v1/docker/status", None).await;
    let st: DockerStatus = serde_json::from_slice(&b).unwrap();
    assert!(st.installed && st.install_block.is_none(), "{st:?}");
    let (s, b) = call(&e, "POST", "/v1/docker/install", None).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let t: TaskStarted = serde_json::from_slice(&b).unwrap();
    let info = wait_task(&e, &t.task_id).await;
    let l = log(&e, &t.task_id);
    assert_eq!(info.status, TaskStatus::Ok, "{l}");
    assert!(
        l.contains("finishing an install that did not complete"),
        "{l}"
    );
    assert!(!l.contains("dpkg --configure"), "nothing to repair: {l}");

    // pin and account there: adopted, the install is refused
    std::fs::create_dir_all(host.join("etc/apt/preferences.d")).unwrap();
    std::fs::write(
        host.join("etc/apt/preferences.d/rynas-docker"),
        rynas_docker::install::preferences_content(29),
    )
    .unwrap();
    std::fs::write(host.join("etc/passwd"), "docker-apps:x:996:995::/:/x\n").unwrap();
    std::fs::write(host.join("etc/group"), "docker-apps:x:995:\n").unwrap();
    let (s, b) = call(&e, "POST", "/v1/docker/install", None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{}", String::from_utf8_lossy(&b));
}

/// The `docker-apps` account exists before any stack folder gets an owner: created when
/// missing, an error (never `nobody`) when it cannot be.
#[tokio::test]
async fn apps_account_is_ensured_before_stack_folders() {
    let r = Arc::new(Dpkg::new(""));
    let e = env_with(false, r.clone(), Some("vol1"));
    let root = e.dir.path().join("root");
    std::fs::remove_file(root.join("etc/passwd")).unwrap();
    std::fs::remove_file(root.join("etc/group")).unwrap();
    // groupadd/useradd "succeed" but no account appears: a clear error
    let err = e.svc.ensure_apps_account_blocking(&|_| {}).unwrap_err();
    assert!(err.contains("could not be created"), "{err}");
    // a stack's `up` fails before compose runs or any bind folder is made
    let dir = root.join("vol1/docker/stacks/web");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("compose.yaml"),
        "services:\n  web:\n    image: x\n",
    )
    .unwrap();
    let (s, b) = call(&e, "POST", "/v1/docker/stacks/web/up", None).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let id = serde_json::from_slice::<TaskStarted>(&b).unwrap().task_id;
    let t = wait_task(&e, &id).await;
    let l = log(&e, &id);
    assert_eq!(t.status, TaskStatus::Error, "{l}");
    assert!(l.contains("could not be created"), "{l}");
    assert!(!r.calls.lock().unwrap().iter().any(|c| c.contains(" up ")));

    // created when missing (with the ids it got), and only once
    *r.accounts.lock().unwrap() = Some(root.clone());
    r.calls.lock().unwrap().clear();
    let me = my_ids();
    assert_eq!(
        e.svc.ensure_apps_account_blocking(&|_| {}).unwrap(),
        Some(me)
    );
    let calls = r.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        calls[0].starts_with("groupadd --system docker-apps"),
        "{calls:?}"
    );
    assert!(calls[1].starts_with("useradd --system"), "{calls:?}");
    assert_eq!(
        e.svc.ensure_apps_account_blocking(&|_| {}).unwrap(),
        Some(me)
    );
    assert_eq!(r.calls.lock().unwrap().len(), 2, "nothing to create");
}

// ------------------------------------------------------------------ adopted stacks (§5.7)

/// Compose and copies for the adoption tests: `config` answers one bind `<project dir>/data`
/// (so a moved folder moves its bind), `cp -a --reflink=auto <from>/. <to>/` copies.
struct Adopting(Mutex<Vec<String>>);

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            copy_tree(&p, &to.join(e.file_name()));
        } else {
            std::fs::copy(&p, to.join(e.file_name())).unwrap();
        }
    }
}

impl Runner for Adopting {
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, String> {
        let line = cmd.display();
        self.0.lock().unwrap().push(line.clone());
        let words: Vec<&str> = line.split_whitespace().collect();
        let out = match words.first().copied().unwrap_or("") {
            "dpkg" => "amd64\n".to_string(),
            "docker" if line.contains(" config") => {
                let i = words.iter().position(|w| *w == "--project-directory").unwrap();
                format!(
                    r#"{{"services":{{"web":{{"image":"x","volumes":[{{"type":"bind","source":"{}/data","target":"/data"}}]}}}}}}"#,
                    words[i + 1]
                )
            }
            "cp" => {
                let n = words.len();
                copy_tree(
                    Path::new(words[n - 2].trim_end_matches("/.")),
                    Path::new(words[n - 1].trim_end_matches('/')),
                );
                String::new()
            }
            _ => String::new(),
        };
        Ok(CmdOutput {
            code: Some(0),
            stdout: out,
            stderr: String::new(),
        })
    }
}

/// A folder restored from another NAS: previewed (name from `name:`, the override found,
/// profiles and external networks warned), imported where it is (nothing written in it),
/// edited only with the hash it was read with, started with every `-f` file and profile,
/// moved into the rynas stacks folder (binds checked, the old folder set aside) and
/// released without touching the files.
#[tokio::test]
async fn import_edit_move_and_release_a_folder() {
    use rynas_api_types::docker::{AdoptPreview, AdoptWarning, StackAdopted};
    let r = Arc::new(Adopting(Mutex::default()));
    let e = env_with(false, r.clone(), Some("vol1"));
    let root = e.dir.path().join("root");
    let app = root.join("srv/compose/app");
    std::fs::create_dir_all(app.join("data")).unwrap();
    let main = "name: myapp\nservices:\n  web:\n    image: nginx:1.27\n    volumes:\n      - ./data:/data\n  debug:\n    image: busybox\n    profiles: [debug]\nnetworks:\n  proxy:\n    external: true\n";
    std::fs::write(app.join("docker-compose.yml"), main).unwrap();
    std::fs::write(
        app.join("docker-compose.override.yml"),
        "services:\n  web:\n    restart: always\n",
    )
    .unwrap();
    std::fs::write(app.join(".env"), "TZ=Europe/Rome\n").unwrap();
    std::fs::write(app.join("data/db"), "data").unwrap();

    // refused folders
    for dir in ["/etc", "relative", "/srv/none", "/srv"] {
        let (s, b) = call(
            &e,
            "POST",
            "/v1/docker/stacks/import/preview",
            Some(json!({ "dir": dir })),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{dir}: {}", String::from_utf8_lossy(&b));
        assert_eq!(err(&b).field.as_deref(), Some("dir"), "{dir}");
    }
    let (s, b) = call(
        &e,
        "POST",
        "/v1/docker/stacks/import/preview",
        Some(json!({"dir": "/srv/compose/app/"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let p: AdoptPreview = serde_json::from_slice(&b).unwrap();
    assert_eq!(p.name, "myapp");
    assert_eq!(p.dir, "/srv/compose/app");
    assert_eq!(
        p.files,
        [
            "/srv/compose/app/docker-compose.yml",
            "/srv/compose/app/docker-compose.override.yml"
        ]
    );
    assert_eq!(p.profiles_found, ["debug"]);
    assert_eq!(p.external_networks, ["proxy"]);
    assert_eq!(
        p.warnings,
        [AdoptWarning::ExternalNetworks, AdoptWarning::Profiles]
    );

    // a non-admin cannot import
    let (s, _) = call_as(
        &e,
        "bob",
        "POST",
        "/v1/docker/stacks/import",
        Some(json!({"dir": "/srv/compose/app"})),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let before = std::fs::read_dir(&app).unwrap().count();
    let (s, b) = call(
        &e,
        "POST",
        "/v1/docker/stacks/import",
        Some(json!({"dir": "/srv/compose/app", "profiles": ["debug"]})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    let a: StackAdopted = serde_json::from_slice(&b).unwrap();
    assert_eq!(
        (a.name.as_str(), a.task_id.is_none(), a.adopted.read_only),
        ("myapp", true, false)
    );
    assert_eq!(
        std::fs::read_dir(&app).unwrap().count(),
        before,
        "nothing written in the folder"
    );
    assert!(e.dir.path().join("state/docker/adopted.json").exists());
    // twice: already adopted; a native stack cannot take the name
    let (s, b) = call(
        &e,
        "POST",
        "/v1/docker/stacks/import",
        Some(json!({"dir": "/srv/compose/app"})),
    )
    .await;
    assert_eq!(
        (s, err(&b).code.as_str()),
        (StatusCode::CONFLICT, error_codes::ALREADY_EXISTS)
    );
    let (s, _) = call(
        &e,
        "POST",
        "/v1/docker/stacks",
        Some(json!({"name": "myapp", "compose": "services: {}\n"})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);

    // the editor: main file and .env, the override listed, a stale hash refused
    let (s, b) = call(&e, "GET", "/v1/docker/stacks/myapp/files", None).await;
    assert_eq!(s, StatusCode::OK);
    let f: StackFiles = serde_json::from_slice(&b).unwrap();
    assert_eq!(
        (f.compose_file.as_str(), f.env.as_str(), f.read_only),
        ("docker-compose.yml", "TZ=Europe/Rome\n", false)
    );
    assert_eq!(
        f.other_files,
        ["/srv/compose/app/docker-compose.override.yml"]
    );
    std::fs::write(app.join(".env"), "TZ=UTC\n").unwrap();
    let (s, b) = call(
        &e,
        "PUT",
        "/v1/docker/stacks/myapp/files",
        Some(json!({"compose": main, "env": "X=1\n", "hash": f.hash})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{}", String::from_utf8_lossy(&b));
    assert_eq!(err(&b).field.as_deref(), Some("hash"));
    assert_eq!(std::fs::read_to_string(app.join(".env")).unwrap(), "TZ=UTC\n");

    // up: every -f file, the profile, the folder as project directory
    let (s, b) = call(&e, "POST", "/v1/docker/stacks/myapp/up", None).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let id = serde_json::from_slice::<TaskStarted>(&b).unwrap().task_id;
    assert_eq!(
        wait_task(&e, &id).await.status,
        TaskStatus::Ok,
        "{}",
        log(&e, &id)
    );
    let real = app.to_string_lossy().into_owned();
    let up = r
        .0
        .lock()
        .unwrap()
        .iter()
        .find(|c| c.contains(" up "))
        .cloned()
        .unwrap();
    assert!(
        up.contains(&format!(
            "-p myapp --project-directory {real} -f {real}/docker-compose.yml -f {real}/docker-compose.override.yml --profile debug up"
        )),
        "{up}"
    );
    // deleting its files is refused (released instead)
    let (s, b) = call(
        &e,
        "POST",
        "/v1/docker/stacks/myapp/remove",
        Some(json!({"delete_files": true, "confirm": "myapp"})),
    )
    .await;
    assert_eq!(
        (s, err(&b).field.as_deref()),
        (StatusCode::BAD_REQUEST, Some("delete_files"))
    );

    // move: confirmed, copied with the data, binds checked, old folder set aside
    let (s, b) = call(
        &e,
        "POST",
        "/v1/docker/stacks/myapp/move",
        Some(json!({"confirm": "nope"})),
    )
    .await;
    assert_eq!(
        (s, err(&b).code.as_str()),
        (StatusCode::BAD_REQUEST, error_codes::CONFIRMATION_REQUIRED)
    );
    let (s, b) = call(
        &e,
        "POST",
        "/v1/docker/stacks/myapp/move",
        Some(json!({"confirm": "myapp"})),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let id = serde_json::from_slice::<TaskStarted>(&b).unwrap().task_id;
    let t = wait_task(&e, &id).await;
    let l = log(&e, &id);
    assert_eq!(t.status, TaskStatus::Ok, "{l}");
    let target = root.join("vol1/docker/stacks/myapp");
    assert_eq!(std::fs::read_to_string(target.join("data/db")).unwrap(), "data");
    assert!(!app.exists(), "{l}");
    let aside: Vec<_> = std::fs::read_dir(root.join("srv/compose"))
        .unwrap()
        .flatten()
        .map(|x| x.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        aside.len() == 1 && aside[0].starts_with("app.moved-"),
        "{aside:?}"
    );
    // two -f files: still adopted, in its new folder
    let (s, b) = call(&e, "GET", "/v1/docker/stacks/myapp/files", None).await;
    assert_eq!(s, StatusCode::OK);
    let f: StackFiles = serde_json::from_slice(&b).unwrap();
    assert_eq!(
        f.other_files,
        ["/vol1/docker/stacks/myapp/docker-compose.override.yml"]
    );

    // release: the record goes, the files stay
    let (s, _) = call(&e, "POST", "/v1/docker/stacks/myapp/release", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(target.join("docker-compose.yml").exists());
    let (s, _) = call(&e, "POST", "/v1/docker/stacks/myapp/release", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let reg = std::fs::read_to_string(e.dir.path().join("state/docker/adopted.json")).unwrap();
    assert!(!reg.contains("myapp"), "{reg}");
}

/// `config` answers a bind `../shared` relative to the project directory.
struct Outside;

impl Runner for Outside {
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, String> {
        let line = cmd.display();
        let words: Vec<&str> = line.split_whitespace().collect();
        let out = if line.starts_with("docker") && line.contains(" config") {
            let i = words
                .iter()
                .position(|w| *w == "--project-directory")
                .unwrap();
            let parent = Path::new(words[i + 1]).parent().unwrap().display().to_string();
            format!(
                r#"{{"services":{{"web":{{"image":"x","volumes":[{{"type":"bind","source":"{parent}/shared","target":"/s"}}]}}}}}}"#
            )
        } else {
            String::new()
        };
        Ok(CmdOutput {
            code: Some(0),
            stdout: out,
            stderr: String::new(),
        })
    }
}

/// A move whose bind would land elsewhere is undone: the copy removed, the record and the
/// old folder unchanged.
#[tokio::test]
async fn move_with_a_bind_outside_the_folder_is_undone() {
    let e = env_with(false, Arc::new(Outside), Some("vol1"));
    let app = e.dir.path().join("root/srv/app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(app.join("compose.yaml"), "services:\n  web:\n    image: x\n").unwrap();
    let (s, b) = call(
        &e,
        "POST",
        "/v1/docker/stacks/import",
        Some(json!({"dir": "/srv/app"})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    let (s, b) = call(
        &e,
        "POST",
        "/v1/docker/stacks/app/move",
        Some(json!({"confirm": "app"})),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let id = serde_json::from_slice::<TaskStarted>(&b).unwrap().task_id;
    let t = wait_task(&e, &id).await;
    let l = log(&e, &id);
    assert_eq!(t.status, TaskStatus::Error, "{l}");
    assert!(l.contains("bind mounts would point elsewhere"), "{l}");
    assert!(!e.dir.path().join("root/vol1/docker/stacks/app").exists());
    assert!(app.join("compose.yaml").exists());
    let (s, _) = call(&e, "GET", "/v1/docker/stacks/app/files", None).await;
    assert_eq!(s, StatusCode::OK, "still adopted where it was");
}

/// [`Adopting`], and after the copy the records cannot be saved any more (the state folder
/// of Docker, known once the test env exists, becomes a file).
struct RecordFails(Adopting, Arc<Mutex<std::path::PathBuf>>);

impl Runner for RecordFails {
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, String> {
        let out = self.0.run(cmd)?;
        if cmd.display().starts_with("cp ") {
            let state = self.1.lock().unwrap().clone();
            std::fs::rename(&state, state.with_extension("away")).unwrap();
            std::fs::write(&state, "not a folder").unwrap();
        }
        Ok(out)
    }
}

/// A move whose record cannot be saved is undone: the copy removed, the old folder where
/// it was (not set aside), the stack still adopted there.
#[tokio::test]
async fn move_with_a_record_that_cannot_be_saved_is_undone() {
    let state = Arc::new(Mutex::new(std::path::PathBuf::new()));
    let r = Arc::new(RecordFails(Adopting(Mutex::default()), state.clone()));
    let e = env_with(false, r, Some("vol1"));
    *state.lock().unwrap() = e.dir.path().join("state/docker");
    let app = e.dir.path().join("root/srv/app");
    std::fs::create_dir_all(app.join("data")).unwrap();
    std::fs::write(app.join("compose.yaml"), "services:\n  web:\n    image: x\n").unwrap();
    // env files in system folders are refused
    let (s, b) = call(
        &e,
        "POST",
        "/v1/docker/stacks/import",
        Some(json!({"dir": "/srv/app", "env_files": ["/etc/shadow"]})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{}", String::from_utf8_lossy(&b));
    let (s, b) = call(
        &e,
        "POST",
        "/v1/docker/stacks/import",
        Some(json!({"dir": "/srv/app"})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    let (s, b) = call(
        &e,
        "POST",
        "/v1/docker/stacks/app/move",
        Some(json!({"confirm": "app"})),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let id = serde_json::from_slice::<TaskStarted>(&b).unwrap().task_id;
    let t = wait_task(&e, &id).await;
    let l = log(&e, &id);
    assert_eq!(t.status, TaskStatus::Error, "{l}");
    assert!(l.contains("the record could not be saved"), "{l}");
    assert!(!e.dir.path().join("root/vol1/docker/stacks/app").exists(), "{l}");
    assert!(app.join("compose.yaml").exists() && app.join("data").is_dir());
    let (s, _) = call(&e, "GET", "/v1/docker/stacks/app/files", None).await;
    assert_eq!(s, StatusCode::OK, "still adopted where it was");
}

/// A stack folder swapped for a link before the move starts is not copied.
#[tokio::test]
async fn move_of_a_folder_made_a_link_is_refused() {
    let r = Arc::new(Adopting(Mutex::default()));
    let e = env_with(false, r.clone(), Some("vol1"));
    let app = e.dir.path().join("root/srv/app");
    std::fs::create_dir_all(app.join("data")).unwrap();
    std::fs::write(app.join("compose.yaml"), "services:\n  web:\n    image: x\n").unwrap();
    let (s, _) = call(
        &e,
        "POST",
        "/v1/docker/stacks/import",
        Some(json!({"dir": "/srv/app"})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let elsewhere = e.dir.path().join("root/srv/real");
    std::fs::rename(&app, &elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &app).unwrap();
    let (s, b) = call(
        &e,
        "POST",
        "/v1/docker/stacks/app/move",
        Some(json!({"confirm": "app"})),
    )
    .await;
    if s == StatusCode::ACCEPTED {
        let id = serde_json::from_slice::<TaskStarted>(&b).unwrap().task_id;
        let t = wait_task(&e, &id).await;
        let l = log(&e, &id);
        assert_eq!(t.status, TaskStatus::Error, "{l}");
        assert!(l.contains("not a plain folder"), "{l}");
    } else {
        assert!(s.is_client_error(), "{s}");
    }
    assert!(!r.0.lock().unwrap().iter().any(|c| c.starts_with("cp ")));
    assert!(!e.dir.path().join("root/vol1/docker/stacks/app").exists());
}

/// Compose commands succeed (`docker compose config` answers one service); every line is
/// recorded. No engine socket: the health check after `up` cannot read the containers.
struct ComposeOk(Mutex<Vec<String>>);

impl Runner for ComposeOk {
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, String> {
        let line = cmd.display();
        self.0.lock().unwrap().push(line.clone());
        let out = if line.contains(" config") {
            r#"{"services":{"web":{"image":"nginx:1.27.3"}}}"#
        } else if line.starts_with("dpkg") {
            "amd64\n"
        } else {
            ""
        };
        Ok(CmdOutput {
            code: Some(0),
            stdout: out.into(),
            stderr: String::new(),
        })
    }
}

fn web_update() -> StackUpdate {
    StackUpdate {
        stack: "web".into(),
        images: vec!["nginx:1.27.3".into()],
        enabled: true,
        verdict: UpdateVerdict::Auto,
        ..Default::default()
    }
}

fn web_stack(e: &Env) {
    let dir = e.dir.path().join("root/vol1/docker/stacks/web");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("compose.yaml"),
        "services:\n  web:\n    image: nginx:1.27.3\n",
    )
    .unwrap();
}

async fn report(e: &Env) -> UpdateReport {
    let (s, b) = call(e, "GET", "/v1/docker/updates", None).await;
    assert_eq!(s, StatusCode::OK);
    serde_json::from_slice(&b).unwrap()
}

/// The `auto_update` part of the `docker` key: validated, entries that say nothing are
/// dropped, and the report lists no stack while nothing has an update.
#[tokio::test]
async fn auto_update_settings_are_checked() {
    let e = env();
    let (s, b) = call(
        &e,
        "PUT",
        "/v1/docker/settings",
        Some(json!({"auto_update": {"window_min": 5}})),
    )
    .await;
    assert_eq!(
        (s, err(&b).field.as_deref()),
        (StatusCode::BAD_REQUEST, Some("auto_update.window_min"))
    );
    let (s, b) = call(
        &e,
        "PUT",
        "/v1/docker/settings",
        Some(json!({"auto_update": {"hour": 2, "stacks": {
            "web": {"enabled": true, "policy": "patch"},
            "idle": {"enabled": false}
        }}})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let st: DockerSettingsState = serde_json::from_slice(&b).unwrap();
    let p = st.pending.unwrap().auto_update;
    assert_eq!(p.hour, 2);
    assert_eq!(p.stacks.keys().cloned().collect::<Vec<_>>(), vec!["web".to_string()]);
    assert_eq!(p.stacks["web"].policy, Some(UpdatePolicy::Patch));
    let r = report(&e).await;
    assert!(r.stacks.is_empty() && r.auto.is_empty());
}

/// An automatic update whose download fails changes nothing: no `up`, outcome `failed`.
#[tokio::test]
async fn auto_update_with_a_failed_download_changes_nothing() {
    let r = Arc::new(Scripted {
        calls: Mutex::default(),
        du_bytes: 1,
    });
    let e = env_with(false, r.clone(), Some("vol1"));
    web_stack(&e);
    let id = rynasd::docker_auto::start_auto(&e.svc, &web_update(), "rynas").unwrap();
    let t = wait_task(&e, &id).await;
    let l = log(&e, &id);
    assert_eq!(t.status, TaskStatus::Error, "{l}");
    assert!(l.contains("download failed"), "{l}");
    let calls = r.calls.lock().unwrap().clone();
    assert!(!calls.iter().any(|c| c.contains(" up ")), "{calls:?}");
    let rep = report(&e).await;
    assert_eq!(rep.auto.len(), 1);
    assert_eq!(rep.auto[0].outcome, AutoOutcome::Failed);
    assert_eq!(rep.auto[0].task_id.as_deref(), Some(id.as_str()));
}

/// A new version that cannot be shown healthy is rolled back: pull, up, health check failed
/// (no engine answers), then `up` again on the previous images; the stack is not busy
/// afterwards.
#[tokio::test]
async fn auto_update_rolls_back_an_unhealthy_stack() {
    let r = Arc::new(ComposeOk(Mutex::default()));
    let e = env_with(false, r.clone(), Some("vol1"));
    web_stack(&e);
    let id = rynasd::docker_auto::start_auto(&e.svc, &web_update(), "rynas").unwrap();
    let t = wait_task(&e, &id).await;
    let l = log(&e, &id);
    assert_eq!(t.status, TaskStatus::Error, "{l}");
    assert!(l.contains("step 4/4: health check"), "{l}");
    assert!(l.contains("rolling back"), "{l}");
    let calls = r.0.lock().unwrap().clone();
    let pulls = calls.iter().filter(|c| c.contains(" pull")).count();
    let ups = calls.iter().filter(|c| c.contains(" up ")).count();
    assert_eq!((pulls, ups), (1, 2), "{calls:?}");
    // only the services whose images the policy judged are pulled and recreated
    assert!(
        calls
            .iter()
            .filter(|c| c.contains(" pull") || c.contains(" up "))
            .all(|c| c.ends_with(" -- web")),
        "{calls:?}"
    );
    let rep = report(&e).await;
    assert_eq!(rep.auto[0].outcome, AutoOutcome::RolledBack);
    assert!(
        rep.auto[0].detail.as_deref().unwrap_or("").contains("Docker"),
        "{:?}",
        rep.auto
    );
    // the busy mark is gone: a second run starts
    assert!(rynasd::docker_auto::start_auto(&e.svc, &web_update(), "rynas").is_ok());
}
