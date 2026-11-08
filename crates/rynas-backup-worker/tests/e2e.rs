//! End to end on a local repository in a temporary folder: create, back up twice (as a
//! fixed path, like rynasd does from a snapshot), list, browse, restore, dump one file,
//! forget, prune, check, stats, and the password never printed.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use rynas_api_types::backup::{
    RepoSpec, Retention, TestOutcome, WorkerEvent, WorkerOp, WorkerRequest, WorkerSource,
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
fn run(repo: &RepoSpec, op: WorkerOp) -> (Vec<WorkerEvent>, bool, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rynas-backup-worker"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let req = WorkerRequest {
        repo: repo.clone(),
        op,
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

fn last(r: &(Vec<WorkerEvent>, bool, String)) -> &WorkerEvent {
    r.0.last().expect("an event")
}

#[test]
fn backup_restore_forget_prune_check() {
    let t = tempfile::tempdir().unwrap();
    let repo = t.path().join("repo");
    let data = t.path().join("snap"); // stands for the read-only snapshot
    std::fs::create_dir_all(data.join("sub")).unwrap();
    std::fs::create_dir_all(data.join("#recycle")).unwrap();
    std::fs::write(data.join("a.txt"), "hello").unwrap();
    std::fs::write(data.join("sub/b.bin"), vec![7u8; 300_000]).unwrap();
    std::fs::write(data.join("#recycle/old.txt"), "gone").unwrap();
    std::fs::write(data.join("skip.tmp"), "tmp").unwrap();
    let s = spec(&repo, PW);

    // an empty folder: reachable, no repository
    let r = run(&s, WorkerOp::Test);
    assert!(matches!(last(&r), WorkerEvent::Test { outcome: TestOutcome::Empty, .. }), "{}", r.2);

    let r = run(&s, WorkerOp::Init);
    assert!(matches!(last(&r), WorkerEvent::Initialized { created: true }), "{}", r.2);
    // again: opens, nothing created
    let r = run(&s, WorkerOp::Init);
    assert!(matches!(last(&r), WorkerEvent::Initialized { created: false }), "{}", r.2);

    let backup = || WorkerOp::Backup {
        sources: vec![WorkerSource {
            read: data.display().to_string(),
            as_path: "/volume1/docs".into(),
        }],
        tags: vec!["rynas".into(), "rynas-job:docs".into()],
        host: "nas".into(),
        excludes: vec!["*.tmp".into()],
    };
    let r = run(&s, backup());
    assert!(r.1, "{}", r.2);
    let WorkerEvent::BackedUp { snapshot: first } = last(&r) else { panic!("{}", r.2) };
    assert_eq!(first.paths, vec!["/volume1/docs".to_string()]);
    assert!(r.0.iter().any(|e| matches!(e, WorkerEvent::Step { .. })));
    std::fs::write(data.join("a.txt"), "hello again").unwrap();
    let r = run(&s, backup());
    assert!(r.1, "{}", r.2);

    let r = run(&s, WorkerOp::Snapshots { tag: Some("rynas-job:docs".into()) });
    let WorkerEvent::Snapshots { list } = last(&r) else { panic!("{}", r.2) };
    assert_eq!(list.len(), 2);
    let newest = list[0].id.clone();
    assert!(list[0].time_ms >= list[1].time_ms);

    let r = run(&s, WorkerOp::Ls { snapshot: newest.clone(), path: "/volume1/docs".into() });
    let WorkerEvent::Entries { list } = last(&r) else { panic!("{}", r.2) };
    let names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"a.txt") && names.contains(&"sub"), "{names:?}");
    assert!(!names.contains(&"#recycle") && !names.contains(&"skip.tmp"), "{names:?}");
    assert!(list.iter().any(|e| e.name == "sub" && e.dir && e.path == "/volume1/docs/sub"));

    // restore everything elsewhere, keeping the path below the folder
    let dest = t.path().join("restore");
    let r = run(&s, WorkerOp::Restore {
        snapshot: first.id.clone(),
        paths: vec![],
        target: Some(dest.display().to_string()),
    });
    assert!(r.1, "{}", r.2);
    let back = dest.join("volume1/docs");
    assert_eq!(std::fs::read_to_string(back.join("a.txt")).unwrap(), "hello");
    assert_eq!(std::fs::read(back.join("sub/b.bin")).unwrap().len(), 300_000);
    // one file
    let r = run(&s, WorkerOp::Restore {
        snapshot: newest.clone(),
        paths: vec!["/volume1/docs/a.txt".into()],
        target: Some(t.path().join("one").display().to_string()),
    });
    assert!(r.1, "{}", r.2);
    assert_eq!(
        std::fs::read_to_string(t.path().join("one/volume1/docs/a.txt")).unwrap(),
        "hello again"
    );
    // download
    let out = t.path().join("dl.txt");
    let r = run(&s, WorkerOp::Dump {
        snapshot: newest.clone(),
        path: "/volume1/docs/a.txt".into(),
        out: out.display().to_string(),
    });
    assert!(matches!(last(&r), WorkerEvent::Dumped { bytes: 11 }), "{}", r.2);
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "hello again");

    // retention: preview, then keep the last one
    let keep1 = Retention { last: 1, hourly: 0, daily: 0, weekly: 0, monthly: 0, yearly: 0 };
    let r = run(&s, WorkerOp::Forget { tag: "rynas-job:docs".into(), retention: keep1, dry_run: true });
    let WorkerEvent::Forgot { keep, remove } = last(&r) else { panic!("{}", r.2) };
    assert_eq!((keep.len(), remove.len()), (1, 1));
    assert_eq!(remove[0].id, first.id);
    let r = run(&s, WorkerOp::Forget { tag: "rynas-job:docs".into(), retention: keep1, dry_run: false });
    assert!(r.1, "{}", r.2);
    // another job's tag is never touched
    let r = run(&s, WorkerOp::Forget { tag: "rynas-job:other".into(), retention: keep1, dry_run: false });
    assert!(matches!(last(&r), WorkerEvent::Forgot { remove, .. } if remove.is_empty()));
    let r = run(&s, WorkerOp::Snapshots { tag: None });
    assert!(matches!(last(&r), WorkerEvent::Snapshots { list } if list.len() == 1));

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
    let r = run(&bad, WorkerOp::Snapshots { tag: None });
    assert!(!r.1 && matches!(last(&r), WorkerEvent::Error { .. }));
    assert!(!r.2.contains("wrong-password-123"), "{}", r.2);
    assert!(!run(&s, WorkerOp::Snapshots { tag: None }).2.contains(PW));
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
