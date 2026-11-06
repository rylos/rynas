//! User homes end to end with the in-memory system, accounts and filesystem: the setting
//! (homes key and the SMB `[homes]` share staged together), new users created in the homes
//! folder, the move of an existing home (plan, blockers, task, kept old folder, undo,
//! confirmation) and what the move protects (user deletion, folder listing).

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rynas_api_types::{
    ApiError, HomeLocation, HomeMoveBlockerCode, HomeMovePlan, HomesState, TaskInfo, TaskStarted,
    TaskStatus, UserView, error_codes,
};
use rynas_auth::ManualClock;
use rynas_auth::backend::{AuthError, Authenticator, BoxFuture, Identity};
use rynas_auth::totp::{MemoryStorage, TotpManager};
use rynas_config::{Config, ConfigOptions, MemorySystem};
use rynas_services::accounts::SysUser;
use rynas_services::homes::MoveStore;
use rynas_services::{
    HomesDomain, HomesHost, MemoryAccounts, MemoryHomes, MemoryShares, SharesDomain, UsersDomain,
};
use rynas_tasks::{TaskConfig, TaskManager};
use rynasd::ops::Ops;
use rynasd::server::{DaemonState, router};
use rynasd::services::Services;
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

struct Env {
    _dir: tempfile::TempDir,
    app: axum::Router,
    acc: Arc<MemoryAccounts>,
    homes: Arc<MemoryHomes>,
    config: Arc<Config>,
    ops: Arc<Ops>,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("etc/samba")).unwrap();
    let sys = Arc::new(MemorySystem::new("nas"));
    let acc = Arc::new(MemoryAccounts::new());
    // the account made at install time, home on the system disk with SSH keys
    acc.state().users.push(SysUser {
        name: "marco".into(),
        uid: 1000,
        gid: 100,
        full_name: "Marco".into(),
        home: "/home/marco".into(),
        shell: "/bin/bash".into(),
        expire_days: None,
    });
    let homes = Arc::new(MemoryHomes::new());
    homes.add_volume("/", "ext4", 50 << 30);
    homes.add_volume("/volume1", "btrfs", 3 << 40);
    homes.add_dir("/home/marco", 1000, 2 << 30);
    homes.add_dir("/home/marco/.ssh/authorized_keys", 1000, 1);
    let shares_host = Arc::new(MemoryShares::new());
    let users = Arc::new(UsersDomain::new(acc.clone()).with_homes(homes.clone()));
    let shares = Arc::new(SharesDomain::new(shares_host, acc.clone()));
    let homes_domain = Arc::new(HomesDomain::new(homes.clone(), acc.clone()));
    let config = Arc::new(
        Config::open(
            ConfigOptions::new(dir.path().join("state"), &root),
            sys,
            vec![homes_domain.clone(), users.clone(), shares.clone()],
        )
        .unwrap(),
    );
    let tasks = TaskManager::new(dir.path().join("tasks"), "nas", TaskConfig::default()).unwrap();
    let ops = Arc::new(Ops::new(config.clone(), tasks));
    let clock = Arc::new(ManualClock::new(1_800_000_000_000));
    let totp = Arc::new(TotpManager::new(
        MemoryStorage::default(),
        clock.clone(),
        "rynas",
    ));
    let mut st = DaemonState::new(Arc::new(NoAuth), totp, clock);
    st.ops = Some(ops.clone());
    st.services = Some(Arc::new(Services {
        ops: ops.clone(),
        users,
        shares,
        homes: homes_domain,
        moves: Arc::new(MoveStore::in_dir(dir.path())),
    }));
    Env {
        _dir: dir,
        app: router(st),
        acc,
        homes,
        config,
        ops,
    }
}

async fn call(e: &Env, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-rynas-user", "admin")
        .body(body.map(|b| Body::from(b.to_string())).unwrap_or_default())
        .unwrap();
    let r = e.app.clone().oneshot(req).await.unwrap();
    let s = r.status();
    (s, to_bytes(r.into_body(), 4 << 20).await.unwrap().to_vec())
}

fn code(b: &[u8]) -> String {
    serde_json::from_slice::<ApiError>(b).unwrap().code
}

fn apply(e: &Env) {
    e.config
        .apply("admin", "admin", "test", &|l| eprintln!("{l}"), &|| false)
        .unwrap();
}

async fn task(e: &Env, b: &[u8]) -> TaskInfo {
    let id = serde_json::from_slice::<TaskStarted>(b).unwrap().task_id;
    for _ in 0..200 {
        if let Some(t) = e.ops.tasks.get(&id)
            && t.status.is_finished()
        {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task {id} still running");
}

async fn state(e: &Env) -> HomesState {
    let (s, b) = call(e, "GET", "/v1/services/homes", None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    serde_json::from_slice(&b).unwrap()
}

fn passwd_home(e: &Env, n: &str) -> String {
    e.acc
        .state()
        .users
        .iter()
        .find(|u| u.name == n)
        .unwrap()
        .home
        .clone()
}

#[tokio::test]
async fn homes_setting_new_users_and_moving_an_existing_home() {
    let e = env();
    // off by default; the volumes offered leave out the system disk
    let st = state(&e).await;
    assert!(!st.settings.enabled && st.target.is_none());
    assert_eq!(
        st.volumes
            .iter()
            .map(|v| v.path.as_str())
            .collect::<Vec<_>>(),
        ["/volume1"]
    );
    assert_eq!(st.homes[0].location, HomeLocation::System);

    // a move while the service is off: refused with the reason
    let (s, b) = call(
        &e,
        "POST",
        "/v1/services/homes/move/plan",
        Some(json!({"users": ["marco"]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let p: Vec<HomeMovePlan> = serde_json::from_slice(&b).unwrap();
    assert!(
        p[0].blockers
            .iter()
            .any(|b| b.code == HomeMoveBlockerCode::ServiceOff)
    );
    let (s, _) = call(
        &e,
        "POST",
        "/v1/services/homes/move",
        Some(json!({"users": ["marco"]})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);

    // the system disk is refused, a volume is staged with the [homes] share
    let (s, b) = call(
        &e,
        "PUT",
        "/v1/services/homes",
        Some(json!({"enabled": true, "volume": "/"})),
    )
    .await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE)
    );
    let (s, b) = call(
        &e,
        "PUT",
        "/v1/services/homes",
        Some(json!({"enabled": true, "volume": "/volume1", "smb": {"enabled": true, "recycle": {"enabled": true, "retention_days": 30}}})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let st: HomesState = serde_json::from_slice(&b).unwrap();
    assert!(st.pending && st.settings.smb.enabled);
    assert_eq!(st.folder, "/volume1/homes");

    // a user created now gets the stable path, created as a subvolume by the apply
    let (s, b) = call(&e, "POST", "/v1/users", Some(json!({"name": "alice"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    let v: UserView = serde_json::from_slice(&b).unwrap();
    assert_eq!(v.user.home, "/srv/rynas/homes/alice");
    apply(&e);
    assert_eq!(passwd_home(&e, "alice"), "/srv/rynas/homes/alice");
    assert!(e.homes.probe("/volume1/homes/alice").subvolume);
    let st = state(&e).await;
    assert!(!st.pending);
    assert_eq!(st.target.as_deref(), Some("/volume1/homes"));
    let alice = st.homes.iter().find(|h| h.user == "alice").unwrap();
    assert_eq!(
        (alice.location, alice.subvolume),
        (HomeLocation::Volume, true)
    );
    // the home cannot be changed by an edit
    let (s, b) = call(
        &e,
        "PUT",
        "/v1/users/alice",
        Some(json!({"home": "/home/alice"})),
    )
    .await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE)
    );

    // marco: the plan shows the size and the SSH keys; a running session blocks
    e.homes.state().processes.insert(1000, vec![777]);
    let (_, b) = call(&e, "POST", "/v1/services/homes/move/plan", Some(json!({}))).await;
    let p: Vec<HomeMovePlan> = serde_json::from_slice(&b).unwrap();
    assert_eq!(p.len(), 1, "alice is already in the homes folder: {p:?}");
    assert!(p[0].ssh_keys && p[0].subvolume && !p[0].allowed);
    assert_eq!(p[0].size_bytes, 2 << 30);
    assert_eq!(p[0].blockers[0].code, HomeMoveBlockerCode::Processes);
    let (s, _) = call(
        &e,
        "POST",
        "/v1/services/homes/move",
        Some(json!({"users": ["marco"]})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    e.homes.state().processes.clear();

    // the move: copy, verify, passwd switched, old folder kept
    let (s, b) = call(
        &e,
        "POST",
        "/v1/services/homes/move",
        Some(json!({"users": ["marco"]})),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let t = task(&e, &b).await;
    assert_eq!(t.status, TaskStatus::Ok, "{t:?}");
    assert_eq!(passwd_home(&e, "marco"), "/srv/rynas/homes/marco");
    assert!(e.homes.probe("/home/marco.rynas-old").exists);
    assert!(!e.homes.probe("/home/marco").exists);
    let st = state(&e).await;
    let m = st.homes.iter().find(|h| h.user == "marco").unwrap();
    assert_eq!(m.location, HomeLocation::Volume);
    assert_eq!(m.moved.as_ref().unwrap().kept, "/home/marco.rynas-old");

    // while the move waits, the user cannot be deleted and cannot move again
    let (s, _) = call(&e, "DELETE", "/v1/users/marco", None).await;
    assert_eq!(s, StatusCode::CONFLICT);

    // undo: the old home is back, the copy is aside
    let (s, b) = call(&e, "POST", "/v1/services/homes/move/marco/undo", None).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let t = task(&e, &b).await;
    assert_eq!(t.status, TaskStatus::Ok, "{t:?}");
    assert_eq!(passwd_home(&e, "marco"), "/home/marco");
    assert!(e.homes.probe("/home/marco").exists && !e.homes.probe("/volume1/homes/marco").exists);
    assert!(state(&e).await.homes.iter().all(|h| h.moved.is_none()));
    let (s, _) = call(&e, "POST", "/v1/services/homes/move/marco/finish", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // again, then confirmed: the kept folder goes
    e.homes
        .state()
        .entries
        .retain(|k, _| !k.contains(".marco.undone"));
    let (_, b) = call(
        &e,
        "POST",
        "/v1/services/homes/move",
        Some(json!({"users": ["marco"]})),
    )
    .await;
    assert_eq!(task(&e, &b).await.status, TaskStatus::Ok);
    let (s, b) = call(&e, "POST", "/v1/services/homes/move/marco/finish", None).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    assert_eq!(task(&e, &b).await.status, TaskStatus::Ok);
    assert!(!e.homes.probe("/home/marco.rynas-old").exists);
    assert_eq!(passwd_home(&e, "marco"), "/srv/rynas/homes/marco");

    // the homes folder cannot change volume while it holds homes
    e.homes.add_volume("/volume2", "btrfs", 1 << 40);
    let (s, b) = call(
        &e,
        "PUT",
        "/v1/services/homes",
        Some(json!({"enabled": true, "volume": "/volume2"})),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "{}",
        String::from_utf8_lossy(&b)
    );

    // service off: the [homes] share goes with it, the homes stay
    let (s, b) = call(
        &e,
        "PUT",
        "/v1/services/homes",
        Some(json!({"enabled": false, "volume": "/volume1", "smb": {"enabled": true}})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let st: HomesState = serde_json::from_slice(&b).unwrap();
    assert!(!st.settings.smb.enabled);
    apply(&e);
    assert_eq!(
        e.homes.read_link("/srv/rynas/homes").as_deref(),
        Some("/volume1/homes")
    );
    let (_, b) = call(&e, "POST", "/v1/users", Some(json!({"name": "bob"}))).await;
    let v: UserView = serde_json::from_slice(&b).unwrap();
    assert_eq!(v.user.home, "/home/bob");
}
