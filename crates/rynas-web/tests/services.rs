//! Users, groups and shares end to end: rynas-web (session, CSRF, proxy) → a real rynasd
//! router → rynas-config with the `users` and `shares` domains on in-memory accounts,
//! filesystem and system.

mod common;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode, header};
use common::{Creds, FakeAuth, TestApp, build_with_rynasd};
use rynas_api_types::{
    DriftReport, EffectiveAccess, EffectivePermission, GroupView, PendingChanges, PendingOp,
    ShareView, SmbState, StagedChanges, StateDriftKind, TaskInfo, TaskStarted, TaskStatus,
    UserView, error_codes,
};
use rynas_auth::ManualClock;
use rynas_auth::totp::{MemoryStorage, TotpManager};
use rynas_config::{Config, ConfigOptions, HostnameDomain, MemorySystem};
use rynas_services::{MemoryAccounts, MemoryShares, SharesDomain, UsersDomain};
use rynas_tasks::{TaskConfig, TaskManager};
use rynas_web::hardware::NoProvider;
use rynasd::ops::Ops;
use rynasd::server::{DaemonState, PeerCheckedListener, router};
use rynasd::services::Services;
use serde_json::json;
use tempfile::TempDir;

/// `ApiError.field` of an error answer.
fn field(r: &common::Resp) -> Option<String> {
    r.json::<rynas_api_types::ApiError>().field
}

struct Rig {
    app: TestApp,
    sys: Arc<MemorySystem>,
    acc: Arc<MemoryAccounts>,
    host: Arc<MemoryShares>,
    root: PathBuf,
    _dir: TempDir,
}

async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("etc")).unwrap();
    std::fs::write(root.join("etc/hostname"), "nas\n").unwrap();
    let sys = Arc::new(MemorySystem::new("nas"));
    let acc = Arc::new(MemoryAccounts::new());
    let host = Arc::new(MemoryShares::new());
    host.add_dir(
        "/srv/vol1/docs",
        Some("11111111-2222-4333-8444-555555555555"),
    );
    let users = Arc::new(UsersDomain::new(acc.clone()));
    let shares = Arc::new(SharesDomain::new(host.clone(), acc.clone()));
    let state_dir = dir.path().join("state");
    let config = Config::open(
        ConfigOptions::new(&state_dir, &root),
        sys.clone(),
        vec![Arc::new(HostnameDomain), users.clone(), shares.clone()],
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
    let ops = Arc::new(Ops::new(Arc::new(config), tasks));
    st.services = Some(Arc::new(Services {
        ops: ops.clone(),
        users,
        shares,
        homes: Arc::new(rynas_services::HomesDomain::new(
            Arc::new(rynas_services::MemoryHomes::new()),
            acc.clone(),
        )),
        moves: Arc::new(rynas_services::homes::MoveStore::memory()),
    }));
    st.ops = Some(ops);
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
        acc,
        host,
        root,
        _dir: dir,
    }
}

async fn wait_task(app: &TestApp, c: &Creds, id: &str) -> TaskInfo {
    for _ in 0..200 {
        let t: TaskInfo = app.get(&format!("/api/v1/tasks/{id}"), c).await.json();
        if t.status.is_finished() {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task {id} did not finish");
}

async fn started(app: &TestApp, c: &Creds, r: common::Resp) -> TaskInfo {
    assert_eq!(
        r.status,
        StatusCode::ACCEPTED,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    let s: TaskStarted = r.json();
    wait_task(app, c, &s.task_id).await
}

async fn apply(app: &TestApp, c: &Creds) -> TaskInfo {
    let r = app
        .send(
            Method::POST,
            "/api/v1/config/apply",
            c,
            Some(json!({ "comment": "t" })),
        )
        .await;
    started(app, c, r).await
}

#[tokio::test]
async fn users_groups_shares_lifecycle() {
    let rig = rig().await;
    let app = &rig.app;
    let c = app.login().await;

    // group and user are staged
    let r = app
        .send(
            Method::POST,
            "/api/v1/groups",
            &c,
            Some(json!({ "name": "office", "description": "Office" })),
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    let g: GroupView = r.json();
    assert_eq!((g.group.gid, g.pending), (1000, Some(PendingOp::Add)));
    assert_eq!(r.headers[header::LOCATION], "/api/v1/groups/office");
    let r = app
        .send(
            Method::POST,
            "/api/v1/users",
            &c,
            Some(json!({ "name": "alice", "full_name": "Alice Smith", "groups": ["office"] })),
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    let u: UserView = r.json();
    assert_eq!(u.user.uid, 1000);
    assert_eq!(r.headers[header::LOCATION], "/api/v1/users/alice");
    assert_eq!(u.user.home, "/home/alice");
    assert_eq!(u.status.pending, Some(PendingOp::Add));
    assert!(!u.status.on_system);
    // duplicates and invalid values
    let r = app
        .send(
            Method::POST,
            "/api/v1/users",
            &c,
            Some(json!({ "name": "alice" })),
        )
        .await;
    assert_eq!(
        (r.status, r.code()),
        (StatusCode::CONFLICT, error_codes::ALREADY_EXISTS.into())
    );
    let r = app
        .send(
            Method::POST,
            "/api/v1/users",
            &c,
            Some(json!({ "name": "Bad Name" })),
        )
        .await;
    assert_eq!(r.code(), error_codes::INVALID_VALUE);
    assert_eq!(field(&r).as_deref(), Some("name"));
    let r = app
        .send(
            Method::POST,
            "/api/v1/users",
            &c,
            Some(json!({ "name": "bob", "email": "not-an-address" })),
        )
        .await;
    assert_eq!(r.code(), error_codes::INVALID_VALUE);
    assert_eq!(field(&r).as_deref(), Some("email"));

    // the password needs the user on the system
    let r = app
        .send(
            Method::POST,
            "/api/v1/users/alice/password",
            &c,
            Some(json!({ "password": "correct horse" })),
        )
        .await;
    assert_eq!(
        (r.status, r.code()),
        (StatusCode::CONFLICT, error_codes::NOT_APPLIED.into())
    );

    // share: unknown principals are refused
    let share = |perms: serde_json::Value| {
        json!({
            "name": "docs",
            "path": "/srv/vol1/docs",
            "smb": { "enabled": true, "preset": "multiprotocol" },
            "nfs": { "enabled": true, "rules": [ { "client": "192.168.1.0/24" } ] },
            "permissions": perms,
        })
    };
    let r = app
        .send(
            Method::POST,
            "/api/v1/shares",
            &c,
            Some(share(
                json!([{ "kind": "user", "name": "ghost", "access": "ro" }]),
            )),
        )
        .await;
    assert_eq!(r.code(), error_codes::INVALID_VALUE);
    assert_eq!(field(&r).as_deref(), Some("permissions"));
    assert!(String::from_utf8_lossy(&r.body).contains("unknown user ghost"));
    let r = app
        .send(
            Method::POST,
            "/api/v1/shares",
            &c,
            Some(share(json!([
                { "kind": "group", "name": "office", "access": "rw" },
                { "kind": "user", "name": "alice", "access": "ro" }
            ]))),
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    let s: ShareView = r.json();
    assert_eq!(s.share.nfs.fsid, "11111111-2222-4333-8444-555555555555");
    assert!(
        r.headers[header::LOCATION]
            .to_str()
            .unwrap()
            .starts_with("/api/v1/shares/")
    );
    assert!(s.status.subvolume && s.status.path_ok);
    assert!(s.status.warnings.iter().any(|w| w.contains("same uids")));

    // effective permission with provenance, computed on the staged configuration
    let e: EffectivePermission = app
        .get("/api/v1/shares/docs/effective?principal=alice", &c)
        .await
        .json();
    assert_eq!(e.access, EffectiveAccess::Rw);
    assert_eq!(
        e.explanation,
        "RW from group office (also RO from user alice)"
    );
    let e: EffectivePermission = app
        .get("/api/v1/shares/docs/effective?principal=@office", &c)
        .await
        .json();
    assert_eq!(e.access, EffectiveAccess::Rw);

    // one apply for both keys
    let p: PendingChanges = app.get("/api/v1/config/pending", &c).await.json();
    assert_eq!(
        p.changes.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(),
        ["shares", "users"]
    );
    let t = apply(app, &c).await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    assert!(rig.acc.state().users.iter().any(|u| u.name == "alice"));
    assert!(rig.host.state().acls["/srv/vol1/docs"].contains("user:alice:rwx"));
    let smb = std::fs::read_to_string(rig.root.join("etc/samba/smb.conf")).unwrap();
    assert!(smb.contains("[docs]") && smb.contains("write list = @office"));
    assert!(
        rig.sys
            .state()
            .calls
            .iter()
            .any(|c| c == "run exportfs -ra")
    );

    let u: UserView = app.get("/api/v1/users/alice", &c).await.json();
    assert!(u.status.managed && u.status.on_system && u.status.needs_password);
    assert!(
        u.status
            .badges
            .contains(&rynas_api_types::UserBadge::SmbPasswordReset)
    );
    let d: DriftReport = app.get("/api/v1/config/drift", &c).await.json();
    assert!(
        d.state
            .iter()
            .any(|s| s.object == "user:alice" && s.kind == StateDriftKind::MissingInSamba)
    );

    // password: a task, Linux then Samba
    let r = app
        .send(
            Method::POST,
            "/api/v1/users/alice/password",
            &c,
            Some(json!({ "password": "short" })),
        )
        .await;
    assert_eq!(r.code(), error_codes::INVALID_VALUE);
    assert_eq!(field(&r).as_deref(), Some("password"));
    let r = app
        .send(
            Method::POST,
            "/api/v1/users/alice/password",
            &c,
            Some(json!({ "password": "correct horse" })),
        )
        .await;
    let t = started(app, &c, r).await;
    assert_eq!(
        (t.status, t.kind.as_str()),
        (TaskStatus::Ok, "user.password"),
        "{:?}",
        t.error
    );
    let log = String::from_utf8(
        app.get(&format!("/api/v1/tasks/{}/log", t.id), &c)
            .await
            .body,
    )
    .unwrap();
    assert!(log.contains("chpasswd: new password for alice"), "{log}");
    assert!(
        !log.contains("correct horse"),
        "the password never reaches the log"
    );
    let u: UserView = app.get("/api/v1/users/alice", &c).await.json();
    assert_eq!(u.status.in_samba, Some(true));
    assert!(!u.status.needs_password);

    // SMB settings
    let r = app
        .send(
            Method::PUT,
            "/api/v1/services/smb",
            &c,
            Some(json!({ "enabled": true, "workgroup": "bad group!" })),
        )
        .await;
    assert_eq!(r.code(), error_codes::INVALID_VALUE);
    assert_eq!(field(&r).as_deref(), Some("workgroup"));
    let r = app
        .send(
            Method::PUT,
            "/api/v1/services/smb",
            &c,
            Some(json!({ "enabled": true, "workgroup": "office", "netbios": true })),
        )
        .await;
    let st: SmbState = r.json();
    assert!(st.pending && st.settings.workgroup == "OFFICE");
    app.send(Method::POST, "/api/v1/config/discard", &c, None)
        .await;

    // delete: CSRF, confirmation, staged with the matrix entry
    let no_csrf = Creds {
        cookie: c.cookie.clone(),
        csrf: None,
    };
    let r = app
        .send(Method::DELETE, "/api/v1/users/alice", &no_csrf, None)
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = app
        .send(
            Method::DELETE,
            "/api/v1/users/alice?remove_home=true",
            &c,
            None,
        )
        .await;
    assert_eq!(r.code(), error_codes::CONFIRMATION_REQUIRED);
    let r = app
        .send(
            Method::DELETE,
            "/api/v1/users/alice?remove_home=true&confirm=alice",
            &c,
            None,
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(
        r.json::<StagedChanges>().staged,
        2,
        "users and the share matrix"
    );
    let u: UserView = app.get("/api/v1/users/alice", &c).await.json();
    assert_eq!(u.status.pending, Some(PendingOp::Delete));
    let t = apply(app, &c).await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    let st = rig.acc.state();
    assert!(!st.users.iter().any(|u| u.name == "alice"));
    assert!(st.log.contains(&"userdel alice (home removed)".to_string()));
    assert!(st.samba.as_ref().unwrap().is_empty());
}
