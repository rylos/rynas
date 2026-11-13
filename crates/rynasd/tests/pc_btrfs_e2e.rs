//! PC backup, btrfs method end to end on a real btrfs filesystem: `rynas-agent btrfs-send`
//! (snapshot, parent choice, `btrfs send | zstd`, chunked upload) against rynasd's agent
//! routes served on 127.0.0.1, with the real receive ([`HostReceive`]: the folder lock, the
//! stream guard, `btrfs receive --chroot` in its transient unit and staging folder, the check
//! afterwards, the move into the folder). A full send, then an incremental one on it, and the
//! `received_uuid` chain checked with ioctls; then hand-made hostile streams (a symlink
//! escape, a rename of an older snapshot, a repeated attribute, `..`, a v2 stream, and with
//! `RYNAS_TEST_BLOCK=<the test's loop device>` a write into a block node), each refused with
//! nothing left in the folder.
//!
//! Needs root, systemd and a throwaway mounted btrfs filesystem, so it is `#[ignore]`d (it
//! never passes in silence). Build as yourself, run the test binary as root:
//!
//! ```sh
//! cargo test -p rynasd --test pc_btrfs_e2e --no-run
//! sudo RYNAS_TEST_BTRFS=/mnt/loop RYNAS_AGENT_BIN=…/rynas-agent <test binary> --ignored --nocapture
//! ```
//!
//! It creates `<mnt>/e2e-src/@data` (the PC's subvolume), `<mnt>/e2e-snaps` (the agent's
//! snapshots) and `<mnt>/e2e-nas/pc` (the NAS folder), and refuses to run when they exist.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rynas_api_types::TaskStarted;
use rynas_auth::ManualClock;
use rynas_auth::backend::{AuthError, Authenticator, BoxFuture, Identity};
use rynas_auth::totp::{MemoryStorage, TotpManager};
use rynas_config::{Config, ConfigOptions, MemorySystem};
use rynas_notify::MemoryNotifier;
use rynas_tasks::{TaskConfig, TaskManager};
use rynasd::ops::Ops;
use rynasd::pc_backup::{HostReceive, PcBackup};
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

fn groups(u: &str) -> Option<Vec<String>> {
    (u == "admin").then(|| vec!["rynas-admin".into()])
}

fn sh(cmd: &mut Command) -> String {
    let o = cmd.output().unwrap();
    assert!(o.status.success(), "{cmd:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

async fn call(app: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-rynas-user", "admin")
        .body(body.map(|b| Body::from(b.to_string())).unwrap_or_default())
        .unwrap();
    let r = app.clone().oneshot(req).await.unwrap();
    let s = r.status();
    (s, to_bytes(r.into_body(), 1 << 20).await.unwrap().to_vec())
}

fn uuid_of(p: &Path) -> (Option<String>, Option<String>) {
    let f = rynas_btrfs::ioctl::open_dir(p).unwrap();
    let i = rynas_btrfs::ioctl::subvol_info(&f).unwrap();
    (i.uuid, i.received_uuid)
}

#[tokio::test]
#[ignore = "root, systemd and a throwaway btrfs mount: RYNAS_TEST_BTRFS=<mount> RYNAS_AGENT_BIN=<rynas-agent>"]
async fn agent_send_into_rynasd() {
    let mnt = PathBuf::from(std::env::var("RYNAS_TEST_BTRFS").expect("RYNAS_TEST_BTRFS=<btrfs mount>"));
    let agent_bin = std::env::var("RYNAS_AGENT_BIN").expect("RYNAS_AGENT_BIN=<rynas-agent binary>");
    let (src, snaps, nas_parent) = (mnt.join("e2e-src"), mnt.join("e2e-snaps"), mnt.join("e2e-nas"));
    for p in [&src, &snaps, &nas_parent] {
        assert!(!p.exists(), "{} exists: use a fresh filesystem", p.display());
    }
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&nas_parent).unwrap();
    let data = src.join("@data");
    sh(Command::new("btrfs").args(["subvolume", "create"]).arg(&data));
    std::fs::write(data.join("a"), vec![7u8; 3 << 20]).unwrap();

    // rynasd's side, as main.rs wires it, on the real host
    let dir = tempfile::tempdir().unwrap();
    let (root, state_dir) = (dir.path().join("root"), dir.path().join("state"));
    std::fs::create_dir_all(&root).unwrap();
    let config = Arc::new(Config::open(ConfigOptions::new(&state_dir, &root), Arc::new(MemorySystem::new("nas")), Vec::new()).unwrap());
    let mut ops = Ops::new(config, TaskManager::new(dir.path().join("tasks"), "nas", TaskConfig::default()).unwrap());
    ops.admins = AdminPolicy::new(vec!["rynas-admin".into()], groups);
    let ops = Arc::new(ops);
    let notifier = Arc::new(MemoryNotifier::default());
    let clock = Arc::new(ManualClock::new(1_800_000_000_000));
    let totp = Arc::new(TotpManager::new(MemoryStorage::default(), clock.clone(), "rynas"));
    let mut st = DaemonState::new(Arc::new(NoAuth), totp, clock);
    st.ops = Some(ops.clone());
    let svc = PcBackup::new(ops.clone(), Arc::new(HostReceive), notifier.clone(), state_dir.clone(), AdminPolicy::new(vec!["rynas-admin".into()], groups), "nas".into());
    let agent = rynasd::pc_agent::PcAgent::new(
        ops.clone(),
        notifier,
        state_dir,
        true,
        rynas_backup::Launch::Direct { exe: "/nonexistent".into() },
        Arc::new(tokio::sync::Semaphore::new(1)),
        None,
        "nas".into(),
    );
    agent.bind_btrfs(svc.clone());
    st.pc_backup = Some(svc);
    st.pc_agent = Some(agent);
    let app = router(st);

    // the folder (agent: no key, no account), the pairing, the token
    let folder = nas_parent.join("pc");
    let (s, b) = call(&app, "POST", "/v1/pc-backups/targets", Some(json!({"device": "pc", "path": folder, "agent": true}))).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let id = serde_json::from_slice::<TaskStarted>(&b).unwrap().task_id;
    for _ in 0..500 {
        if ops.tasks.get(&id).is_some_and(|t| t.status.is_finished()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let t = ops.tasks.get(&id).unwrap();
    assert!(t.error.is_none(), "{:?}", t.error);
    let (s, b) = call(&app, "POST", "/v1/pcbackup/pairings", Some(json!({"btrfs_target": "pc"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    let code = serde_json::from_slice::<Value>(&b).unwrap()["code"].as_str().unwrap().to_string();
    let (s, b) = call(&app, "POST", "/v1/pcbackup/agent/pair", Some(json!({"code": code, "hostname": "pc"}))).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let token = serde_json::from_slice::<Value>(&b).unwrap()["token"].as_str().unwrap().to_string();
    let token_for_crafted = token.clone();
    // another PC's folder: hidden from this PC's receives (InaccessiblePaths in the unit)
    let other = nas_parent.join("other");
    let (s, b) = call(&app, "POST", "/v1/pc-backups/targets", Some(json!({"device": "other", "path": other, "agent": true}))).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let id = serde_json::from_slice::<TaskStarted>(&b).unwrap().task_id;
    for _ in 0..500 {
        if ops.tasks.get(&id).is_some_and(|t| t.status.is_finished()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // served as rynas-web would: the agent talks to /api/v1/…
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = axum::Router::new().nest("/api", app.clone());
    tokio::spawn(async move { axum::serve(listener, served).await.unwrap() });

    let (snaps2, data2) = (snaps.clone(), data.clone());
    let send = move |round: &str| {
        let (snaps, data) = (&snaps2, &data2);
        let out = Command::new(&agent_bin)
            .args(["btrfs-send", "--nas", &format!("http://{addr}"), "--allow-http", "--token", &token, "--snapshot-dir"])
            .arg(snaps)
            .arg("--subvol")
            .arg(format!("@data={}", data.display()))
            .output()
            .unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        println!("{round}: {text}");
        assert!(out.status.success(), "{round}: {text}");
        text
    };
    let first = tokio::task::spawn_blocking({
        let send = send.clone();
        move || send("full")
    })
    .await
    .unwrap();
    assert!(first.contains("full"), "{first}");
    std::fs::write(data.join("b"), vec![9u8; 1 << 20]).unwrap();
    // a new minute, so the second run gets another timestamp (or `_1`)
    let second = tokio::task::spawn_blocking(move || send("incremental")).await.unwrap();
    assert!(second.contains("incremental"), "{second}");

    // on the NAS: two complete snapshots, each the copy of the PC's snapshot of that name
    let (s, b) = call(&app, "GET", "/v1/pc-backups/targets/pc", None).await;
    assert_eq!(s, StatusCode::OK);
    let d: Value = serde_json::from_slice(&b).unwrap();
    let list = d["list"].as_array().unwrap();
    assert_eq!(list.len(), 2, "{list:?}");
    for x in list {
        let name = x["name"].as_str().unwrap();
        assert!(x["readonly"].as_bool().unwrap() && !x["incomplete"].as_bool().unwrap(), "{x}");
        let (pc_uuid, _) = uuid_of(&snaps.join(name));
        let (_, nas_received) = uuid_of(&folder.join(name));
        assert_eq!(pc_uuid, nas_received, "{name}: the NAS copy's received UUID is the PC snapshot's UUID");
    }
    let newest = list[0]["name"].as_str().unwrap();
    assert_eq!(std::fs::read(folder.join(newest).join("b")).unwrap().len(), 1 << 20);
    // the folder stays the agent's: 0700 root, and no staging folder is left
    let m = std::fs::metadata(&folder).unwrap();
    assert_eq!(std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777, 0o700);
    assert!(!folder.join(".incoming").exists());
    println!("ok: {list:?}");

    // hand-made streams through the whole receive path (guard, unit, check, move): each is
    // refused, and nothing of it stays in the folder
    use rynas_btrfs::send_stream::fake::*;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let mut k = 10;
    let mut name = || {
        k += 1;
        format!("@data.{}", rynas_btrfs::name::long_timestamp(now - k * 3600))
    };
    let older = list[1]["name"].as_str().unwrap().to_string();
    let u = uuid_bytes("2b1c7a3e-5d6f-4a8b-9c0d-1e2f3a4b5c6d");
    let subvol = |n: &str, extra: &[(u16, &[u8])]| {
        let mut a: Vec<(u16, &[u8])> = vec![(PATH, n.as_bytes()), (UUID, &u), (CTRANSID, &[1, 0, 0, 0, 0, 0, 0, 0])];
        a.extend_from_slice(extra);
        cmd(SUBVOL, &a)
    };
    let ino = |i: u64| i.to_le_bytes();
    let (i257, i258) = (ino(257), ino(258));
    let stream_of = |first: Vec<u8>, body: Vec<Vec<u8>>, version: u32| {
        let mut s = header(version);
        s.extend(first);
        for c in body {
            s.extend(c);
        }
        s.extend(cmd(END, &[]));
        s
    };
    let mut cases: Vec<(&str, String, Vec<u8>)> = Vec::new();
    // a symlink to "/" followed by a later command: lands next to the subvolume in the
    // staging folder (the guard cannot see it), the check afterwards refuses it
    let n = name();
    cases.push((
        "symlink escape",
        n.clone(),
        stream_of(
            subvol(&n, &[]),
            vec![
                cmd(SYMLINK, &[(PATH, b"up"), (PATH_LINK, b"/"), (3, &i257)]),
                cmd(MKFILE, &[(PATH, b"up/escaped"), (3, &i258)]),
                cmd(WRITE, &[(PATH, b"up/escaped"), (FILE_OFFSET, &0u64.to_le_bytes()), (DATA, &[7u8; 1000])]),
            ],
            1,
        ),
    ));
    // the same symlink, then a rename of an older snapshot of the PC: not reachable
    let n = name();
    let target = format!("up/{older}");
    cases.push((
        "rename of an older snapshot",
        n.clone(),
        stream_of(subvol(&n, &[]), vec![cmd(SYMLINK, &[(PATH, b"up"), (PATH_LINK, b"/"), (3, &i257)]), cmd(RENAME, &[(PATH, target.as_bytes()), (PATH_TO, b"up/gone")])], 1),
    ));
    let n = name();
    cases.push(("repeated PATH", n.clone(), stream_of(subvol(&n, &[(PATH, b"sneaky")]), vec![], 1)));
    let n = name();
    let up = format!("../{older}");
    cases.push(("'..' path", n.clone(), stream_of(subvol(&n, &[]), vec![cmd(RENAME, &[(PATH, up.as_bytes()), (PATH_TO, b"x")])], 1)));
    let n = name();
    let fall = [(PATH, &b"f"[..]), (FALLOCATE_MODE, &[0u8; 4][..]), (FILE_OFFSET, &[0u8; 8][..]), (SIZE, &(150u64 << 20).to_le_bytes()[..])];
    cases.push(("v2 stream", n.clone(), stream_of(subvol(&n, &[]), vec![cmd(MKFILE, &[(PATH, b"f"), (3, &i257)]), cmd(FALLOCATE, &fall)], 2)));
    // a block device node of the test's own loop device, then a write into it: created, but
    // the unit cannot open it (RYNAS_TEST_BLOCK=<that loop device>, optional)
    if let Ok(dev) = std::env::var("RYNAS_TEST_BLOCK") {
        use std::os::unix::fs::MetadataExt;
        let rdev = std::fs::metadata(&dev).unwrap().rdev();
        let (maj, min) = (libc::major(rdev) as u64, libc::minor(rdev) as u64);
        let n = name();
        cases.push((
            "write into a block node",
            n.clone(),
            stream_of(
                subvol(&n, &[]),
                vec![
                    cmd(MKNOD, &[(PATH, b"disk"), (MODE, &0o60600u64.to_le_bytes()), (RDEV, &((maj << 8) | min).to_le_bytes()), (3, &i257)]),
                    cmd(WRITE, &[(PATH, b"disk"), (FILE_OFFSET, &0u64.to_le_bytes()), (DATA, &[0u8; 512])]),
                ],
                1,
            ),
        ));
    }
    for (what, n, s) in cases {
        let req = Request::builder()
            .method("POST")
            .uri(format!("/v1/pcbackup/agent/btrfs/receive?snapshot={n}&parent=none"))
            .header(header::CONTENT_TYPE, "application/zstd")
            .header(header::AUTHORIZATION, format!("Bearer {}", token_for_crafted))
            .body(Body::from(zstd::stream::encode_all(&s[..], 3).unwrap()))
            .unwrap();
        let r = app.clone().oneshot(req).await.unwrap();
        let st = r.status();
        let body = String::from_utf8_lossy(&to_bytes(r.into_body(), 1 << 20).await.unwrap()).into_owned();
        println!("{what}: {st} {body}");
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{what}: {body}");
        let names: Vec<String> = std::fs::read_dir(&folder).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names.len(), 2, "{what}: {names:?}");
        assert!(names.contains(&older), "{what}: {names:?}");
    }
    println!("ok: hand-made streams refused");
}
