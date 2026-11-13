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
//! The second test covers the review fixes: two subvolumes with the snapshot folder inside
//! `@`, an older snapshot of `@home` restored (the next backup still incremental), and a
//! genfstab fstab with `subvolid=` that must mount after the restore.
//!
//! Needs root, systemd and two throwaway mounted btrfs filesystems (top levels), so it is
//! `#[ignore]`d; both tests use the same two mounts, one after the other:
//!
//! ```sh
//! cargo test -p rynasd --test pc_btrfs_restore_e2e --no-run
//! sudo RYNAS_TEST_BTRFS=/mnt/a RYNAS_TEST_POOL=/mnt/b RYNAS_AGENT_BIN=…/rynas-agent <test binary> --ignored --nocapture --test-threads=1
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

mod common;
use common::*;

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
    let n = Nas::new();
    let app = n.app.clone();
    let folder = nas_parent.join("pc");
    n.folder("pc", &folder).await;
    let token = n.pair("pc").await;
    let nas = n.serve().await;

    let send = {
        let (agent_bin, nas, token) = (agent_bin.clone(), nas.clone(), token.clone());
        move |round: &str, snap_dir: &Path, subvol: &Path| agent_send(&agent_bin, &nas, &token, round, snap_dir, &[("@data", subvol)])
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
    let rcode = n.restore_code("pc").await;
    let (agent2, nas2, pool2) = (agent_bin.clone(), nas.clone(), pool.clone());
    let restored = tokio::task::spawn_blocking(move || agent_restore_pool(&agent2, &nas2, &rcode, &pool2, &[])).await.unwrap();
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
    assert!(!unit_active("rynas-pcsend-pc-"));

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
    let rcode = n.restore_code("pc").await;
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
    let active = || unit_active("rynas-pcsend-pc-");
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

/// Review fixes, on real btrfs: a PC with `@` and `@home`, both snapshotted into
/// `/.btrbk_snapshots` **inside `@`**, and a genfstab-style fstab with `subvolid=`.
///
/// - The restore takes the latest run of `@` and an **older** snapshot of `@home`: the
///   received `@home` snapshot must land in `@/.btrbk_snapshots` of the restored `@` (the
///   review feared an empty folder there, the place a nested snapshot leaves in a local
///   snapshot, would keep it in the staging folder and make the next backup a full one).
/// - The next backup of the restored `@home` is incremental on it.
/// - The restored fstab has no `subvolid=` left: its lines mount on the new filesystem
///   (the old line does not: the IDs changed).
///
/// Uses fresh paths of the same two mounts as the test above (`--test-threads=1`).
#[tokio::test]
#[ignore = "root, systemd and two throwaway btrfs mounts: RYNAS_TEST_BTRFS=<nas mount> RYNAS_TEST_POOL=<restore mount> RYNAS_AGENT_BIN=<rynas-agent>"]
async fn older_nested_snapshot_and_subvolid_fstab() {
    let mnt = PathBuf::from(std::env::var("RYNAS_TEST_BTRFS").expect("RYNAS_TEST_BTRFS=<btrfs mount>"));
    let pool = PathBuf::from(std::env::var("RYNAS_TEST_POOL").expect("RYNAS_TEST_POOL=<another btrfs mount>"));
    let agent_bin = std::env::var("RYNAS_AGENT_BIN").expect("RYNAS_AGENT_BIN=<rynas-agent binary>");
    let (src, nas_parent) = (mnt.join("b3r-src"), mnt.join("b3r-nas"));
    for p in [&src, &nas_parent, &pool.join("@"), &pool.join("@home")] {
        assert!(!p.exists(), "{} exists: use fresh filesystems", p.display());
    }
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&nas_parent).unwrap();
    let (root, home) = (src.join("@"), src.join("@home"));
    for s in [&root, &home] {
        sh(Command::new("btrfs").args(["subvolume", "create"]).arg(s));
    }
    let snap_dir = root.join(".btrbk_snapshots");
    std::fs::create_dir_all(&snap_dir).unwrap();
    std::fs::create_dir_all(root.join("etc")).unwrap();
    std::fs::create_dir_all(root.join(".rynas")).unwrap();
    std::fs::write(home.join("file"), "home v1").unwrap();
    // what genfstab writes: subvolid next to subvol, and a line with subvolid alone
    let fs_uuid = "00000000-0000-4000-8000-0000000000bb";
    std::fs::write(root.join("etc/fstab"), format!("UUID={fs_uuid}\t/\tbtrfs\trw,relatime,subvolid=256,subvol=/@\t0 0\nUUID={fs_uuid}\t/home\tbtrfs\trw,relatime,subvolid=257\t0 0\n")).unwrap();
    let manifest = json!({
        "format": 1, "written_at_ms": 1, "hostname": "pc2",
        "boot": {"uefi": true, "loader": "systemd_boot", "initramfs": "mkinitcpio"},
        "disks": [],
        "filesystems": [{
            "device": "/dev/vda2", "source": {"kind": "partition", "disk": 0, "number": 2}, "fstype": "btrfs",
            "uuid": fs_uuid,
            "mounts": [{"path": "/", "subvol": "@"}, {"path": "/home", "subvol": "@home"}],
            "subvolumes": [{"path": "@", "id": 256, "captured": true}, {"path": "@home", "id": 257, "captured": true}]
        }],
        "btrbk": {"snapshot_dir": "/.btrbk_snapshots", "subvolumes": [{"name": "@", "path": "@", "mount": "/"}, {"name": "@home", "path": "@home", "mount": "/home"}], "agent": true}
    });
    std::fs::write(root.join(".rynas/manifest.json"), serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

    let n = Nas::new();
    let app = n.app.clone();
    let folder = nas_parent.join("pc2");
    n.folder("pc2", &folder).await;
    let token = n.pair("pc2").await;
    let nas = n.serve().await;
    let send = {
        let (agent_bin, nas, token, snap_dir) = (agent_bin.clone(), nas.clone(), token.clone(), snap_dir.clone());
        move |round: &str, subvols: Vec<(&'static str, PathBuf)>| {
            let s: Vec<(&str, &Path)> = subvols.iter().map(|(n, p)| (*n, p.as_path())).collect();
            agent_send(&agent_bin, &nas, &token, round, &snap_dir, &s)
        }
    };
    // two runs; @home changes in between
    let (s1, r1, h1) = (send.clone(), root.clone(), home.clone());
    tokio::task::spawn_blocking(move || s1("run 1", vec![("@", r1), ("@home", h1)])).await.unwrap();
    std::fs::write(home.join("file"), "home v2").unwrap();
    let (s2, r2, h2) = (send.clone(), root.clone(), home.clone());
    tokio::task::spawn_blocking(move || s2("run 2", vec![("@", r2), ("@home", h2)])).await.unwrap();
    let (_, b) = call(&app, "GET", "/v1/pc-backups/targets/pc2", None).await;
    let d: Value = serde_json::from_slice(&b).unwrap();
    let names: Vec<String> = d["list"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap().to_string()).collect();
    let of = |g: &str| {
        let mut v: Vec<&String> = names.iter().filter(|x| x.starts_with(&format!("{g}."))).collect();
        v.sort();
        v.into_iter().cloned().collect::<Vec<_>>()
    };
    let (roots, homes) = (of("@"), of("@home"));
    assert_eq!((roots.len(), homes.len()), (2, 2), "{names:?}");
    let older_home = homes[0].clone();
    // the PC's own newest @ snapshot holds an empty folder where the older @home snapshot
    // was (a nested subvolume); `btrfs send` leaves such folders out (kernel 7.3), the
    // restore makes way for one either way
    let local = snap_dir.join(&roots[1]).join(".btrbk_snapshots").join(&older_home);
    assert!(local.is_dir() && std::fs::read_dir(&local).unwrap().next().is_none(), "{} is not an empty folder", local.display());
    let received = folder.join(&roots[1]).join(".btrbk_snapshots").join(&older_home);
    println!("the received {} holds the empty folder: {}", roots[1], received.exists());
    let nas_received = info(&folder.join(&older_home)).received_uuid;

    // the restore: latest run, the older @home
    let rcode = n.restore_code("pc2").await;
    let (agent2, nas2, pool2, pick) = (agent_bin.clone(), nas.clone(), pool.clone(), format!("@home={older_home}"));
    let out = tokio::task::spawn_blocking(move || agent_restore_pool(&agent2, &nas2, &rcode, &pool2, &["--subvol", &pick])).await.unwrap();
    assert!(out.contains("pool filled"), "{out}");
    assert_eq!(std::fs::read_to_string(pool.join("@home/file")).unwrap(), "home v1");
    let kept = pool.join("@/.btrbk_snapshots").join(&older_home);
    let ki = info(&kept);
    assert!(ki.readonly, "{} is not the received snapshot", kept.display());
    assert_eq!(ki.received_uuid, nas_received);
    assert!(!pool.join(".rynas-restore-incoming").exists(), "no staging folder left: {out}");
    println!("ok: the older @home snapshot is in @/.btrbk_snapshots");

    // the fstab: no subvolid left; the new lines mount, the old one does not
    let fstab = std::fs::read_to_string(pool.join("@/etc/fstab")).unwrap();
    println!("restored fstab:\n{fstab}");
    assert!(!fstab.contains("subvolid="), "{fstab}");
    let opts: Vec<String> = fstab.lines().map(|l| l.split_whitespace().nth(3).unwrap().to_string()).collect();
    assert_eq!(opts, vec!["rw,relatime,subvol=/@".to_string(), "rw,relatime,subvol=/@home".to_string()]);
    let dev = sh(Command::new("findmnt").args(["-nvo", "SOURCE", "--target"]).arg(&pool)).trim().to_string();
    let at = tempfile::tempdir().unwrap();
    let try_mount = |o: &str| Command::new("mount").args(["-t", "btrfs", "-o", &format!("{o},ro"), &dev]).arg(at.path()).status().unwrap().success();
    let umount = || sh(Command::new("umount").arg(at.path()));
    assert!(try_mount(&opts[1]), "the fixed /home line does not mount");
    assert_eq!(std::fs::read_to_string(at.path().join("file")).unwrap(), "home v1");
    umount();
    assert!(try_mount(&opts[0]), "the fixed / line does not mount");
    assert!(at.path().join("etc/fstab").is_file());
    umount();
    assert!(!try_mount("rw,relatime,subvolid=256,subvol=/@"), "the PC's old line mounted: the IDs did not change?");
    println!("ok: the fixed fstab lines mount");

    // the next backup of the restored @home is incremental on the restored snapshot
    std::fs::write(pool.join("@home/file"), "home v3").unwrap();
    let (agent3, nas3, token3, sd3, h3) = (agent_bin.clone(), nas.clone(), token.clone(), pool.join("@/.btrbk_snapshots"), pool.join("@home"));
    let next = tokio::task::spawn_blocking(move || agent_send(&agent3, &nas3, &token3, "after restore", &sd3, &[("@home", h3.as_path())])).await.unwrap();
    assert!(next.contains(&format!("incremental from {older_home}")), "{next}");
    println!("ok: the next backup of @home is incremental on {older_home}");
}
