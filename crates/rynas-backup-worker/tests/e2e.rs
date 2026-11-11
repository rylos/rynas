//! End to end on a local repository in a temporary folder: create, back up twice (as a
//! fixed path, like rynasd does from a snapshot), list, browse, restore, dump one file,
//! forget, prune, check, stats, the password never printed; snapshots scoped to a job and
//! a host (two NAS in one repository), the persistent cache, the download size limit, and
//! the private files removed on SIGINT.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use rynas_api_types::backup::{
    RepoSpec, Retention, SnapshotScope, TestOutcome, WorkerEvent, WorkerOp, WorkerRequest,
    WorkerSource,
};

const PW: &str = "correct-horse-battery-staple";

fn spec(repo: &Path, pw: &str) -> RepoSpec {
    RepoSpec {
        repository: repo.display().to_string(),
        password: pw.into(),
        ..Default::default()
    }
}

/// Runs the worker; every event line and whether it exited 0.
fn run_with(repo: &RepoSpec, op: WorkerOp, cache: Option<&Path>) -> (Vec<WorkerEvent>, bool, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rynas-backup-worker"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let req = WorkerRequest {
        repo: repo.clone(),
        op,
        cache_dir: cache.map(|c| c.display().to_string()),
    };
    child
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(&req).unwrap().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let events = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect();
    (events, out.status.success(), text)
}

fn run(repo: &RepoSpec, op: WorkerOp) -> (Vec<WorkerEvent>, bool, String) {
    run_with(repo, op, None)
}

fn last(r: &(Vec<WorkerEvent>, bool, String)) -> &WorkerEvent {
    r.0.last().expect("an event")
}

fn scope(tag: &str, host: &str) -> SnapshotScope {
    SnapshotScope {
        tag: tag.into(),
        host: host.into(),
    }
}

fn docs() -> Option<SnapshotScope> {
    Some(scope("rynas-job:docs", "nas"))
}

#[test]
fn backup_restore_forget_prune_check() {
    let t = tempfile::tempdir().unwrap();
    let repo = t.path().join("repo");
    let cache = t.path().join("cache");
    let data = t.path().join("snap"); // stands for the read-only snapshot
    std::fs::create_dir_all(data.join("sub")).unwrap();
    std::fs::create_dir_all(data.join("#recycle")).unwrap();
    std::fs::create_dir_all(data.join("anchored")).unwrap();
    std::fs::write(data.join("a.txt"), "hello").unwrap();
    std::fs::write(data.join("sub/b.bin"), vec![7u8; 300_000]).unwrap();
    std::fs::write(data.join("#recycle/old.txt"), "gone").unwrap();
    std::fs::write(data.join("skip.tmp"), "tmp").unwrap();
    std::fs::write(data.join("anchored/x"), "x").unwrap();
    let s = spec(&repo, PW);

    // an empty folder: reachable, no repository
    let r = run(&s, WorkerOp::Test);
    assert!(matches!(last(&r), WorkerEvent::Test { outcome: TestOutcome::Empty, .. }), "{}", r.2);

    let r = run(&s, WorkerOp::Init);
    assert!(matches!(last(&r), WorkerEvent::Initialized { created: true }), "{}", r.2);
    // again: opens, nothing created
    let r = run(&s, WorkerOp::Init);
    assert!(matches!(last(&r), WorkerEvent::Initialized { created: false }), "{}", r.2);

    let backup = |host: &str| WorkerOp::Backup {
        sources: vec![WorkerSource {
            read: data.display().to_string(),
            as_path: "/volume1/docs".into(),
            // an anchored exclude, already rewritten to the read path by rynasd
            excludes: vec![format!("{}/anchored/*", data.display())],
        }],
        tags: vec!["rynas".into(), "rynas-job:docs".into()],
        host: host.into(),
        excludes: vec!["*.tmp".into()],
    };
    let r = run_with(&s, backup("nas"), Some(&cache));
    assert!(r.1, "{}", r.2);
    let WorkerEvent::BackedUp { snapshot: first } = last(&r) else { panic!("{}", r.2) };
    assert_eq!(first.paths, vec!["/volume1/docs".to_string()]);
    assert!(r.0.iter().any(|e| matches!(e, WorkerEvent::Step { .. })));
    // the job's cache is kept
    assert!(std::fs::read_dir(&cache).unwrap().next().is_some());
    std::fs::write(data.join("a.txt"), "hello again").unwrap();
    let r = run_with(&s, backup("nas"), Some(&cache));
    assert!(r.1, "{}", r.2);
    // another NAS, same job name, same repository
    std::fs::write(data.join("a.txt"), "from nas2").unwrap();
    let r = run(&s, backup("nas2"));
    assert!(r.1, "{}", r.2);
    let WorkerEvent::BackedUp { snapshot: other } = last(&r) else { panic!("{}", r.2) };

    let r = run(&s, WorkerOp::Snapshots { scope: docs() });
    let WorkerEvent::Snapshots { list } = last(&r) else { panic!("{}", r.2) };
    assert_eq!(list.len(), 2);
    let newest = list[0].id.clone();
    assert!(list[0].time_ms >= list[1].time_ms);
    let r = run(&s, WorkerOp::Snapshots { scope: Some(scope("rynas-job:docs", "")) });
    assert!(matches!(last(&r), WorkerEvent::Snapshots { list } if list.len() == 3));

    let ls = |snapshot: &str, path: &str, scope: Option<SnapshotScope>| WorkerOp::Ls {
        snapshot: snapshot.into(),
        path: path.into(),
        scope,
    };
    let r = run(&s, ls(&newest, "/volume1/docs", docs()));
    let WorkerEvent::Entries { list } = last(&r) else { panic!("{}", r.2) };
    let names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"a.txt") && names.contains(&"sub"), "{names:?}");
    assert!(!names.contains(&"#recycle") && !names.contains(&"skip.tmp"), "{names:?}");
    assert!(list.iter().any(|e| e.name == "sub" && e.dir && e.path == "/volume1/docs/sub"));
    // the anchored exclude left the folder's contents out
    let r = run(&s, ls(&newest, "/volume1/docs/anchored", docs()));
    assert!(matches!(last(&r), WorkerEvent::Entries { list } if list.is_empty()), "{}", r.2);
    // an explicit id of another job is refused
    let r = run(&s, ls(&newest, "/volume1/docs", Some(scope("rynas-job:other", ""))));
    assert!(
        !r.1 && matches!(last(&r), WorkerEvent::Error { message } if message.contains("not one of this job")),
        "{}",
        r.2
    );

    // restore everything elsewhere, keeping the path below the folder
    let dest = t.path().join("restore");
    let r = run(&s, WorkerOp::Restore {
        snapshot: first.id.clone(),
        paths: vec![],
        target: Some(dest.display().to_string()),
        scope: docs(),
    });
    assert!(r.1, "{}", r.2);
    let back = dest.join("volume1/docs");
    assert_eq!(std::fs::read_to_string(back.join("a.txt")).unwrap(), "hello");
    assert_eq!(std::fs::read(back.join("sub/b.bin")).unwrap().len(), 300_000);
    // one file, from "latest" of this NAS (not the newer one of nas2)
    let r = run(&s, WorkerOp::Restore {
        snapshot: "latest".into(),
        paths: vec!["/volume1/docs/a.txt".into()],
        target: Some(t.path().join("one").display().to_string()),
        scope: docs(),
    });
    assert!(r.1, "{}", r.2);
    assert_eq!(
        std::fs::read_to_string(t.path().join("one/volume1/docs/a.txt")).unwrap(),
        "hello again"
    );
    // no target: refused
    let r = run(&s, WorkerOp::Restore {
        snapshot: "latest".into(),
        paths: vec![],
        target: None,
        scope: docs(),
    });
    assert!(!r.1);
    // download
    let dump = |out: &Path, max_bytes: u64, scope: Option<SnapshotScope>| WorkerOp::Dump {
        snapshot: "latest".into(),
        path: "/volume1/docs/a.txt".into(),
        out: out.display().to_string(),
        max_bytes,
        scope,
    };
    let out = t.path().join("dl.txt");
    let r = run(&s, dump(&out, 64, docs()));
    assert!(matches!(last(&r), WorkerEvent::Dumped { bytes: 11 }), "{}", r.2);
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "hello again");
    let out2 = t.path().join("dl2.txt");
    let r = run(&s, dump(&out2, 64, Some(scope("rynas-job:docs", "nas2"))));
    assert!(matches!(last(&r), WorkerEvent::Dumped { bytes: 9 }), "{}", r.2);
    // over the limit: refused before anything is written
    let out3 = t.path().join("dl3.txt");
    let r = run(&s, dump(&out3, 5, docs()));
    assert!(
        matches!(last(&r), WorkerEvent::Error { message } if message.starts_with("the file is too large")),
        "{}",
        r.2
    );
    assert!(!out3.exists());

    // retention: preview, then keep the last one of this NAS
    let keep1 = Retention { last: 1, hourly: 0, daily: 0, weekly: 0, monthly: 0, yearly: 0 };
    let forget = |dry_run| WorkerOp::Forget { scope: docs().unwrap(), retention: keep1, dry_run, min_age_days: 0 };
    let r = run(&s, forget(true));
    let WorkerEvent::Forgot { keep, remove } = last(&r) else { panic!("{}", r.2) };
    assert_eq!((keep.len(), remove.len()), (1, 1));
    assert_eq!(remove[0].id, first.id);
    let r = run(&s, forget(false));
    assert!(r.1, "{}", r.2);
    // another job's tag is never touched
    let r = run(&s, WorkerOp::Forget {
        scope: scope("rynas-job:other", "nas"),
        retention: keep1,
        dry_run: false,
        min_age_days: 0,
    });
    assert!(matches!(last(&r), WorkerEvent::Forgot { remove, .. } if remove.is_empty()));
    // nor is the other NAS' snapshot
    let r = run(&s, WorkerOp::Snapshots { scope: None });
    let WorkerEvent::Snapshots { list } = last(&r) else { panic!("{}", r.2) };
    assert_eq!(list.len(), 2);
    assert!(list.iter().any(|x| x.id == other.id));
    // forget without a tag: refused
    let r = run(&s, WorkerOp::Forget { scope: scope("", ""), retention: keep1, dry_run: true, min_age_days: 0 });
    assert!(!r.1);

    let r = run(&s, WorkerOp::Prune);
    assert!(matches!(last(&r), WorkerEvent::Pruned { .. }), "{}", r.2);
    for (pct, slice) in [(0, None), (100, None), (0, Some((1, 12)))] {
        let r = run(&s, WorkerOp::Check { read_percent: pct, slice });
        let WorkerEvent::Checked { errors, .. } = last(&r) else { panic!("{}", r.2) };
        assert!(errors.is_empty(), "{errors:?}");
    }
    let r = run(&s, WorkerOp::Stats);
    assert!(matches!(last(&r), WorkerEvent::Stats { bytes, .. } if *bytes > 0), "{}", r.2);

    // a wrong password: locked on test, an error elsewhere, and never echoed
    let bad = spec(&repo, "wrong-password-123");
    let r = run(&bad, WorkerOp::Test);
    assert!(matches!(last(&r), WorkerEvent::Test { outcome: TestOutcome::Locked, .. }), "{}", r.2);
    let r = run(&bad, WorkerOp::Snapshots { scope: None });
    assert!(!r.1 && matches!(last(&r), WorkerEvent::Error { .. }));
    assert!(!r.2.contains("wrong-password-123"), "{}", r.2);
    assert!(!run(&s, WorkerOp::Snapshots { scope: None }).2.contains(PW));
}

/// A PC group's repository: `init_pc`, a PC key that opens it, `index_info`, the minimum
/// age of `forget`, `stream` of a file and of a zip (data on stdout, events on stderr), a
/// removed key that no longer opens it.
#[test]
fn pc_repository_keys_stream_and_min_age() {
    let t = tempfile::tempdir().unwrap();
    let repo = t.path().join("repo");
    let data = t.path().join("pc");
    std::fs::create_dir_all(data.join("docs")).unwrap();
    std::fs::write(data.join("docs/a.txt"), "hello pc").unwrap();
    let nas = spec(&repo, PW);
    let r = run(&nas, WorkerOp::InitPc);
    assert!(matches!(last(&r), WorkerEvent::Initialized { created: true }), "{}", r.2);
    let r = run(&nas, WorkerOp::AddKey { password: "pc-password".into(), hostname: "pc".into() });
    let WorkerEvent::KeyAdded { id } = last(&r).clone() else { panic!("{}", r.2) };
    let pc = spec(&repo, "pc-password");
    let backup = WorkerOp::Backup {
        sources: vec![WorkerSource { read: data.display().to_string(), as_path: "/".into(), excludes: vec![] }],
        tags: vec!["rynas-pc".into(), "pc-laptop".into()],
        host: "laptop".into(),
        excludes: vec![],
    };
    let r = run(&pc, backup);
    assert!(matches!(last(&r), WorkerEvent::BackedUp { .. }), "{}", r.2);
    let r = run(&nas, WorkerOp::IndexInfo);
    assert!(matches!(last(&r), WorkerEvent::IndexInfo { data_blobs, .. } if *data_blobs >= 1), "{}", r.2);
    // younger than the minimum age: kept although the retention keeps nothing by count
    let r = run(&nas, WorkerOp::Forget {
        scope: scope("pc-laptop", ""),
        retention: Retention { last: 1, ..Default::default() },
        dry_run: true,
        min_age_days: 7,
    });
    assert!(matches!(last(&r), WorkerEvent::Forgot { remove, .. } if remove.is_empty()), "{}", r.2);
    // stream: raw bytes on stdout
    let stream = |zip: bool, paths: Vec<String>| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rynas-backup-worker"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let req = WorkerRequest {
            repo: nas.clone(),
            op: WorkerOp::Stream { snapshot: "latest".into(), paths, zip, scope: Some(scope("pc-laptop", "")) },
            cache_dir: None,
        };
        child.stdin.take().unwrap().write_all(serde_json::to_string(&req).unwrap().as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        out.stdout
    };
    assert_eq!(stream(false, vec!["/docs/a.txt".into()]), b"hello pc");
    let z = stream(true, vec!["/docs".into()]);
    assert_eq!(&z[..4], b"PK\x03\x04");
    assert!(z.windows(8).any(|w| w == b"hello pc"));
    // the PC's key removed: its password opens nothing
    let r = run(&nas, WorkerOp::RemoveKey { id });
    assert!(matches!(last(&r), WorkerEvent::KeyRemoved), "{}", r.2);
    let r = run(&pc, WorkerOp::Snapshots { scope: None });
    assert!(!r.1, "{}", r.2);
}

#[test]
fn invalid_request_is_an_error_line() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rynas-backup-worker"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"{nope").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    let ev: WorkerEvent = serde_json::from_slice(out.stdout.trim_ascii()).unwrap();
    assert!(matches!(ev, WorkerEvent::Error { .. }));
}

#[test]
fn sigint_removes_the_private_files() {
    // a REST server that accepts and never answers: the worker waits with its private files
    let tmp = tempfile::tempdir().unwrap();
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let mut s = spec(Path::new("/x"), PW);
    s.repository = format!("rest:http://127.0.0.1:{port}/r/");
    s.files.insert("key".into(), "PRIVATE KEY".into());
    let mut child = Command::new(env!("CARGO_BIN_EXE_rynas-backup-worker"))
        .env("TMPDIR", tmp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let req = WorkerRequest {
        repo: s,
        op: WorkerOp::Stats,
        cache_dir: None,
    };
    child
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(&req).unwrap().as_bytes())
        .unwrap();
    let dir = tmp.path().join(format!("rynas-backup-{}", child.id()));
    let start = std::time::Instant::now();
    while !dir.join("key").exists() && start.elapsed() < std::time::Duration::from_secs(10) {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(dir.join("key").exists());
    // SAFETY: a signal to our own child
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(!dir.exists());
    drop(l);
}
