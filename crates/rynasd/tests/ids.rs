//! Adoption of accounts made outside rynas, UID change and file takeover end to end with the
//! in-memory accounts; the walk reads a real temp dir (the "volume") and records its changes
//! instead of making them.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rynas_api_types::{
    GroupView, IdBlockerCode, IdRemapPlan, StagedChanges, TaskInfo, TaskStarted, TaskStatus,
    UserView,
};
use rynas_auth::ManualClock;
use rynas_auth::backend::{AuthError, Authenticator, BoxFuture, Identity};
use rynas_auth::totp::{MemoryStorage, TotpManager};
use rynas_config::{Config, ConfigOptions, MemorySystem};
use rynas_services::accounts::{SysGroup, SysUser};
use rynas_services::homes::MoveStore;
use rynas_services::idremap::{Entry, RemapFs};
use rynas_services::{
    HomesDomain, MemoryAccounts, MemoryHomes, MemoryShares, SharesDomain, UsersDomain,
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

type Chown = (PathBuf, Option<u32>, Option<u32>);

/// Records `chown` and `chmod` calls; reads (ACLs, snapshots, mounts) find nothing special.
#[derive(Default)]
struct Recorder {
    chowns: Mutex<Vec<Chown>>,
    chmods: Mutex<Vec<PathBuf>>,
}

impl RemapFs for Recorder {
    fn chown(&self, e: &Entry, u: Option<u32>, g: Option<u32>) -> std::io::Result<()> {
        self.chowns.lock().unwrap().push((e.path.into(), u, g));
        Ok(())
    }
    fn chmod(&self, e: &Entry, _: u32) -> std::io::Result<()> {
        self.chmods.lock().unwrap().push(e.path.into());
        Ok(())
    }
    fn get_acl(&self, _: &Entry, _: bool) -> std::io::Result<Option<Vec<u8>>> {
        Ok(None)
    }
    fn set_acl(&self, _: &Entry, _: bool, _: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
    fn readonly_subvolume(&self, _: &Entry) -> bool {
        false
    }
    fn mount_points(&self) -> BTreeSet<PathBuf> {
        BTreeSet::new()
    }
}

struct Env {
    dir: tempfile::TempDir,
    app: axum::Router,
    acc: Arc<MemoryAccounts>,
    config: Arc<Config>,
    ops: Arc<Ops>,
    rec: Arc<Recorder>,
    homes: Arc<MemoryHomes>,
}

fn env() -> Env {
    // not under /tmp: a system path is never a data volume
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("etc/samba")).unwrap();
    let vol = dir.path().join("volume1");
    std::fs::create_dir_all(vol.join("photos")).unwrap();
    std::fs::write(vol.join("photos/a.jpg"), "x").unwrap();
    let acc = Arc::new(MemoryAccounts::new());
    let homes = Arc::new(MemoryHomes::new());
    homes.add_volume(vol.to_str().unwrap(), "btrfs", 1 << 40);
    let rec = Arc::new(Recorder::default());
    let users = Arc::new(
        UsersDomain::new(acc.clone())
            .with_homes(homes.clone())
            .with_remap(rec.clone()),
    );
    let shares = Arc::new(SharesDomain::new(Arc::new(MemoryShares::new()), acc.clone()));
    let homes_domain = Arc::new(HomesDomain::new(homes.clone(), acc.clone()));
    let config = Arc::new(
        Config::open(
            ConfigOptions::new(dir.path().join("state"), &root),
            Arc::new(MemorySystem::new("nas")),
            vec![homes_domain.clone(), users.clone(), shares.clone()],
        )
        .unwrap(),
    );
    // rynas manages users already (an applied document without these accounts): they are
    // made afterwards, outside rynas (before any apply the system state is the document)
    config
        .apply_value("admin", "users", Some(json!({})), "test", &|_| {})
        .unwrap();
    {
        let mut s = acc.state();
        s.groups.push(SysGroup {
            name: "family".into(),
            gid: 1001,
            members: vec!["anna".into()],
        });
        // made by the installer and by hand: not in rynas' configuration
        for (n, u) in [("anna", 1005), ("luca", 1006)] {
            s.users.push(SysUser {
                name: n.into(),
                uid: u,
                gid: 100,
                home: format!("/home/{n}"),
                shell: "/bin/bash".into(),
                ..Default::default()
            });
        }
    }
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
        dir,
        app: router(st),
        acc,
        config,
        ops,
        rec,
        homes,
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

async fn plan(e: &Env, body: Value) -> IdRemapPlan {
    let (s, b) = call(e, "POST", "/v1/ids/plan", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    serde_json::from_slice(&b).unwrap()
}

fn uid_of(e: &Env, n: &str) -> u32 {
    e.acc.state().users.iter().find(|u| u.name == n).unwrap().uid
}

#[tokio::test]
async fn adopt_change_uid_and_take_over_files() {
    let e = env();
    let vol = e.dir.path().join("volume1");

    // both users are listed, not managed; the unmanaged group too
    let (_, b) = call(&e, "GET", "/v1/users", None).await;
    let list: Vec<UserView> = serde_json::from_slice(&b).unwrap();
    assert!(list.iter().all(|u| !u.status.managed));
    let (_, b) = call(&e, "GET", "/v1/groups", None).await;
    let groups: Vec<GroupView> = serde_json::from_slice(&b).unwrap();
    let fam = groups.iter().find(|g| g.group.name == "family").unwrap();
    assert!(!fam.managed && fam.members == vec!["anna"]);

    // a UID change needs the account adopted first
    let p = plan(&e, json!({"kind": "user", "name": "anna", "new_id": 1026})).await;
    assert!(!p.allowed);
    assert!(p.blockers.iter().any(|b| b.code == IdBlockerCode::Unmanaged));

    // saving an unmanaged user adopts it; "adopt all" takes the rest
    let (s, b) = call(
        &e,
        "PUT",
        "/v1/users/anna",
        Some(json!({"full_name": "Anna", "groups": ["family"]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let (s, b) = call(&e, "POST", "/v1/users/adopt", Some(json!({}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(serde_json::from_slice::<StagedChanges>(&b).unwrap().staged, 1);
    let (s, _) = call(&e, "POST", "/v1/users/adopt", Some(json!({"names": ["root"]}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    apply(&e);
    assert!(
        e.acc
            .state()
            .log
            .iter()
            .all(|l| !l.starts_with("useradd")),
        "accounts are not recreated"
    );

    // guards: system ids, ids in use, folders outside the data folders
    let p = plan(&e, json!({"kind": "user", "name": "anna", "new_id": 1006})).await;
    assert!(p.blockers.iter().any(|b| b.code == IdBlockerCode::IdInUse && b.detail == "luca"));
    let p = plan(&e, json!({"kind": "user", "name": "anna", "new_id": 0})).await;
    assert!(p.blockers.iter().any(|b| b.code == IdBlockerCode::OutOfRange));
    let p = plan(&e, json!({"kind": "user", "name": "anna", "from_id": 33})).await;
    assert!(p.blockers.iter().any(|b| b.code == IdBlockerCode::OutOfRange));
    let p = plan(
        &e,
        json!({"kind": "user", "name": "anna", "new_id": 1026, "paths": ["/etc"]}),
    )
    .await;
    assert!(p.blockers.iter().any(|b| b.code == IdBlockerCode::PathInvalid));

    // the dry run walks the volume and changes nothing
    let p = plan(&e, json!({"kind": "user", "name": "anna", "new_id": 1026})).await;
    assert!(p.allowed, "{:?}", p.blockers);
    assert_eq!(p.roots, vec![vol.display().to_string()]);
    let scan = p.scan.unwrap();
    assert_eq!(scan.scanned, 3);
    assert!(e.rec.chowns.lock().unwrap().is_empty());

    // the change: usermod -u through the configuration, then the walk
    let (s, b) = call(
        &e,
        "POST",
        "/v1/ids/remap",
        Some(json!({"kind": "user", "name": "anna", "new_id": 1026})),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let t = task(&e, &b).await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    assert_eq!(uid_of(&e, "anna"), 1026);
    let applied: Value = e.config.applied("users").unwrap().unwrap();
    assert!(
        applied["users"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["name"] == "anna" && u["uid"] == 1026)
    );

    // take over files of an id no account has: the files of the test (owned by the uid
    // running it) become luca's; only possible when that uid is free in the fake accounts
    // SAFETY: plain getter.
    let me = unsafe { libc::getuid() };
    if (1000..65534).contains(&me) && me != 1006 && me != 1026 {
        let p = plan(&e, json!({"kind": "user", "name": "luca", "from_id": me})).await;
        assert!(p.allowed, "{:?}", p.blockers);
        assert_eq!(p.scan.as_ref().unwrap().owners, 3);
        let (s, b) = call(
            &e,
            "POST",
            "/v1/ids/remap",
            Some(json!({"kind": "user", "name": "luca", "from_id": me, "expected": 3})),
        )
        .await;
        assert_eq!(s, StatusCode::ACCEPTED);
        let t = task(&e, &b).await;
        assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
        let c = e.rec.chowns.lock().unwrap();
        assert_eq!(c.len(), 3);
        assert!(c.iter().all(|(_, u, g)| *u == Some(1006) && g.is_none()));
        // the account did not change
        drop(c);
        assert_eq!(uid_of(&e, "luca"), 1006);
    }

    // refused when blocked
    let (s, _) = call(
        &e,
        "POST",
        "/v1/ids/remap",
        Some(json!({"kind": "user", "name": "anna", "new_id": 1006})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
}

#[tokio::test]
async fn folders_resolved_retired_ids_resume_and_group_processes() {
    let e = env();
    let vol = e.dir.path().join("volume1");
    let (s, _) = call(&e, "POST", "/v1/users/adopt", Some(json!({}))).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(&e, "POST", "/v1/groups/adopt", Some(json!({}))).await;
    assert_eq!(s, StatusCode::OK);
    apply(&e);

    // a folder is checked and walked as resolved: a link out of the volume is refused, a
    // missing folder too; a link inside the volume is walked as its target
    std::os::unix::fs::symlink("/etc", vol.join("etc-link")).unwrap();
    std::os::unix::fs::symlink(vol.join("photos"), vol.join("pics")).unwrap();
    let bad = |p: &IdRemapPlan, path: &str| {
        p.blockers
            .iter()
            .any(|b| b.code == IdBlockerCode::PathInvalid && b.detail == path)
    };
    let link = vol.join("etc-link").display().to_string();
    let missing = vol.join("nope").display().to_string();
    let p = plan(
        &e,
        json!({"kind": "user", "name": "anna", "new_id": 1026, "paths": [link, missing]}),
    )
    .await;
    assert!(bad(&p, &link) && bad(&p, &missing), "{:?}", p.blockers);
    let pics = vol.join("pics").display().to_string();
    let p = plan(
        &e,
        json!({"kind": "user", "name": "anna", "new_id": 1026, "paths": [pics]}),
    )
    .await;
    assert!(p.allowed, "{:?}", p.blockers);
    let real = std::fs::canonicalize(vol.join("photos")).unwrap();
    assert_eq!(p.paths, vec![real.display().to_string()]);

    // the change retires the old id
    let (s, b) = call(
        &e,
        "POST",
        "/v1/ids/remap",
        Some(json!({"kind": "user", "name": "anna", "new_id": 1026})),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let t = task(&e, &b).await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    let applied: rynas_api_types::UsersConfig =
        serde_json::from_value(e.config.applied("users").unwrap().unwrap()).unwrap();
    assert_eq!(applied.retired_ids.len(), 1);
    let r = &applied.retired_ids[0];
    assert_eq!((r.id, r.name.as_str(), r.new_id), (1005, "anna", 1026));
    assert_eq!(r.date.len(), 10);

    // shown by the plan: luca moving to 1005 is warned about, anna taking over the files of
    // 1005 finishes her change
    let p = plan(&e, json!({"kind": "user", "name": "luca", "new_id": 1005})).await;
    assert_eq!(p.retired.len(), 1);
    assert_eq!(p.target_retired.as_ref().map(|r| r.id), Some(1005));
    let p = plan(&e, json!({"kind": "user", "name": "anna", "from_id": 1005})).await;
    assert!(p.allowed && p.resumes, "{:?}", p.blockers);
    let p = plan(&e, json!({"kind": "user", "name": "luca", "from_id": 1005})).await;
    assert!(!p.resumes);

    // a new account never gets a retired id: with 1000-1004 taken, the next one is 1007
    {
        let mut s = e.acc.state();
        for (i, uid) in (1000..1005).enumerate() {
            s.users.push(SysUser {
                name: format!("sys{i}"),
                uid,
                gid: 100,
                home: format!("/home/sys{i}"),
                shell: "/bin/bash".into(),
                ..Default::default()
            });
        }
    }
    let (s, b) = call(&e, "POST", "/v1/users", Some(json!({"name": "neo"}))).await;
    assert!(s.is_success(), "{s} {}", String::from_utf8_lossy(&b));
    apply(&e);
    assert_eq!(uid_of(&e, "neo"), 1007);

    // a GID change waits for the processes running with that gid
    e.homes.state().group_processes.insert(1001, vec![4321]);
    let p = plan(&e, json!({"kind": "group", "name": "family", "new_id": 1500})).await;
    assert!(
        p.blockers
            .iter()
            .any(|b| b.code == IdBlockerCode::Processes && b.detail == "4321"),
        "{:?}",
        p.blockers
    );
    e.homes.state().group_processes.clear();
    let p = plan(&e, json!({"kind": "group", "name": "family", "new_id": 1500})).await;
    assert!(p.allowed, "{:?}", p.blockers);
}
