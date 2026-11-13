//! PC backup B3 end to end on real btrfs filesystems: a bare-metal restore's pool step from
//! the NAS' new send route, then the next backup of the restored PC.
//!
//! 1. `rynas-agent btrfs-send` sends a fake root subvolume (`@data`, with a layout manifest
//!    that lists it at `/`, the snapshot folder `/.btrbk_snapshots` and an excluded NOCOW
//!    subvolume) into rynasd's folder of the PC, served on 127.0.0.1;
//! 2. a restore code from the console, and `rynas-agent btrfs-restore-pool` receives it
//!    through `GET …/restore/btrfs/<pc>/send` ([`HostReceive::send`]: the folder locked
//!    shared, `btrfs send` in its read-only transient unit) into a second btrfs filesystem
//!    (the new "disk"), through the stream guard and `btrfs receive --chroot`;
//! 3. the restored pool: `@data` writable with the files, the received snapshot kept
//!    read-only in `@data/.btrbk_snapshots` with the NAS copy's received UUID, the excluded
//!    subvolume made empty with NOCOW, no staging folder left;
//! 4. `btrfs-send` from the restored pool: the NAS gets an **incremental** stream on the
//!    restored snapshot.
//!
//! Needs root, systemd and two throwaway mounted btrfs filesystems, so it is `#[ignore]`d:
//!
//! ```sh
//! cargo test -p rynasd --test pc_btrfs_restore_e2e --no-run
//! sudo RYNAS_TEST_BTRFS=/mnt/a RYNAS_TEST_POOL=/mnt/b RYNAS_AGENT_BIN=…/rynas-agent <test binary> --ignored --nocapture
//! ```

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

fn info(p: &Path) -> rynas_btrfs::ioctl::SubvolInfo {
    rynas_btrfs::ioctl::subvol_info(&rynas_btrfs::ioctl::open_dir(p).unwrap()).unwrap()
}

async fn wait_task(ops: &Ops, b: &[u8]) {
    let id = serde_json::from_slice::<TaskStarted>(b).unwrap().task_id;
    for _ in 0..500 {
        if ops.tasks.get(&id).is_some_and(|t| t.status.is_finished()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let t = ops.tasks.get(&id).unwrap();
    assert!(t.error.is_none(), "{:?}", t.error);
}

#[tokio::test]
#[ignore = "root, systemd and two throwaway btrfs mounts: RYNAS_TEST_BTRFS=<nas mount> RYNAS_TEST_POOL=<restore mount> RYNAS_AGENT_BIN=<rynas-agent>"]
async fn restore_from_the_nas_then_back_up_incrementally() {
    let mnt = PathBuf::from(std::env::var("RYNAS_TEST_BTRFS").expect("RYNAS_TEST_BTRFS=<btrfs mount>"));
    let pool = PathBuf::from(std::env::var("RYNAS_TEST_POOL").expect("RYNAS_TEST_POOL=<another btrfs mount>"));
    let agent_bin = std::env::var("RYNAS_AGENT_BIN").expect("RYNAS_AGENT_BIN=<rynas-agent binary>");
    let (src, snaps, nas_parent) = (mnt.join("b3-src"), mnt.join("b3-snaps"), mnt.join("b3-nas"));
    for p in [&src, &snaps, &nas_parent, &pool.join("@data")] {
        assert!(!p.exists(), "{} exists: use fresh filesystems", p.display());
    }
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&nas_parent).unwrap();
    let data = src.join("@data");
    sh(Command::new("btrfs").args(["subvolume", "create"]).arg(&data));
    std::fs::write(data.join("a"), vec![7u8; 3 << 20]).unwrap();
    std::fs::create_dir_all(data.join("etc")).unwrap();
    std::fs::write(data.join("etc/fstab"), "UUID=00000000-0000-4000-8000-0000000000aa / btrfs subvol=@data 0 0\n").unwrap();
    // the empty folder a nested snapshot folder leaves in the root's snapshot
    std::fs::create_dir_all(data.join(".btrbk_snapshots")).unwrap();
    std::fs::create_dir_all(data.join(".rynas")).unwrap();
    // what an older restore script's hook left (read by the legacy route): a GPT dump under
    // any name, a boot folder copy, and a symlink that must never be followed
    let bb = data.join(".bootbackup");
    std::fs::create_dir_all(bb.join("20261101T0300_post/EFI")).unwrap();
    std::fs::write(bb.join("disk-table"), "label: gpt\nlabel-id: 1E2B8F6D-3643-4D8D-AAC5-AB7EDEBECACC\ndevice: /dev/vda\nunit: sectors\nsector-size: 512\n\n/dev/vda1 : start=2048, size=262144, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, uuid=0397CC02-0000-4000-8000-000000000002\n").unwrap();
    std::os::unix::fs::symlink("/etc/shadow", bb.join("shadow-link")).unwrap();
    std::fs::write(bb.join("notes.txt"), "not a table\n").unwrap();
    let manifest = json!({
        "format": 1, "written_at_ms": 1, "hostname": "pc",
        "boot": {"uefi": true, "loader": "systemd_boot", "initramfs": "mkinitcpio"},
        "disks": [],
        "filesystems": [{
            "device": "/dev/vda2", "source": {"kind": "partition", "disk": 0, "number": 2}, "fstype": "btrfs",
            "uuid": "00000000-0000-4000-8000-0000000000aa",
            "mounts": [{"path": "/", "subvol": "@data"}],
            "subvolumes": [{"path": "@data", "captured": true}, {"path": "@cache", "captured": false, "nocow": true}]
        }],
        "btrbk": {"snapshot_dir": "/.btrbk_snapshots", "subvolumes": [{"name": "@data", "path": "@data", "mount": "/"}], "agent": true}
    });
    std::fs::write(data.join(".rynas/manifest.json"), serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

    // rynasd, as main.rs wires it, on the real host
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

    let folder = nas_parent.join("pc");
    let (s, b) = call(&app, "POST", "/v1/pc-backups/targets", Some(json!({"device": "pc", "path": folder, "agent": true}))).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    wait_task(&ops, &b).await;
    let (s, b) = call(&app, "POST", "/v1/pcbackup/pairings", Some(json!({"btrfs_target": "pc"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    let code = serde_json::from_slice::<Value>(&b).unwrap()["code"].as_str().unwrap().to_string();
    let (s, b) = call(&app, "POST", "/v1/pcbackup/agent/pair", Some(json!({"code": code, "hostname": "pc"}))).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let token = serde_json::from_slice::<Value>(&b).unwrap()["token"].as_str().unwrap().to_string();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = axum::Router::new().nest("/api", app.clone());
    tokio::spawn(async move { axum::serve(listener, served).await.unwrap() });
    let nas = format!("http://{addr}");

    let send = {
        let (agent_bin, nas, token) = (agent_bin.clone(), nas.clone(), token.clone());
        move |round: &str, snap_dir: &Path, subvol: &Path| {
            let out = Command::new(&agent_bin)
                .args(["btrfs-send", "--nas", &nas, "--allow-http", "--token", &token, "--snapshot-dir"])
                .arg(snap_dir)
                .arg("--subvol")
                .arg(format!("@data={}", subvol.display()))
                .output()
                .unwrap();
            let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            println!("{round}: {text}");
            assert!(out.status.success(), "{round}: {text}");
            text
        }
    };
    // 1. the PC's first backup
    let (s1, snaps1, data1) = (send.clone(), snaps.clone(), data.clone());
    let first = tokio::task::spawn_blocking(move || s1("full", &snaps1, &data1)).await.unwrap();
    assert!(first.contains("full"), "{first}");
    let (_, b) = call(&app, "GET", "/v1/pc-backups/targets/pc", None).await;
    let d: Value = serde_json::from_slice(&b).unwrap();
    let sent_name = d["list"][0]["name"].as_str().unwrap().to_string();
    let nas_received = info(&folder.join(&sent_name)).received_uuid;
    assert!(nas_received.is_some());

    // 2. a restore code, then the restore's pool step into the other filesystem
    let (s, b) = call(&app, "POST", "/v1/pcbackup/devices/pc/restore-code", None).await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    let rcode = serde_json::from_slice::<Value>(&b).unwrap()["code"].as_str().unwrap().to_string();
    let (agent2, nas2, pool2) = (agent_bin.clone(), nas.clone(), pool.clone());
    let restored = tokio::task::spawn_blocking(move || {
        let out = Command::new(&agent2).args(["btrfs-restore-pool", "--nas", &nas2, "--allow-http", "--code", &rcode, "--pool"]).arg(&pool2).output().unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        println!("restore: {text}");
        assert!(out.status.success(), "restore: {text}");
        text
    })
    .await
    .unwrap();
    assert!(restored.contains("pool filled"), "{restored}");

    // 3. the restored pool
    let w = pool.join("@data");
    let wi = info(&w);
    assert!(!wi.readonly, "the restored subvolume is writable");
    assert_eq!(wi.received_uuid, None, "a writable snapshot carries no received UUID");
    assert_eq!(std::fs::read(w.join("a")).unwrap().len(), 3 << 20);
    let kept = w.join(".btrbk_snapshots").join(&sent_name);
    let ki = info(&kept);
    assert!(ki.readonly, "the received snapshot stays read-only");
    assert_eq!(ki.received_uuid, nas_received, "the restored snapshot carries the NAS copy's received UUID");
    assert!(ki.generation > 0);
    assert_eq!(wi.parent_uuid, ki.uuid, "@data is a snapshot of the received one");
    let cache = pool.join("@cache");
    assert!(rynas_btrfs::ioctl::is_subvolume(&rynas_btrfs::ioctl::open_dir(&cache).unwrap()), "the excluded subvolume is made empty");
    let attrs = sh(Command::new("lsattr").arg("-d").arg(&cache));
    assert!(attrs.split_whitespace().next().unwrap_or("").contains('C'), "NOCOW: {attrs}");
    assert!(!pool.join(".rynas-restore-incoming").exists(), "no staging folder left");
    // the send unit is gone
    let unit = Command::new("systemctl").args(["is-active", "rynas-pcsend-pc.service"]).output().unwrap();
    assert!(!unit.status.success());

    // 4. the restored PC's next backup is incremental on the restored snapshot
    // incompressible, so a send of it later takes a while (the cancel below)
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let noise: Vec<u8> = (0..(64usize << 20))
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    std::fs::write(w.join("b"), &noise).unwrap();
    drop(noise);
    let (s2, sd, w2) = (send.clone(), w.join(".btrbk_snapshots"), w.clone());
    let next = tokio::task::spawn_blocking(move || s2("after restore", &sd, &w2)).await.unwrap();
    assert!(next.contains(&format!("incremental from {sent_name}")), "{next}");
    let (_, b) = call(&app, "GET", "/v1/pc-backups/targets/pc", None).await;
    let d: Value = serde_json::from_slice(&b).unwrap();
    let list = d["list"].as_array().unwrap();
    assert_eq!(list.len(), 2, "{list:?}");
    assert!(list.iter().all(|x| x["readonly"].as_bool().unwrap() && !x["incomplete"].as_bool().unwrap()));
    let newest = list[0]["name"].as_str().unwrap();
    assert_eq!(std::fs::read(folder.join(newest).join("b")).unwrap().len(), 64 << 20);
    println!("ok: restored from {sent_name}, next backup {newest} incremental");

    // 5. a restore that goes away in the middle of a send: the unit and btrfs send stop
    let (_, b) = call(&app, "POST", "/v1/pcbackup/devices/pc/restore-code", None).await;
    let rcode = serde_json::from_slice::<Value>(&b).unwrap()["code"].as_str().unwrap().to_string();
    let (s, b) = call(&app, "POST", "/v1/pcbackup/agent/restore/login", Some(json!({"code": rcode}))).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let rtok = serde_json::from_slice::<Value>(&b).unwrap()["token"].as_str().unwrap().to_string();
    // the legacy route on the real snapshot (descriptor walk, no symlink followed)
    let req = Request::builder()
        .uri(format!("/v1/pcbackup/agent/restore/btrfs/pc/legacy?snapshot={sent_name}"))
        .header(header::AUTHORIZATION, format!("Bearer {rtok}"))
        .body(Body::empty())
        .unwrap();
    let r = app.clone().oneshot(req).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let lg: rynas_api_types::pcagent::RestoreLegacy = serde_json::from_slice(&to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert!(lg.fstab.as_deref().is_some_and(|f| f.contains("subvol=@data")), "{lg:?}");
    assert_eq!(lg.boot_backups, vec!["20261101T0300_post".to_string()]);
    assert_eq!(lg.partition_dumps.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), vec!["disk-table"], "{lg:?}");
    println!("ok: legacy files read");
    let req = Request::builder()
        .uri(format!("/v1/pcbackup/agent/restore/btrfs/pc/send?snapshot={newest}"))
        .header(header::AUTHORIZATION, format!("Bearer {rtok}"))
        .body(Body::empty())
        .unwrap();
    let r = app.clone().oneshot(req).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let mut body = r.into_body().into_data_stream();
    use futures_util::StreamExt;
    let first = body.next().await.unwrap().unwrap();
    assert!(!first.is_empty());
    let active = || Command::new("systemctl").args(["is-active", "--quiet", "rynas-pcsend-pc.service"]).status().unwrap().success();
    assert!(active(), "the send runs in its unit while the client reads");
    drop(body);
    let mut gone = false;
    for _ in 0..100 {
        if !active() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(gone, "the send's unit still runs after the client went away");
    // and the PC's slot and folder are free again (once the stopped send's thread is done)
    let mut r = None;
    for _ in 0..50 {
        let req = Request::builder()
            .uri(format!("/v1/pcbackup/agent/restore/btrfs/pc/send?snapshot={sent_name}"))
            .header(header::AUTHORIZATION, format!("Bearer {rtok}"))
            .body(Body::empty())
            .unwrap();
        let x = app.clone().oneshot(req).await.unwrap();
        if x.status() != StatusCode::CONFLICT {
            r = Some(x);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let r = r.expect("the PC's send slot was never given back");
    assert_eq!(r.status(), StatusCode::OK);
    let all = to_bytes(r.into_body(), 64 << 20).await.unwrap();
    assert!(!zstd::stream::decode_all(&all[..]).unwrap().is_empty());
    println!("ok: a cancelled send stops its unit");
}
