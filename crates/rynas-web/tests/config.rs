//! Configuration and task endpoints end to end: rynas-web (session, CSRF, proxy) → a real
//! rynasd router on a Unix socket → rynas-config on a scratch root with the in-memory
//! system, and rynas-tasks.

mod common;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use common::{Creds, FakeAuth, TestApp, build_with_rynasd};
use futures_util::StreamExt;
use rynas_api_types::{
    DriftReport, HostnameState, PendingChanges, RevisionDetail, RevisionSummary, StagedChanges,
    TaskEvent, TaskEventKind, TaskInfo, TaskStarted, TaskStatus, error_codes,
};
use rynas_auth::ManualClock;
use rynas_auth::totp::{MemoryStorage, TotpManager};
use rynas_config::{Config, ConfigOptions, FileDomain, HostnameDomain, MemorySystem};
use rynas_tasks::{TaskConfig, TaskManager};
use rynas_web::hardware::NoProvider;
use rynasd::ops::Ops;
use rynasd::server::{DaemonState, PeerCheckedListener, router};
use serde_json::json;
use tempfile::TempDir;
use tower::ServiceExt;

struct Rig {
    app: TestApp,
    sys: Arc<MemorySystem>,
    root: PathBuf,
    _dir: TempDir,
}

async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("etc")).unwrap();
    std::fs::write(root.join("etc/hostname"), "debian\n").unwrap();
    std::fs::write(
        root.join("etc/hosts"),
        "127.0.0.1\tlocalhost\n127.0.1.1\tdebian\n",
    )
    .unwrap();
    let sys = Arc::new(MemorySystem::new("debian"));
    let state_dir = dir.path().join("state");
    let config = Config::open(
        ConfigOptions::new(&state_dir, &root),
        sys.clone(),
        vec![
            Arc::new(HostnameDomain),
            Arc::new(FileDomain::new(
                "test.app",
                "/etc/app.conf",
                "x = {{ x }}\n",
            )),
        ],
    )
    .unwrap();
    let tasks =
        TaskManager::new(state_dir.join("tasks"), "testnas", TaskConfig::default()).unwrap();
    let clock = Arc::new(ManualClock::new(common::T0_MS));
    let totp = Arc::new(TotpManager::new(
        MemoryStorage::default(),
        clock.clone(),
        "rynas",
    ));
    let mut st = DaemonState::new(Arc::new(FakeAuth), totp, clock);
    st.ops = Some(Arc::new(Ops::new(Arc::new(config), tasks)));
    let socket = dir.path().join("rynasd.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    // SAFETY: getuid cannot fail and has no preconditions.
    let me = unsafe { libc::getuid() };
    let allowed: HashSet<u32> = [me].into();
    tokio::spawn(async move {
        axum::serve(PeerCheckedListener::new(listener, allowed), router(st))
            .await
            .unwrap();
    });
    let app = build_with_rynasd(Arc::new(NoProvider), Arc::new(FakeAuth), socket);
    Rig {
        app,
        sys,
        root,
        _dir: dir,
    }
}

fn read(root: &Path, p: &str) -> String {
    std::fs::read_to_string(root.join(p)).unwrap()
}

async fn wait_task(app: &TestApp, c: &Creds, id: &str) -> TaskInfo {
    for _ in 0..200 {
        let r = app.get(&format!("/api/v1/tasks/{id}"), c).await;
        assert_eq!(
            r.status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&r.body)
        );
        let t: TaskInfo = r.json();
        if t.status.is_finished() {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task {id} did not finish");
}

async fn apply(app: &TestApp, c: &Creds, comment: &str) -> TaskInfo {
    let r = app
        .send(
            Method::POST,
            "/api/v1/config/apply",
            c,
            Some(json!({ "comment": comment })),
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::ACCEPTED,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    let started: TaskStarted = r.json();
    wait_task(app, c, &started.task_id).await
}

#[tokio::test]
async fn hostname_stage_apply_log_revision_restore() {
    let rig = rig().await;
    let app = &rig.app;
    let c = app.login().await;

    let r = app.get("/api/v1/system/hostname", &c).await;
    assert_eq!(r.status, StatusCode::OK);
    let h: HostnameState = r.json();
    assert_eq!(h.applied.hostname, "debian");
    assert_eq!(h.running, "debian");
    assert_eq!(h.pending, None);

    // CSRF still applies to forwarded endpoints
    let no_csrf = Creds {
        cookie: c.cookie.clone(),
        csrf: None,
    };
    let body = json!({ "hostname": "nas" });
    let r = app
        .send(
            Method::PUT,
            "/api/v1/system/hostname",
            &no_csrf,
            Some(body.clone()),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = app
        .send(
            Method::PUT,
            "/api/v1/system/hostname",
            &c,
            Some(json!({ "hostname": "-x" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.code(), error_codes::INVALID_VALUE);
    // the form field it is about travels from rynasd through rynas-web (A10)
    let e: rynas_api_types::ApiError = r.json();
    assert_eq!(e.field.as_deref(), Some("hostname"));
    let r = app
        .send(Method::PUT, "/api/v1/system/hostname", &c, Some(body))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json::<HostnameState>().pending.unwrap().hostname, "nas");

    // compact by default (what the bar polls), documents and diff with ?detail=1
    let p: PendingChanges = app.get("/api/v1/config/pending", &c).await.json();
    assert_eq!(p.owner, "admin");
    assert_eq!(p.changes.len(), 1);
    assert_eq!(p.changes[0].diff_count, 1);
    assert!(p.changes[0].diff.is_empty());
    assert!(p.changes[0].base.is_none() && p.changes[0].value.is_none());
    let p: PendingChanges = app.get("/api/v1/config/pending?detail=1", &c).await.json();
    assert_eq!(p.changes[0].diff[0].path, "system.hostname.hostname");
    assert_eq!(p.changes[0].value, Some(json!({ "hostname": "nas" })));

    let t = apply(app, &c, "rename").await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    assert_eq!(t.kind, "config.apply");
    assert_eq!(t.user, "admin");
    assert_eq!(rig.sys.state().hostname, "nas");
    assert_eq!(read(&rig.root, "etc/hostname"), "nas\n");

    // the log, polled by offset
    let r = app
        .get(&format!("/api/v1/tasks/{}/log?offset=0", t.id), &c)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/plain")
    );
    let text = String::from_utf8(r.body.clone()).unwrap();
    assert!(text.contains("revision 1 applied"), "{text}");
    assert!(text.ends_with("TASK OK\n"), "{text}");
    assert_eq!(r.headers["x-rynas-log-complete"], "true");
    let next = r.headers["x-rynas-next-offset"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(next, text.len().to_string());
    let r = app
        .get(&format!("/api/v1/tasks/{}/log?offset={next}", t.id), &c)
        .await;
    assert!(r.body.is_empty());

    let revs: Vec<RevisionSummary> = app.get("/api/v1/config/revisions", &c).await.json();
    assert_eq!(revs.len(), 1);
    assert_eq!((revs[0].id, revs[0].author.as_str()), (1, "admin"));
    assert_eq!(revs[0].comment, "rename");
    let d: RevisionDetail = app.get("/api/v1/config/revisions/1", &c).await.json();
    assert_eq!(d.diff[0].new, Some(json!("nas")));
    assert_eq!(
        app.get("/api/v1/config/revisions/7", &c).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.get("/api/v1/config/revisions/x", &c).await.status,
        StatusCode::BAD_REQUEST
    );

    // nothing left to apply
    let r = app
        .send(Method::POST, "/api/v1/config/apply", &c, Some(json!({})))
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(r.code(), error_codes::NOTHING_PENDING);

    // rename again, then restore revision 1
    app.send(
        Method::PUT,
        "/api/v1/system/hostname",
        &c,
        Some(json!({ "hostname": "nas2" })),
    )
    .await;
    assert_eq!(apply(app, &c, "").await.status, TaskStatus::Ok);
    let r = app
        .send(Method::POST, "/api/v1/config/revisions/1/restore", &c, None)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json::<StagedChanges>().staged, 1);
    let h: HostnameState = app.get("/api/v1/system/hostname", &c).await.json();
    assert_eq!(h.applied.hostname, "nas2");
    assert_eq!(h.pending.unwrap().hostname, "nas");
    assert_eq!(apply(app, &c, "restore 1").await.status, TaskStatus::Ok);
    let revs: Vec<RevisionSummary> = app.get("/api/v1/config/revisions", &c).await.json();
    assert_eq!(revs[0].restored_from, Some(1));
    assert_eq!(rig.sys.state().hostname, "nas");

    // tasks list and cancel of a finished task
    let list: Vec<TaskInfo> = app.get("/api/v1/tasks", &c).await.json();
    assert_eq!(list.len(), 3);
    let r = app
        .send(
            Method::POST,
            &format!("/api/v1/tasks/{}/cancel", list[0].id),
            &c,
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(r.code(), error_codes::TASK_FINISHED);
    assert_eq!(
        app.get("/api/v1/tasks/UPID:nope", &c).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn drift_import_discard_and_overwrite() {
    let rig = rig().await;
    let app = &rig.app;
    let c = app.login().await;
    app.send(
        Method::PUT,
        "/api/v1/system/hostname",
        &c,
        Some(json!({ "hostname": "nas" })),
    )
    .await;
    apply(app, &c, "").await;
    let d: DriftReport = app.get("/api/v1/config/drift", &c).await.json();
    assert_eq!((d.checked, d.entries.len()), (2, 0));

    std::fs::write(rig.root.join("etc/hostname"), "byhand\n").unwrap();
    let d: DriftReport = app.get("/api/v1/config/drift", &c).await.json();
    assert_eq!(d.entries.len(), 1);
    assert_eq!(d.entries[0].path, "/etc/hostname");
    assert_eq!(d.entries[0].key, "system.hostname");

    let r = app
        .send(
            Method::POST,
            "/api/v1/config/drift/system.hostname/import",
            &c,
            None,
        )
        .await;
    assert_eq!(r.json::<StagedChanges>().staged, 1);
    let h: HostnameState = app.get("/api/v1/system/hostname", &c).await.json();
    assert_eq!(h.pending.unwrap().hostname, "byhand");
    let r = app
        .send(Method::POST, "/api/v1/config/discard", &c, None)
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let p: PendingChanges = app.get("/api/v1/config/pending", &c).await.json();
    assert!(p.changes.is_empty());

    let r = app
        .send(
            Method::POST,
            "/api/v1/config/drift/system.hostname/overwrite",
            &c,
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    let t = wait_task(app, &c, &r.json::<TaskStarted>().task_id).await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    assert_eq!(read(&rig.root, "etc/hostname"), "nas\n");
    let d: DriftReport = app.get("/api/v1/config/drift", &c).await.json();
    assert!(d.entries.is_empty());
    let r = app
        .send(
            Method::POST,
            "/api/v1/config/drift/no.such/overwrite",
            &c,
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn task_events_over_sse() {
    let rig = rig().await;
    let app = &rig.app;
    let c = app.login().await;
    let req = Request::builder()
        .uri("/api/v1/tasks/stream")
        .header(
            header::COOKIE,
            format!("rynas_session={}", c.cookie.clone().unwrap()),
        )
        .body(Body::empty())
        .unwrap();
    let resp = app.router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let mut body = resp.into_body().into_data_stream();

    app.send(
        Method::PUT,
        "/api/v1/system/hostname",
        &c,
        Some(json!({ "hostname": "nas" })),
    )
    .await;
    let t = apply(app, &c, "").await;

    let mut text = String::new();
    let finished = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(chunk)) = body.next().await {
            text.push_str(&String::from_utf8_lossy(&chunk));
            if text.contains("event: finished") {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(finished, "{text}");
    assert!(text.contains("event: started"), "{text}");
    let data = text
        .lines()
        .skip_while(|l| *l != "event: finished")
        .find_map(|l| l.strip_prefix("data: "))
        .unwrap();
    let e: TaskEvent = serde_json::from_str(data).unwrap();
    assert_eq!(e.kind, TaskEventKind::Finished);
    assert_eq!(e.task.id, t.id);

    // the stream ends with the session
    app.send(Method::POST, "/api/v1/auth/logout", &c, None)
        .await;
    let c2 = app.login().await;
    app.send(
        Method::PUT,
        "/api/v1/system/hostname",
        &c2,
        Some(json!({ "hostname": "x2" })),
    )
    .await;
    apply(app, &c2, "").await;
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(_)) = body.next().await {}
    })
    .await;
    assert!(ended.is_ok(), "stream still open after logout");
}

#[tokio::test]
async fn forwarded_endpoints_need_a_session_and_a_daemon() {
    let rig = rig().await;
    let r = rig
        .app
        .get("/api/v1/config/pending", &Creds::default())
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = rig.app.get("/api/v1/tasks/stream", &Creds::default()).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    let down = common::app(Arc::new(NoProvider));
    let c = down.login().await;
    let r = down.get("/api/v1/config/pending", &c).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(r.code(), error_codes::BACKEND_UNAVAILABLE);
}

#[tokio::test]
async fn every_error_is_an_api_error() {
    let rig = rig().await;
    let c = rig.app.login().await;
    // a query string rynasd cannot parse: its rejection comes back as JSON (A8)
    let r = rig.app.get("/api/v1/tasks?limit=abc", &c).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.headers[header::CONTENT_TYPE], "application/json");
    assert_eq!(r.code(), error_codes::BAD_REQUEST);
    let e: rynas_api_types::ApiError = r.json();
    assert_eq!(e.field.as_deref(), Some("limit"));
    assert!(!e.message.contains("deserialize"), "{}", e.message);
    // a body of the wrong shape: a stable code, the field, no serde text (A10, A13)
    for (body, code, field) in [
        (json!({}), error_codes::MISSING_FIELD, "hostname"),
        (json!({ "hostname": 7 }), error_codes::INVALID_TYPE, "hostname"),
    ] {
        let r = rig
            .app
            .send(Method::PUT, "/api/v1/system/hostname", &c, Some(body))
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        let e: rynas_api_types::ApiError = r.json();
        assert_eq!((e.code.as_str(), e.field.as_deref()), (code, Some(field)));
        assert!(!e.message.contains("expected"), "{}", e.message);
    }
    // method not allowed on a forwarded route
    let r = rig
        .app
        .send(Method::DELETE, "/api/v1/system/hostname", &c, None)
        .await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(r.code(), error_codes::METHOD_NOT_ALLOWED);
    // over the request body limit
    let big = json!({ "hostname": "x".repeat(2 << 20) });
    let r = rig
        .app
        .send(Method::PUT, "/api/v1/system/hostname", &c, Some(big))
        .await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(r.code(), error_codes::PAYLOAD_TOO_LARGE);
    // history lists: one default and one maximum (A17), never an error for a big limit
    let r = rig.app.get("/api/v1/tasks?limit=100000", &c).await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn one_multiplexed_event_stream() {
    let rig = rig().await;
    let app = &rig.app;
    let c = app.login().await;
    let req = Request::builder()
        .uri("/api/v1/events")
        .header(
            header::COOKIE,
            format!("rynas_session={}", c.cookie.clone().unwrap()),
        )
        .body(Body::empty())
        .unwrap();
    let resp = app.router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // a client reads continuously; give the task half a moment to reach rynasd
    let seen = Arc::new(std::sync::Mutex::new(String::new()));
    let s2 = seen.clone();
    tokio::spawn(async move {
        let mut body = resp.into_body().into_data_stream();
        while let Some(Ok(chunk)) = body.next().await {
            s2.lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&chunk));
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    // the latest sample comes first
    assert!(seen.lock().unwrap().starts_with("event: metrics.sample"));

    app.send(
        Method::PUT,
        "/api/v1/system/hostname",
        &c,
        Some(json!({ "hostname": "nas" })),
    )
    .await;
    let t = apply(app, &c, "").await;
    app.store.push(rynas_api_types::MetricsSample {
        ts_ms: 42,
        ..Default::default()
    });
    let mut text = String::new();
    for _ in 0..100 {
        text = seen.lock().unwrap().clone();
        if text.contains("event: task.finished") && text.contains("\"ts_ms\":42") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(text.contains("event: task.finished"), "{text}");
    assert!(text.contains("\"ts_ms\":42"), "{text}");
    assert!(text.contains("event: task.started"), "{text}");
    assert!(text.contains(&t.id), "{text}");
    // the stage and the apply changed the pending set: config.pending, with a generation
    assert!(text.contains("event: config.pending"), "{text}");
    let data = text
        .lines()
        .skip_while(|l| *l != "event: config.pending")
        .find_map(|l| l.strip_prefix("data: "))
        .unwrap();
    let e: rynas_api_types::PendingEvent = serde_json::from_str(data).unwrap();
    assert!(e.generation >= 1);
}

/// `GET /config/pending` with `If-None-Match`.
async fn pending_if(app: &TestApp, c: &Creds, etag: Option<&str>) -> axum::response::Response {
    let mut b = Request::builder().uri("/api/v1/config/pending").header(
        header::COOKIE,
        format!("rynas_session={}", c.cookie.clone().unwrap()),
    );
    if let Some(t) = etag {
        b = b.header(header::IF_NONE_MATCH, t);
    }
    app.router
        .clone()
        .oneshot(b.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn pending_answers_304_while_unchanged() {
    let rig = rig().await;
    let app = &rig.app;
    let c = app.login().await;
    let r = pending_if(app, &c, None).await;
    assert_eq!(r.status(), StatusCode::OK);
    let etag = r.headers()[header::ETAG].to_str().unwrap().to_string();
    assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");

    let r = pending_if(app, &c, Some(&etag)).await;
    assert_eq!(r.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(r.headers()[header::ETAG], etag.as_str());
    let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    assert!(body.is_empty());
    // also as a weak tag or in a list
    let r = pending_if(app, &c, Some(&format!("\"x\", W/{etag}"))).await;
    assert_eq!(r.status(), StatusCode::NOT_MODIFIED);

    // a staged change: a new tag and the full answer
    app.send(
        Method::PUT,
        "/api/v1/system/hostname",
        &c,
        Some(json!({ "hostname": "nas" })),
    )
    .await;
    let r = pending_if(app, &c, Some(&etag)).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_ne!(r.headers()[header::ETAG], etag.as_str());
    let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    let p: PendingChanges = serde_json::from_slice(&body).unwrap();
    assert_eq!(p.changes.len(), 1);
    // the API stays uncacheable for the browser (A26): the client keeps the tag itself
    let r = pending_if(app, &c, None).await;
    assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store");
}
