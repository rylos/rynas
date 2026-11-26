//! The app catalogue through the internal API, with the repository's `apps/` catalogue, a
//! fake host tree (Docker binaries present, a listening socket, the installed RAM), no
//! engine socket and `--dry-system` behaviour: the catalogue and its images, plans (errors
//! by field, a port conflict with a suggestion, masked secrets), an install as a dry run
//! that writes the real stack files, the installed list, the update plan and update,
//! convert to custom stack, and uninstall keeping the data. With web access (W1, a fake
//! proxy admin socket): the default binding by proxy state, the route an install adds, the
//! proxy's ports counted as used, "Move behind the proxy", the route removed at uninstall.

mod fake_caddy;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rynas_api_types::apps::*;
use rynas_api_types::docker::DockerSettings;
use rynas_api_types::{ApiError, TaskInfo, TaskStarted, TaskStatus, error_codes};
use rynas_auth::ManualClock;
use rynas_auth::backend::{AuthError, Authenticator, BoxFuture, Identity};
use rynas_auth::totp::{MemoryStorage, TotpManager};
use rynas_config::{Applier, Config, ConfigOptions, MemorySystem};
use rynas_docker::Runner;
use rynas_notify::MemoryNotifier;
use rynas_tasks::{Cmd, CmdOutput, TaskConfig, TaskManager};
use rynasd::apps::AppsSvc;
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

/// The detection's commands: an architecture and a compose plugin; nothing else runs.
struct Host;

impl Runner for Host {
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, String> {
        let ok = |s: &str| {
            Ok(CmdOutput {
                code: Some(0),
                stdout: s.into(),
                stderr: String::new(),
            })
        };
        match cmd.display().as_str() {
            "dpkg --print-architecture" => ok("amd64\n"),
            "docker compose version --short" => ok("2.40.3\n"),
            _ => Err("cannot start: No such file or directory".into()),
        }
    }
}

struct Env {
    dir: tempfile::TempDir,
    app: axum::Router,
    ops: Arc<Ops>,
    apps: Arc<AppsSvc>,
    /// Web access and its fake proxy, when the test asked for it.
    web: Option<(Arc<rynasd::web_access::WebAccessSvc>, fake_caddy::FakeCaddy)>,
}

impl Env {
    fn root(&self) -> PathBuf {
        self.dir.path().join("root")
    }
    fn stack(&self, name: &str) -> PathBuf {
        self.root().join("vol1/docker/stacks").join(name)
    }
}

fn catalogue() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps")
}

fn env() -> Env {
    env_opts(None)
}

/// `web`: `Some(installed)` adds web access with a fake proxy (installed or not).
fn env_opts(web: Option<bool>) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("vol1/media")).unwrap();
    let host = dir.path().join("host");
    for d in ["etc", "usr/bin", "proc/net"] {
        std::fs::create_dir_all(host.join(d)).unwrap();
    }
    std::fs::write(
        host.join("etc/os-release"),
        "ID=debian\nVERSION_CODENAME=trixie\n",
    )
    .unwrap();
    std::fs::write(host.join("etc/hostname"), "nas\n").unwrap();
    std::fs::write(host.join("usr/bin/docker"), "").unwrap();
    // a 4 GB machine (part of the RAM reserved by the firmware)
    std::fs::write(host.join("proc/meminfo"), "MemTotal:        3900000 kB\n").unwrap();
    // something listens on 8222/tcp (the default port of Vaultwarden)
    std::fs::write(
        host.join("proc/net/tcp"),
        "  sl  local_address rem_address   st tx_queue rx_queue\n   0: 00000000:201E 00000000:0000 0A 00000000:00000000\n",
    )
    .unwrap();
    let (domain, settings) = DockerDomain::new(true);
    domain.tx.send_replace(DockerSettings {
        volume: Some("vol1".into()),
        apps_ssd_volume: Some("ssd".into()),
        ..Default::default()
    });
    let mut domains: Vec<Arc<dyn Applier>> = vec![Arc::new(domain)];
    let web = web.map(|installed| {
        let caddy = fake_caddy::FakeCaddy::start(dir.path());
        let ca = rynas_tls::generate_ca("nas", rynas_tls::now()).unwrap();
        let mut w = rynasd::web_access::WebAccessSvc::new(
            root.clone(),
            caddy.socket.clone(),
            8080,
            8443,
            Box::new(|| rynasd::web_access::model::HostFacts {
                hostname: "nas".into(),
                lan: [rynasd::web_access::model::Cidr::parse("192.0.2.0/24").unwrap()].into(),
                primary_v4: Some("192.0.2.10".parse().unwrap()),
                ..Default::default()
            }),
            Box::new(move || Ok(ca.clone())),
            Box::new(rynasd::web_access::diag::SystemProbe),
        );
        w.installed_override = Some(installed);
        let w = Arc::new(w);
        domains.push(Arc::new(rynasd::web_access::WebAccessDomain { svc: w.clone() }));
        (w, caddy)
    });
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
        Arc::new(Host),
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
    let apps = AppsSvc::new(svc.clone(), catalogue());
    if let Some((w, _)) = &web {
        let _ = w.ops.set(ops.clone());
        let _ = apps.web.set(w.clone());
        st.web_access = Some(w.clone());
    }
    st.apps = Some(apps.clone());
    st.docker = Some(svc);
    Env {
        dir,
        app: router(st),
        ops,
        apps,
        web,
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

fn json_of<T: serde::de::DeserializeOwned>(s: StatusCode, b: &[u8]) -> T {
    assert!(s.is_success(), "{s}: {}", String::from_utf8_lossy(b));
    serde_json::from_slice(b).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(b)))
}

fn err(b: &[u8]) -> ApiError {
    serde_json::from_slice(b)
        .unwrap_or_else(|_| panic!("not an ApiError: {}", String::from_utf8_lossy(b)))
}

async fn wait_task(e: &Env, id: &str) -> (TaskInfo, String) {
    for _ in 0..500 {
        if let Some(t) = e.ops.tasks.get(id)
            && t.status.is_finished()
        {
            return (t, e.ops.tasks.read_log(id, 0, usize::MAX).unwrap().text);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task {id} still running");
}

async fn plan(e: &Env, id: &str, body: Value) -> AppInstallPlan {
    let (s, b) = call(
        e,
        "POST",
        &format!("/v1/apps/catalog/{id}/plan"),
        Some(body),
    )
    .await;
    json_of(s, &b)
}

/// Installs an app and waits for the task; returns the answer and the task log.
async fn install(e: &Env, id: &str, body: Value) -> (AppInstallStarted, String) {
    let (s, b) = call(
        e,
        "POST",
        &format!("/v1/apps/catalog/{id}/install"),
        Some(body),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let started: AppInstallStarted = serde_json::from_slice(&b).unwrap();
    let (info, log) = wait_task(e, &started.task_id).await;
    assert_eq!(info.status, TaskStatus::Ok, "{log}");
    (started, log)
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let t = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &t);
        } else {
            std::fs::copy(e.path(), t).unwrap();
        }
    }
}

#[tokio::test]
async fn catalogue_source_without_a_key() {
    let e = env();
    let (s, b) = call(&e, "GET", "/v1/apps/catalog-source", None).await;
    let src: CatalogSourceState = json_of(s, &b);
    assert!(!src.online_enabled);
    assert_eq!(src.disabled_reason.as_deref(), Some("no_key"));
    assert_eq!(src.shipped_serial, Some(1));
    assert_eq!(src.online_serial, None);
    assert_eq!(src.url, rynasd::apps::online::DEFAULT_URL);
    let (s, b) = call(&e, "POST", "/v1/apps/catalog-source/check", None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{}", String::from_utf8_lossy(&b));
}

#[tokio::test]
async fn downloaded_catalogue_is_merged() {
    use rynas_apps::online::test_signer::TestKey;
    let e = env();
    // a catalogue downloaded earlier: serial 7 (above the shipped 1), a newer Uptime Kuma
    let dir = e.dir.path().join("catalog");
    let dl = dir.join("7");
    std::fs::create_dir_all(&dl).unwrap();
    std::fs::write(dl.join("catalog.toml"), "serial = 7\n").unwrap();
    copy_dir(&catalogue().join("uptime-kuma"), &dl.join("uptime-kuma"));
    let app_toml = dl.join("uptime-kuma/app.toml");
    // a version above any the shipped package will reach (Renovate bumps it)
    let t: Vec<String> = std::fs::read_to_string(&app_toml)
        .unwrap()
        .lines()
        .map(|l| {
            if l.starts_with("version = ") {
                "version = \"99.0.0\"".to_string()
            } else {
                l.to_string()
            }
        })
        .collect();
    std::fs::write(&app_toml, t.join("\n") + "\n").unwrap();
    let index = json!({
        "schema": 1, "serial": 7, "published": "2026-11-10T00:00:00Z", "expires": "2099-01-01T00:00:00Z",
        "archive": {"url": "apps-7.tar.zst", "sha256": "0".repeat(64), "size": 1},
        "apps": [{"id": "uptime-kuma", "version": "99.0.0", "min_rynas": "0.1.0"}]
    });
    std::fs::write(dl.join("index.json"), index.to_string()).unwrap();
    std::os::unix::fs::symlink("7", dir.join("current")).unwrap();
    let key = TestKey::new(5).public();
    let _ = e
        .apps
        .online
        .set(rynasd::apps::online::Online::new_for_tests(dir, &[&key]));

    let (s, b) = call(&e, "GET", "/v1/apps/catalog", None).await;
    let c: AppCatalog = json_of(s, &b);
    assert!(c.issues.is_empty(), "{:?}", c.issues);
    assert_eq!(c.apps.len(), 27);
    let uk = c.apps.iter().find(|a| a.id == "uptime-kuma").unwrap();
    assert_eq!(uk.version, "99.0.0");
    let (s, b) = call(&e, "GET", "/v1/apps/catalog-source", None).await;
    let src: CatalogSourceState = json_of(s, &b);
    assert!(src.online_enabled);
    assert_eq!(
        (src.online_serial, src.online_apps, src.expired),
        (Some(7), 1, false)
    );
    assert_eq!(src.published.as_deref(), Some("2026-11-10T00:00:00Z"));
    // the icon of the merged entry is served from the download
    let (s, _) = call(&e, "GET", "/v1/apps/catalog/uptime-kuma/icon", None).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn catalogue_cards_detail_and_images() {
    let e = env();
    let (s, _) = call_as(&e, "bob", "GET", "/v1/apps/catalog", None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "reads need an administrator too");
    let (s, b) = call(&e, "GET", "/v1/apps/catalog", None).await;
    let c: AppCatalog = json_of(s, &b);
    assert!(c.issues.is_empty(), "{:?}", c.issues);
    assert_eq!(c.apps.len(), 27);
    assert_eq!(c.installed_ram_mb, Some(3808));
    assert!(c.docker_installed);
    assert_eq!(c.stacks_dir.as_deref(), Some("/vol1/docker/stacks"));
    let card = |id: &str| c.apps.iter().find(|a| a.id == id).unwrap().clone();
    let vw = card("vaultwarden");
    assert_eq!(vw.ram, RamGate::Ok);
    assert_eq!(vw.categories, vec![AppCategory::Security]);
    assert_eq!(vw.antifeatures, vec![AppAntifeature::RequiresHttps]);
    assert_eq!(
        (vw.ports[0].env.as_str(), vw.ports[0].default),
        ("WEB_PORT", 8222)
    );
    assert!(vw.installed.is_empty() && !vw.update_available);
    assert_eq!(vw.icon, "/api/v1/apps/catalog/vaultwarden/icon");
    assert!(matches!(
        card("immich").ram,
        RamGate::BelowRecommended {
            installed_mb: 3808,
            recommended_mb: 8192
        }
    ));
    assert!(card("arcane").manager);
    assert!(vw.featured && !card("arcane").featured);
    assert!(card("nginx-proxy-manager").reverse_proxy && !vw.reverse_proxy);

    let (s, b) = call(&e, "GET", "/v1/apps/catalog/paperless-ngx", None).await;
    let d: AppDetail = json_of(s, &b);
    assert!(d.description.len() > 40);
    assert_eq!(d.form.groups.len(), 2);
    let pw = &d.form.properties["PAPERLESS_ADMIN_PASSWORD"];
    assert_eq!(pw.min_length, Some(10));
    assert_eq!(
        pw.ui.as_ref().and_then(|u| u.widget),
        Some(FormWidget::Password)
    );
    assert_eq!(d.web.as_ref().map(|w| w.port.as_str()), Some("web"));
    assert_eq!(d.shares[0].env, "CONSUME");
    assert_eq!(d.secrets.len(), 2);
    assert!(!d.folders.is_empty());
    let (s, _) = call(&e, "GET", "/v1/apps/catalog/nope", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // the icon: typed, bounded, cacheable; no path reaches the filesystem
    let req = Request::builder()
        .uri("/v1/apps/catalog/vaultwarden/icon")
        .header("x-rynas-user", "admin")
        .body(Body::empty())
        .unwrap();
    let r = e.app.clone().oneshot(req).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()[header::CONTENT_TYPE], "image/svg+xml");
    assert_eq!(r.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    let etag = r.headers()[header::ETAG].clone();
    assert!(
        to_bytes(r.into_body(), 1 << 20)
            .await
            .unwrap()
            .starts_with(b"<svg")
    );
    let req = Request::builder()
        .uri("/v1/apps/catalog/vaultwarden/icon")
        .header("x-rynas-user", "admin")
        .header(header::IF_NONE_MATCH, etag)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        e.app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::NOT_MODIFIED
    );
    for uri in [
        "/v1/apps/catalog/vaultwarden/screenshots/..%2Fapp.toml",
        "/v1/apps/catalog/vaultwarden/screenshots/app.toml",
        "/v1/apps/catalog/..%2F..%2Fetc/icon",
    ] {
        let (s, _) = call(&e, "GET", uri, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn plan_errors_by_field_port_conflict_and_masked_secrets() {
    let e = env();
    // the default port is taken by a listening socket: a free one is suggested
    let p = plan(
        &e,
        "vaultwarden",
        json!({"form": {"DOMAIN": "https://vault.example.lan"}}),
    )
    .await;
    assert!(!p.ok);
    assert_eq!(p.port_conflicts.len(), 1, "{:?}", p.port_conflicts);
    let c = &p.port_conflicts[0];
    assert_eq!(
        (c.env.as_str(), c.port, c.suggestion),
        ("WEB_PORT", 8222, Some(8223))
    );
    assert!(p.warnings.contains(&PlanWarning::EngineUnreachable));
    let (s, b) = call(
        &e,
        "POST",
        "/v1/apps/catalog/vaultwarden/install",
        Some(json!({"form": {"DOMAIN": "https://vault.example.lan"}})),
    )
    .await;
    assert_eq!(
        (s, err(&b).code.as_str(), err(&b).field.as_deref()),
        (
            StatusCode::CONFLICT,
            error_codes::CONFLICT,
            Some("WEB_PORT")
        )
    );
    let p = plan(
        &e,
        "vaultwarden",
        json!({"form": {"DOMAIN": "https://vault.example.lan"}, "ports": {"WEB_PORT": 8223}, "publish_lan": false}),
    )
    .await;
    assert!(p.ok, "{p:?}");
    assert_eq!(p.stack_dir, "/vol1/docker/stacks/vaultwarden");
    assert_eq!(p.ports[0].bind.as_deref(), Some("127.0.0.1"));
    assert!(
        p.compose.contains("x-rynas:") && p.compose.contains("${RYNAS_BIND}:${WEB_PORT}:80"),
        "{}",
        p.compose
    );
    assert!(p.env.contains("WEB_PORT=8223") && p.env.contains("RYNAS_BIND=127.0.0.1"));
    assert_eq!(p.folders[0].path, "/vol1/docker/stacks/vaultwarden/data");
    assert!(!p.folders[0].exists);
    assert_eq!(
        p.folders[0].suggested.as_deref(),
        Some("/ssd/docker/appdata/vaultwarden/data")
    );
    // published on the LAN on request
    let p = plan(&e, "vaultwarden", json!({"form": {"DOMAIN": "https://vault.example.lan"}, "ports": {"WEB_PORT": 8223}, "publish_lan": true})).await;
    assert!(
        p.env.contains("RYNAS_BIND=0.0.0.0") && p.warnings.contains(&PlanWarning::PublishedOnLan)
    );

    // every problem at once, by field
    let p = plan(
        &e,
        "paperless-ngx",
        json!({"form": {"PAPERLESS_URL": "ftp://x", "NOPE": 1}, "shares": {"CONSUME": "/vol9/scans"}, "ports": {"WEB_PORT": 0}, "stack": "Bad Name"}),
    )
    .await;
    assert!(!p.ok);
    let has = |f: &str, k: FieldErrorKind| p.errors.iter().any(|x| x.field == f && x.kind == k);
    assert!(
        has("PAPERLESS_ADMIN_PASSWORD", FieldErrorKind::MissingValue),
        "{:?}",
        p.errors
    );
    assert!(
        has("PAPERLESS_URL", FieldErrorKind::InvalidValue),
        "{:?}",
        p.errors
    );
    assert!(has("NOPE", FieldErrorKind::UnknownValue), "{:?}", p.errors);
    assert!(has("CONSUME", FieldErrorKind::BadPath), "{:?}", p.errors);
    assert!(has("WEB_PORT", FieldErrorKind::BadPort), "{:?}", p.errors);
    assert!(has("stack", FieldErrorKind::BadStack), "{:?}", p.errors);
    assert!(p.compose.is_empty());
    let (s, b) = call(
        &e,
        "POST",
        "/v1/apps/catalog/paperless-ngx/install",
        Some(json!({"form": {"PAPERLESS_ADMIN_PASSWORD": "short"}})),
    )
    .await;
    assert_eq!(
        (s, err(&b).field.as_deref()),
        (StatusCode::BAD_REQUEST, Some("PAPERLESS_ADMIN_PASSWORD"))
    );

    // a valid plan: generated secrets and the admin password never in clear
    let p = plan(&e, "paperless-ngx", json!({"form": {"PAPERLESS_ADMIN_PASSWORD": "a-long-password"}, "shares": {"CONSUME": "/vol1/media"}})).await;
    assert!(p.ok, "{p:?}");
    assert!(
        p.env.contains("DB_PASSWORD=********") && p.env.contains("SECRET_KEY=********"),
        "{}",
        p.env
    );
    assert!(!p.compose.contains("a-long-password"));
    assert!(
        p.folders
            .iter()
            .any(|f| f.share && f.path == "/vol1/media" && f.exists)
    );
    assert!(p.ram == RamGate::Ok || matches!(p.ram, RamGate::BelowRecommended { .. }));
}

#[tokio::test]
async fn install_update_convert_and_uninstall_as_dry_runs() {
    let e = env();
    let (started, log) = install(
        &e,
        "vaultwarden",
        json!({"form": {"DOMAIN": "https://vault.example.lan"}, "ports": {"WEB_PORT": 8223}, "publish_lan": false}),
    )
    .await;
    assert_eq!(started.stack, "vaultwarden");
    assert!(started.secrets.is_empty(), "nothing marked to show");
    assert!(
        log.contains("up --detach") && log.contains("(dry system: not run)"),
        "{log}"
    );
    let dir = e.stack("vaultwarden");
    let compose = std::fs::read_to_string(dir.join("compose.yaml")).unwrap();
    assert!(
        compose.contains("x-rynas:") && compose.contains("app: vaultwarden"),
        "{compose}"
    );
    let envf = std::fs::read_to_string(dir.join(".env")).unwrap();
    assert!(
        envf.contains("WEB_PORT=8223") && envf.contains("RYNAS_BIND=127.0.0.1"),
        "{envf}"
    );
    assert_eq!(
        std::fs::metadata(dir.join(".env"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(dir.join("data").is_dir());
    // the folder exists now: a second install is refused
    let (s, b) = call(
        &e,
        "POST",
        "/v1/apps/catalog/vaultwarden/install",
        Some(json!({"form": {"DOMAIN": "https://vault.example.lan"}, "ports": {"WEB_PORT": 8223}})),
    )
    .await;
    assert_eq!(
        (s, err(&b).code.as_str()),
        (StatusCode::CONFLICT, error_codes::ALREADY_EXISTS)
    );

    let (s, b) = call(&e, "GET", "/v1/apps/installed", None).await;
    let list: Vec<InstalledApp> = json_of(s, &b);
    assert_eq!(list.len(), 1);
    let i = &list[0];
    assert_eq!(
        (
            i.app.as_str(),
            i.version.as_str(),
            i.web_port,
            i.bind.as_deref()
        ),
        ("vaultwarden", "1.0.0", Some(8223), Some("127.0.0.1"))
    );
    assert!(i.in_catalog && !i.update_available && i.state.is_none() && !i.snapshots);
    let (s, b) = call(&e, "GET", "/v1/apps/catalog", None).await;
    let c: AppCatalog = json_of(s, &b);
    assert_eq!(
        c.apps
            .iter()
            .find(|a| a.id == "vaultwarden")
            .unwrap()
            .installed,
        vec!["vaultwarden".to_string()]
    );

    // up to date: nothing to do
    let (s, b) = call(&e, "GET", "/v1/apps/installed/vaultwarden/update", None).await;
    let u: AppUpdatePlan = json_of(s, &b);
    assert!(u.up_to_date && !u.snapshot);
    let (s, _) = call(&e, "POST", "/v1/apps/installed/vaultwarden/update", None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // an older install with another image digest
    let at = compose.find("@sha256:").unwrap() + 8;
    let flip = if &compose[at..at + 1] == "0" {
        "1"
    } else {
        "0"
    };
    let old = format!("{}{flip}{}", &compose[..at], &compose[at + 1..])
        .replace("version: 1.0.0", "version: 0.9.0");
    std::fs::write(dir.join("compose.yaml"), &old).unwrap();
    let (s, b) = call(&e, "GET", "/v1/apps/installed/vaultwarden/update", None).await;
    let u: AppUpdatePlan = json_of(s, &b);
    assert!(!u.up_to_date && !u.downgrade, "{u:?}");
    assert_eq!(
        (u.from_version.as_str(), u.to_version.as_str()),
        ("0.9.0", "1.0.0")
    );
    assert_eq!(u.image_changes.len(), 1);
    assert_eq!(u.image_changes[0].kind, ImageChangeKind::Digest);
    let (s, b) = call(&e, "GET", "/v1/apps/installed", None).await;
    assert!(json_of::<Vec<InstalledApp>>(s, &b)[0].update_available);
    let (s, b) = call(
        &e,
        "POST",
        "/v1/apps/installed/vaultwarden/update",
        Some(json!({})),
    )
    .await;
    let t: TaskStarted = json_of(s, &b);
    let (info, log) = wait_task(&e, &t.task_id).await;
    assert_eq!(info.status, TaskStatus::Ok, "{log}");
    assert!(
        log.contains("no snapshot") && log.contains(" pull --quiet") && log.contains("up --detach"),
        "{log}"
    );
    let now = std::fs::read_to_string(dir.join("compose.yaml")).unwrap();
    assert!(now.contains("version: 1.0.0"));
    assert_eq!(
        std::fs::read_to_string(dir.join(".env"))
            .unwrap()
            .lines()
            .find(|l| l.starts_with("WEB_PORT")),
        Some("WEB_PORT=8223"),
        "port kept"
    );

    // convert: an ordinary stack, no longer an app
    let (s, _) = call_as(
        &e,
        "bob",
        "POST",
        "/v1/apps/installed/vaultwarden/convert",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, b) = call(&e, "POST", "/v1/apps/installed/vaultwarden/convert", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{}", String::from_utf8_lossy(&b));
    let now = std::fs::read_to_string(dir.join("compose.yaml")).unwrap();
    assert!(
        !now.contains("x-rynas") && now.contains("vaultwarden"),
        "{now}"
    );
    let (s, b) = call(&e, "GET", "/v1/apps/installed", None).await;
    assert!(json_of::<Vec<InstalledApp>>(s, &b).is_empty());
    let (s, _) = call(&e, "GET", "/v1/apps/installed/vaultwarden", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // uninstall keeps the data by default; deleting needs the confirmation
    install(&e, "uptime-kuma", json!({})).await;
    let (s, b) = call(
        &e,
        "POST",
        "/v1/apps/installed/uptime-kuma/uninstall",
        Some(json!({"delete_data": true})),
    )
    .await;
    assert_eq!(
        (s, err(&b).code.as_str()),
        (StatusCode::BAD_REQUEST, error_codes::CONFIRMATION_REQUIRED)
    );
    let (s, b) = call(&e, "POST", "/v1/apps/installed/uptime-kuma/uninstall", None).await;
    let t: TaskStarted = json_of(s, &b);
    let (info, log) = wait_task(&e, &t.task_id).await;
    assert_eq!(info.status, TaskStatus::Ok, "{log}");
    assert!(
        log.contains(" down (dry system: not run)") && log.contains("stays"),
        "{log}"
    );
    assert!(e.stack("uptime-kuma").join("compose.yaml").is_file());
    assert!(e.stack("uptime-kuma").join(".env").is_file());
}

// ---------------------------------------------------------------------------------------------
// web access (W1)
// ---------------------------------------------------------------------------------------------

fn web_routes(e: &Env) -> Vec<rynas_api_types::web_access::WebRoute> {
    e.ops
        .config
        .applied(rynasd::web_access::WEB_ACCESS_KEY)
        .unwrap()
        .map(|v| serde_json::from_value::<rynas_api_types::web_access::WebAccessConfig>(v).unwrap().routes)
        .unwrap_or_default()
}

const VW: &str = r#"{"form": {"DOMAIN": "https://vault.example.lan"}, "ports": {"WEB_PORT": 8223}}"#;

fn vw() -> Value {
    serde_json::from_str(VW).unwrap()
}

#[tokio::test]
async fn default_binding_follows_the_proxy_state() {
    // web access present, the proxy not installed: the LAN, as before W1
    let e = env_opts(Some(false));
    let p = plan(&e, "vaultwarden", vw()).await;
    assert!(!p.proxy_active && p.web_url.is_none());
    let web = p.ports.iter().find(|x| x.proxied).unwrap();
    assert_eq!(web.bind.as_deref(), Some("0.0.0.0"));
    assert!(p.warnings.contains(&PlanWarning::PublishedOnLan));
    // loopback asked for explicitly while the proxy is not active: warned
    let mut body = vw();
    body["publish_lan"] = json!(false);
    let p = plan(&e, "vaultwarden", body).await;
    assert!(p.warnings.contains(&PlanWarning::ProxyInactive));
    assert_eq!(p.ports.iter().find(|x| x.proxied).unwrap().bind.as_deref(), Some("127.0.0.1"));

    // the proxy active: loopback plus a route
    let e = env_opts(Some(true));
    let p = plan(&e, "vaultwarden", vw()).await;
    assert!(p.proxy_active);
    assert_eq!(p.web_url.as_deref(), Some("https://vaultwarden-nas.local/"));
    assert_eq!(p.ports.iter().find(|x| x.proxied).unwrap().bind.as_deref(), Some("127.0.0.1"));
    assert!(!p.warnings.contains(&PlanWarning::PublishedOnLan));
    let (_, log) = install(&e, "vaultwarden", vw()).await;
    assert!(log.contains("https://vaultwarden-nas.local/"), "{log}");
    let envf = std::fs::read_to_string(e.stack("vaultwarden").join(".env")).unwrap();
    assert!(envf.contains("RYNAS_BIND=127.0.0.1"), "{envf}");
    let routes = web_routes(&e);
    assert_eq!(routes.len(), 1);
    let r = &routes[0];
    assert_eq!((r.id.as_str(), r.label.as_str(), r.upstream.port), ("app-vaultwarden", "vaultwarden", 8223));
    assert_eq!(r.limits.body.as_deref(), Some("525M"));
    assert!(r.websocket);
    // the proxy got the route
    let (_, caddy) = e.web.as_ref().unwrap();
    assert!(caddy.config().to_string().contains("vaultwarden-nas.local"));
    let (s, b) = call(&e, "GET", "/v1/apps/installed", None).await;
    let list: Vec<InstalledApp> = json_of(s, &b);
    assert_eq!(list[0].web_url.as_deref(), Some("https://vaultwarden-nas.local/"));

    // uninstall removes the route
    let (s, b) = call(&e, "POST", "/v1/apps/installed/vaultwarden/uninstall", None).await;
    let t: TaskStarted = json_of(s, &b);
    let (info, log) = wait_task(&e, &t.task_id).await;
    assert_eq!(info.status, TaskStatus::Ok, "{log}");
    assert!(web_routes(&e).is_empty(), "{log}");
    assert!(!caddy.config().to_string().contains("vaultwarden-nas.local"));
}

#[tokio::test]
async fn proxy_ports_count_as_used_unless_external_mode() {
    let e = env_opts(Some(false));
    let mut body = vw();
    body["ports"]["WEB_PORT"] = json!(443);
    let p = plan(&e, "vaultwarden", body.clone()).await;
    assert!(
        p.port_conflicts.iter().any(|c| c.port == 443),
        "{:?}",
        p.port_conflicts
    );
    assert!(!p.ok);
    // external proxy mode releases 80/443
    let mut c = rynas_api_types::web_access::WebAccessConfig::default();
    c.settings.mode = rynas_api_types::web_access::ProxyMode::External;
    e.ops
        .config
        .apply_value("admin", rynasd::web_access::WEB_ACCESS_KEY, Some(serde_json::to_value(c).unwrap()), "t", &|_| {})
        .unwrap();
    let p = plan(&e, "vaultwarden", body).await;
    assert!(p.port_conflicts.is_empty(), "{:?}", p.port_conflicts);
}

#[tokio::test]
async fn move_behind_the_proxy() {
    let e = env_opts(Some(true));
    let mut body = vw();
    body["publish_lan"] = json!(true);
    install(&e, "vaultwarden", body).await;
    assert!(web_routes(&e).is_empty());
    let (s, b) = call(&e, "POST", "/v1/apps/installed/vaultwarden/proxy", None).await;
    let t: TaskStarted = json_of(s, &b);
    let (info, log) = wait_task(&e, &t.task_id).await;
    assert_eq!(info.status, TaskStatus::Ok, "{log}");
    assert!(log.contains("up --detach"), "{log}");
    let envf = std::fs::read_to_string(e.stack("vaultwarden").join(".env")).unwrap();
    assert!(envf.contains("RYNAS_BIND=127.0.0.1") && !envf.contains("0.0.0.0"), "{envf}");
    assert_eq!(web_routes(&e).len(), 1);
    // already behind it
    let (s, b) = call(&e, "POST", "/v1/apps/installed/vaultwarden/proxy", None).await;
    assert_eq!((s, err(&b).code.as_str()), (StatusCode::CONFLICT, error_codes::CONFLICT));

    // without an active proxy the action is refused
    let e = env_opts(Some(false));
    let mut body = vw();
    body["publish_lan"] = json!(true);
    install(&e, "vaultwarden", body).await;
    let (s, b) = call(&e, "POST", "/v1/apps/installed/vaultwarden/proxy", None).await;
    assert_eq!((s, err(&b).code.as_str()), (StatusCode::CONFLICT, error_codes::CONFLICT));
}

/// Photos from the RyNAS app: an install binds the root-owned staging folder and mounts
/// nobody; an administrator chooses users (with an API: confirms the account), the choice is
/// kept, and the key is only for apps with an API.
#[tokio::test]
async fn user_photos_are_chosen_by_an_administrator() {
    use std::os::unix::fs::MetadataExt;
    let e = env();
    let root = e.root();
    for d in ["etc", "home/ann/Photos/2026/11", "home/bob"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    let me = std::fs::metadata(root.join("home/ann")).unwrap().uid();
    std::fs::write(root.join("etc/passwd"), format!("root:x:0:0::/root:/bin/sh\nann:x:{me}:100::/home/ann:/bin/sh\nbob:x:{me}:100::/home/bob:/bin/sh\n")).unwrap();
    let (_, log) = install(&e, "pigallery2", json!({})).await;
    assert!(log.contains("Photos folders: none yet"), "{log}");
    let staging = e.dir.path().join("state/docker/photos/pigallery2");
    assert!(staging.join("Shared").is_dir(), "the mount point of the shared folder");
    let dir = e.stack("pigallery2");
    let envf = std::fs::read_to_string(dir.join(".env")).unwrap();
    assert!(envf.contains(&format!("USER_PHOTOS={}\n", staging.display())), "{envf}");
    let compose = std::fs::read_to_string(dir.join("compose.yaml")).unwrap();
    assert!(compose.contains("target: /app/data/images\n") && compose.contains("propagation: rslave") && !compose.contains("/home/"), "{compose}");
    let (s, b) = call(&e, "GET", "/v1/apps/installed/pigallery2", None).await;
    assert!(json_of::<InstalledAppDetail>(s, &b).app.user_photos);
    let (s, b) = call(&e, "GET", "/v1/apps/installed/pigallery2/photos", None).await;
    let st: PhotosState = json_of(s, &b);
    let rows: Vec<(&str, bool, bool)> = st.users.iter().map(|u| (u.user.as_str(), u.included, u.mounted)).collect();
    assert_eq!(rows, [("ann", false, false), ("bob", false, false)], "nobody at install");
    assert_eq!((st.api, st.allow_list.as_deref()), (None, Some("/{user}/*")));
    // choose ann: kept, then synced (a dry system mounts nothing)
    let (s, b) = call(&e, "POST", "/v1/apps/installed/pigallery2/photos/users/ann", Some(json!({"include": true}))).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let t: TaskStarted = serde_json::from_slice(&b).unwrap();
    let (info, log) = wait_task(&e, &t.task_id).await;
    assert_eq!(info.status, TaskStatus::Ok, "{log}");
    assert!(log.contains("ann: dry system: not mounted") && !log.contains("bob"), "{log}");
    let kept = std::fs::read_to_string(e.dir.path().join("state/docker/photos/pigallery2.json")).unwrap();
    assert!(kept.contains("\"ann\"") && !kept.contains("bob"), "{kept}");
    let (_, b) = call(&e, "GET", "/v1/apps/installed/pigallery2/photos", None).await;
    let st: PhotosState = json_of(StatusCode::OK, &b);
    assert!(st.users[0].included && !st.users[1].included);
    // not a NAS user; leave ann out again
    let (s, b) = call(&e, "POST", "/v1/apps/installed/pigallery2/photos/users/zed", Some(json!({"include": true}))).await;
    assert_eq!((s, err(&b).field.as_deref()), (StatusCode::BAD_REQUEST, Some("user")));
    let (_, b) = call(&e, "POST", "/v1/apps/installed/pigallery2/photos/users/ann", Some(json!({"include": false}))).await;
    let t: TaskStarted = serde_json::from_slice(&b).unwrap();
    assert_eq!(wait_task(&e, &t.task_id).await.0.status, TaskStatus::Ok);
    assert!(!std::fs::read_to_string(e.dir.path().join("state/docker/photos/pigallery2.json")).unwrap().contains("ann"));
    // no key for an app without an API
    let (s, _) = call(&e, "POST", "/v1/apps/installed/pigallery2/photos/key", Some(json!({"key": "abc"}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // Immich: a user needs a confirmed account, which needs the app's API
    install(&e, "immich", json!({})).await;
    let (s, b) = call(&e, "POST", "/v1/apps/installed/immich/photos/users/ann", Some(json!({"include": true}))).await;
    assert_eq!((s, err(&b).field.as_deref()), (StatusCode::BAD_REQUEST, Some("account")));
    let (s, _) = call(&e, "POST", "/v1/apps/installed/immich/photos/users/ann", Some(json!({"include": true, "account": "u-ann"}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "no key yet");
    let (s, b) = call(&e, "GET", "/v1/apps/installed/immich/photos", None).await;
    let st: PhotosState = json_of(s, &b);
    assert_eq!(st.api, Some(PhotosApi { key: false, error: None }));
    // uninstall with the data: the staging folder and the choices go
    let (s, b) = call(&e, "POST", "/v1/apps/installed/pigallery2/uninstall", Some(json!({"delete_data": true, "confirm": "pigallery2"}))).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let t: TaskStarted = serde_json::from_slice(&b).unwrap();
    let (info, log) = wait_task(&e, &t.task_id).await;
    assert_eq!(info.status, TaskStatus::Ok, "{log}");
    assert!(!staging.exists() && !e.dir.path().join("state/docker/photos/pigallery2.json").exists());
    // an app that does not read the folders
    let (s, _) = call(&e, "GET", "/v1/apps/installed/nope/photos", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

/// An entry with `run_as`: plans offer the NAS users, refuse root, system accounts and the
/// apps' account, and an install writes the user's ids and name.
#[tokio::test]
async fn run_as_a_nas_user() {
    let e = env();
    std::fs::create_dir_all(e.root().join("etc")).unwrap();
    std::fs::write(
        e.root().join("etc/passwd"),
        "root:x:0:0::/root:/bin/sh\ndaemon:x:1:1::/usr/sbin:/usr/sbin/nologin\nann:x:1026:100::/home/ann:/bin/sh\n",
    )
    .unwrap();
    let p = plan(&e, "syncthing", json!({})).await;
    assert_eq!(p.run_as_choices, ["ann"]);
    for bad in ["root", "daemon", "zed", "docker-apps"] {
        let p = plan(&e, "syncthing", json!({"run_as": bad})).await;
        assert!(p.errors.iter().any(|x| x.field == "run_as"), "{bad}: {:?}", p.errors);
    }
    let p = plan(&e, "vaultwarden", json!({"form": {"DOMAIN": "https://vault.example.lan"}, "run_as": "ann"})).await;
    assert!(p.errors.iter().any(|x| x.field == "run_as" && x.kind == FieldErrorKind::UnknownValue), "{:?}", p.errors);
    let p = plan(&e, "syncthing", json!({"run_as": "ann"})).await;
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    assert_eq!(p.run_as.as_deref(), Some("ann"));
    assert!(p.env.contains("PUID=1026\n") && p.env.contains("PGID=100\n") && p.env.contains("RYNAS_RUN_AS=ann\n"), "{}", p.env);
    assert!(p.folders.iter().any(|f| f.env == "CONFIG_FOLDER" && f.owner == "1026:100"), "{:?}", p.folders);
    let (_, log) = install(&e, "syncthing", json!({"run_as": "ann", "publish_lan": true})).await;
    assert!(log.contains("the app runs as ann (1026:100)"), "{log}");
    let (s, b) = call(&e, "GET", "/v1/apps/installed/syncthing", None).await;
    let i: InstalledAppDetail = json_of(s, &b);
    assert_eq!(i.app.run_as.as_deref(), Some("ann"));
}

const ST_CONFIG: &str = r#"<configuration version="37">
    <folder id="photos-1" label="Photos" path="/data/photos" type="sendreceive">
        <device id="AAAAAAA-BBBBBBB-CCCCCCC-DDDDDDD" introducedBy=""></device>
        <paused>false</paused>
        <markerName>.stfolder</markerName>
    </folder>
    <folder id="docs" label="Docs" path="/data/docs" type="sendonly"><paused>false</paused></folder>
    <folder id="far" label="Far" path="/elsewhere" type="sendreceive"></folder>
    <device id="AAAAAAA-BBBBBBB-CCCCCCC-DDDDDDD" name="laptop"></device>
    <gui enabled="true"><address>0.0.0.0:8384</address><apikey>SECRETKEYSECRETKEY</apikey><password>pwhash</password></gui>
</configuration>
"#;

/// Share sets: an install with none, the installed list, a refused change; the older
/// Syncthing layout found by the adoption preview and adopted (update in place, dry run).
#[tokio::test]
async fn share_set_and_syncthing_adoption_of_the_older_layout() {
    let e = env();
    let (s, b) = call(&e, "GET", "/v1/apps/catalog/syncthing", None).await;
    let d: AppDetail = json_of(s, &b);
    let ss = d.share_set.expect("share set");
    assert_eq!((ss.env.as_str(), ss.target.as_str()), ("SYNC_SHARES", "/shares"));
    // a share that is not in the configuration
    let p = plan(&e, "syncthing", json!({"share_set": [{"share": "nope"}]})).await;
    assert!(p.errors.iter().any(|x| x.field == "SYNC_SHARES" && x.message.contains("no share nope")), "{:?}", p.errors);
    let (started, _) = install(&e, "syncthing", json!({"publish_lan": true})).await;
    assert_eq!(started.stack, "syncthing");
    let dir = e.stack("syncthing");
    let envf = std::fs::read_to_string(dir.join(".env")).unwrap();
    assert!(envf.contains("SYNC_SHARES=\n") && envf.contains("ST_API_KEY="), "{envf}");
    let compose = std::fs::read_to_string(dir.join("compose.yaml")).unwrap();
    assert!(compose.contains("STGUIAPIKEY: ${ST_API_KEY}") || compose.contains("STGUIAPIKEY: '${ST_API_KEY}'"), "{compose}");
    assert!(!compose.contains("/shares/"), "{compose}");
    let (s, b) = call(&e, "PUT", "/v1/apps/installed/syncthing/share-set", Some(json!({"items": [{"share": "nope"}]}))).await;
    assert_eq!((s, err(&b).field.as_deref()), (StatusCode::BAD_REQUEST, Some("items")));
    let (s, b) = call(&e, "PUT", "/v1/apps/installed/vaultwarden/share-set", Some(json!({"items": []}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{}", String::from_utf8_lossy(&b));
    // an empty set again is fine (rewrites the same files, up)
    let (s, b) = call(&e, "PUT", "/v1/apps/installed/syncthing/share-set", Some(json!({"items": []}))).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let t: TaskStarted = serde_json::from_slice(&b).unwrap();
    let (info, log) = wait_task(&e, &t.task_id).await;
    assert_eq!(info.status, TaskStatus::Ok, "{log}");
    assert!(log.contains("shared folders none") && log.contains("up --detach"), "{log}");

    // make it look like the older layout: version 1.0.0, one share at /data (the fallback)
    let compose = compose.replace("version: 2.0.0", "version: 1.0.0");
    std::fs::write(dir.join("compose.yaml"), compose).unwrap();
    let old_env: String = envf
        .lines()
        .filter(|l| !l.starts_with("SYNC_SHARES=") && !l.starts_with("ST_API_KEY="))
        .map(|l| format!("{l}\n"))
        .chain(std::iter::once("SYNC_SHARE=./sync\n".to_string()))
        .collect();
    std::fs::write(dir.join(".env"), old_env).unwrap();
    let home = dir.join("state/config");
    std::fs::create_dir_all(&home).unwrap();
    for f in ["cert.pem", "key.pem"] {
        std::fs::write(home.join(f), "x").unwrap();
    }
    std::fs::create_dir_all(home.join("index-v2")).unwrap();
    std::fs::write(home.join("config.xml"), ST_CONFIG).unwrap();
    // photos: verified marker and a .stignore with other systems' patterns; docs: a marker
    // folder without the file
    let photos = dir.join("sync/photos");
    std::fs::create_dir_all(photos.join(".stfolder")).unwrap();
    std::fs::write(photos.join(".stfolder").join(rynasd::syncthing::config::marker_file_name("photos-1")), "").unwrap();
    std::fs::write(photos.join(".stignore"), "@eaDir\n*.tmp\n").unwrap();
    std::fs::create_dir_all(dir.join("sync/docs/.stfolder")).unwrap();

    let (s, b) = call(&e, "POST", "/v1/syncthing/adopt/preview", None).await;
    let pv: rynas_api_types::syncthing::SyncthingAdoptPreview = json_of(s, &b);
    assert_eq!(pv.installed.as_deref(), Some("syncthing"));
    let c = pv.candidates.iter().find(|c| c.source.kind == rynas_api_types::syncthing::AdoptSourceKind::App).expect("the older layout");
    assert_eq!(c.source.id.as_deref(), Some("syncthing"));
    let req = json!({"source": {"kind": "app", "id": "syncthing"}});
    let (s, b) = call(&e, "POST", "/v1/syncthing/adopt/preview", Some(req.clone())).await;
    let text = String::from_utf8_lossy(&b).to_string();
    assert!(!text.contains("SECRETKEY") && !text.contains("pwhash") && !text.contains("BBBBBBB"), "{text}");
    let pv: rynas_api_types::syncthing::SyncthingAdoptPreview = json_of(s, &b);
    assert!(pv.ok, "{:?}", pv.problems);
    assert_eq!(pv.files.database, rynas_api_types::syncthing::AdoptDatabase::V2);
    assert_eq!(pv.devices[0].id_short, "AAAAAAA");
    assert_eq!(pv.shares, ["sync"]);
    let f = |id: &str| pv.folders.iter().find(|f| f.id == id).unwrap().clone();
    use rynas_api_types::syncthing::{MarkerState, UnmappedReason};
    let ph = f("photos-1");
    assert_eq!((ph.new_path.as_deref(), ph.marker, ph.will_pause), (Some("/shares/sync/photos"), MarkerState::Verified, false));
    assert_eq!(ph.stignore.as_ref().unwrap().after, "*.tmp\n(?d)#recycle\n");
    let dc = f("docs");
    assert_eq!((dc.marker, dc.will_pause), (MarkerState::Unverified, true));
    let far = f("far");
    assert_eq!((far.unmapped, far.will_pause), (Some(UnmappedReason::NoVolume), true));

    let (s, b) = call(&e, "POST", "/v1/syncthing/adopt", Some(json!({"source": {"kind": "app", "id": "syncthing"}, "apply_stignore": true}))).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let t: TaskStarted = serde_json::from_slice(&b).unwrap();
    let (info, log) = wait_task(&e, &t.task_id).await;
    assert_eq!(info.status, TaskStatus::Ok, "{log}");
    assert!(log.contains("from 1.0.0 to 2.0.0") && log.contains("dry system: not done"), "{log}");
    assert!(!log.contains("SECRETKEY"), "{log}");
    let envf = std::fs::read_to_string(dir.join(".env")).unwrap();
    assert!(envf.contains("SYNC_SHARES=sync=./sync\n") && !envf.contains("SYNC_SHARE="), "{envf}");
    let compose = std::fs::read_to_string(dir.join("compose.yaml")).unwrap();
    assert!(compose.contains("target: /shares/sync") && compose.contains("source: ./sync"), "{compose}");
    // the old home is never changed
    assert_eq!(std::fs::read_to_string(home.join("config.xml")).unwrap(), ST_CONFIG);
    // dry system: the .stignore is not written
    assert_eq!(std::fs::read_to_string(photos.join(".stignore")).unwrap(), "@eaDir\n*.tmp\n");
    let (s, b) = call(&e, "GET", "/v1/apps/installed/syncthing", None).await;
    let i: InstalledAppDetail = json_of(s, &b);
    assert_eq!(i.app.share_set.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["sync"]);
    // the update itself moved the folders (the step a plain update from 1.x runs too)
    assert!(log.contains("moving the Syncthing folders from /data to /shares/sync (dry system: not done)"), "{log}");

    // a home folder given by path: the preview always warns that rynas cannot stop its
    // instance; volume lines are checked
    let path_src = json!({"kind": "path", "path": "/vol1/docker/stacks/syncthing/state/config"});
    let (s, b) = call(&e, "POST", "/v1/syncthing/adopt/preview", Some(json!({"source": path_src, "volumes": [{"container": "/data", "host": "/vol1/../etc"}]}))).await;
    assert_eq!((s, err(&b).field.as_deref()), (StatusCode::BAD_REQUEST, Some("volumes")));
    let (s, b) = call(&e, "POST", "/v1/syncthing/adopt/preview", Some(json!({"source": path_src}))).await;
    let pv: rynas_api_types::syncthing::SyncthingAdoptPreview = json_of(s, &b);
    assert!(pv.problems.iter().any(|m| m.starts_with("info: ") && m.contains("stopped for good")), "{:?}", pv.problems);
    // two Syncthing stacks: an adoption refuses (one device id runs in one place)
    let (started, _) = install(&e, "syncthing", json!({"stack": "st2", "publish_lan": true})).await;
    assert_eq!(started.stack, "st2");
    let (s, b) = call(&e, "POST", "/v1/syncthing/adopt", Some(json!({"source": path_src}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{}", String::from_utf8_lossy(&b));
    assert!(err(&b).message.contains("other Syncthing stacks"), "{}", String::from_utf8_lossy(&b));
}

/// The `(method, path)` pairs of the `.route(…)` calls of a router's source: new routes
/// are covered without editing this test.
fn routes_of(src: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(i) = rest.find(".route(") {
        rest = rest[i + 7..].trim_start();
        let Some(r) = rest.strip_prefix('"') else {
            continue;
        };
        rest = r;
        let Some(e) = rest.find('"') else { break };
        let path = &rest[..e];
        // the methods up to the closing parenthesis of `.route(`
        let mut depth = 1;
        let mut end = rest.len();
        for (k, c) in rest.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = k;
                        break;
                    }
                }
                _ => {}
            }
        }
        let call = &rest[e..end];
        let p: String = path
            .split('/')
            .map(|s| if s.starts_with('{') { "x1" } else { s })
            .collect::<Vec<_>>()
            .join("/");
        for (m, name) in [
            ("GET", "get("),
            ("POST", "post("),
            ("PUT", "put("),
            ("DELETE", "delete("),
        ] {
            if call.contains(name) {
                out.push((m.to_string(), p.clone()));
            }
        }
    }
    out
}

/// Review 2026-11-07, H3/M1: a non-administrator gets 403 on every route of Docker, the
/// app catalogue, web access and the HTTPS certificate, reads included; the routes come
/// from the routers' sources.
#[tokio::test]
async fn non_admins_get_403_on_every_route() {
    let e = env_opts(Some(true));
    let mut n = 0;
    for src in [
        include_str!("../src/docker.rs"),
        include_str!("../src/apps.rs"),
        include_str!("../src/tls.rs"),
        include_str!("../src/web_access/mod.rs"),
        include_str!("../src/syncthing/mod.rs"),
        // the router only: its tests serve a fake app's API
        include_str!("../src/photos.rs").split("#[cfg(test)]").next().unwrap_or_default(),
    ] {
        let routes = routes_of(src);
        assert!(routes.len() >= 2, "routes not found");
        for (m, p) in routes {
            let (s, b) = call_as(&e, "bob", &m, &p, Some(json!({}))).await;
            assert_eq!(
                (s, err(&b).code.as_str()),
                (StatusCode::FORBIDDEN, error_codes::FORBIDDEN),
                "{m} {p}"
            );
            n += 1;
        }
    }
    assert!(n > 50, "{n} routes checked");
}
