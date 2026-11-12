//! Transfer jobs through the real worker binary (as the user running the tests: a process
//! that is not root starts the worker as itself).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rynas_files::identity::User;
use rynas_files::jobs::manager::{CODE_INTERRUPTED, Config, Control, Info, Manager, NoHooks};
use rynas_files::jobs::protocol::{Policy, Spec};
use serde_json::json;

fn worker() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rynas-files-worker"))
}

fn me() -> User {
    rynas_files::identity::current().expect("the test user has a passwd entry")
}

/// A copy into `dest`; the share is the folder that holds `dest`.
fn spec(dest: &Path, sources: Vec<PathBuf>) -> Spec {
    serde_json::from_value(json!({ "op": "copy", "sources": sources, "dest": dest, "roots": [dest.parent().unwrap()] })).unwrap()
}

fn control(id: u64, action: &str) -> Control {
    Control { id, action: action.into(), policy: None, all: false, bps: None, conflict: None, password: None }
}

fn wait(m: &Manager, user: &str, want: &str) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let v = m.list(user, 0);
        if v["jobs"][0]["state"] == want || Instant::now() > deadline {
            return v["jobs"][0].clone();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn interrupted_jobs_are_cleaned_and_offered_again() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(dest.join("big")).unwrap();
    // what a copy cut short leaves behind, and what is not its own
    std::fs::write(dest.join(".film.mkv.rynas-part"), "half").unwrap();
    std::fs::write(dest.join("big/.inner.rynas-part"), "half").unwrap();
    std::fs::write(dest.join("keep.txt"), "mine").unwrap();
    std::fs::create_dir_all(&journal).unwrap();
    let user = me();
    let entry = json!({
        "id": 7,
        "user": user,
        "admin": false,
        "spec": spec(&dest, vec![dir.path().join("src/big"), dir.path().join("src/film.mkv")]),
        "info": { "sources": ["/s/big"], "dest": "/d", "permanent": false },
        "created": 1,
    });
    std::fs::write(journal.join("7.json"), serde_json::to_vec(&entry).unwrap()).unwrap();
    let m = Manager::new(Config { worker: worker(), journal: journal.clone(), hooks: Arc::new(NoHooks) });
    let v = m.list(&user.name, 0);
    let job = &v["jobs"][0];
    assert_eq!(job["state"], "failed");
    assert_eq!(job["interrupted"], true);
    assert_eq!(job["code"], CODE_INTERRUPTED);
    // the clean-up runs in the background; Retry waits for it
    let deadline = Instant::now() + Duration::from_secs(20);
    while journal.join("7.json").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!journal.join("7.json").exists());
    assert!(!dest.join(".film.mkv.rynas-part").exists());
    assert!(!dest.join("big/.inner.rynas-part").exists());
    assert_eq!(std::fs::read_to_string(dest.join("keep.txt")).unwrap(), "mine");
}

#[test]
fn copies_through_a_worker_and_answers_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(src.join("a.txt"), "new").unwrap();
    std::fs::write(dest.join("a.txt"), "old").unwrap();
    let user = me();
    let m = Manager::new(Config { worker: worker(), journal: dir.path().join("journal"), hooks: Arc::new(NoHooks) });
    let info = Info { sources: vec!["/s/a.txt".into()], dest: Some("/d".into()), permanent: false };
    let id = m.submit(user.clone(), false, spec(&dest, vec![src.join("a.txt")]), info).unwrap()["id"].as_u64().unwrap();
    let job = wait(&m, &user.name, "conflict");
    assert_eq!(job["state"], "conflict", "{job}");
    assert_eq!(job["conflict"]["name"], "a.txt");
    let conflict = job["conflict"]["id"].as_u64().unwrap();
    // another user cannot touch it
    assert!(m.control("someone-else", control(id, "cancel")).is_err());
    let answer = Control { policy: Some(Policy::Overwrite), conflict: Some(conflict), ..control(id, "answer") };
    m.control(&user.name, answer).unwrap();
    let job = wait(&m, &user.name, "done");
    assert_eq!(job["state"], "done", "{job}");
    assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "new");
    assert!(job["samples"].as_array().is_some_and(|s| !s.is_empty()));
    // nothing left in the journal once done, and no temporary file in the destination
    assert_eq!(std::fs::read_dir(dir.path().join("journal")).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(&dest).unwrap().count(), 1);
    assert!(!m.busy(&user.name));
}

/// A verified copy with a file skipped: what the verification read plus what it never will
/// is the whole job, so its share of the bar reaches the end.
#[test]
fn a_verified_copy_counts_skipped_bytes_as_never_verified() {
    let (_dir, docs, _, _) = shares();
    let (src, dest) = (docs.join("src"), docs.join("dest"));
    std::fs::write(src.join("big.bin"), vec![7u8; 300_000]).unwrap();
    std::fs::write(src.join("kept.txt"), "new contents").unwrap();
    std::fs::write(dest.join("kept.txt"), "old").unwrap();
    let user = me();
    let m = Manager::new(Config { worker: worker(), journal: docs.join("journal"), hooks: Arc::new(NoHooks) });
    let job = run(&m, &user, json!({ "op": "copy", "sources": [src.join("big.bin"), src.join("kept.txt")], "dest": dest, "roots": [&docs], "conflict": "skip", "verify": true }));
    assert_eq!(job["state"], "done", "{job}");
    let p = &job["progress"];
    assert_eq!(p["skipped"], 1, "{job}");
    assert_eq!(p["verify_bytes"], 300_000, "{job}");
    assert_eq!(p["verify_skip"], "new contents".len(), "{job}");
    assert_eq!(p["bytes_done"], p["bytes_total"], "{job}");
}

#[test]
fn cancelling_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(src.join("big.bin"), vec![7u8; 8 * 1024 * 1024]).unwrap();
    let user = me();
    let m = Manager::new(Config { worker: worker(), journal: dir.path().join("journal"), hooks: Arc::new(NoHooks) });
    let info = Info { sources: vec!["/s/big.bin".into()], dest: Some("/d".into()), permanent: false };
    let mut s = spec(&dest, vec![src.join("big.bin")]);
    // 1 MB/s: slow enough to cancel in the middle
    s.limit = 1024 * 1024;
    let id = m.submit(user.clone(), false, s, info).unwrap()["id"].as_u64().unwrap();
    wait(&m, &user.name, "running");
    std::thread::sleep(Duration::from_millis(300));
    m.control(&user.name, control(id, "cancel")).unwrap();
    let job = wait(&m, &user.name, "cancelled");
    assert_eq!(job["state"], "cancelled", "{job}");
    assert_eq!(job["code"], "cancelled");
    assert_eq!(std::fs::read_dir(&dest).unwrap().count(), 0, "a cancelled copy leaves no .part file");
}

/// An SMB server on the loopback that takes the connection and what is sent, and never
/// answers (a server behind a VPN that stopped passing its replies).
fn silent_server() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut s, &mut std::io::sink());
            });
        }
    });
    port
}

#[test]
fn a_copy_stuck_on_a_silent_server_shows_it_and_cancels_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&dest).unwrap();
    let user = me();
    let m = Manager::new(Config { worker: worker(), journal: dir.path().join("journal"), hooks: Arc::new(NoHooks) });
    let info = Info { sources: vec!["/:fake/iso/big.iso".into()], dest: Some("/d".into()), permanent: false };
    let s: Spec = serde_json::from_value(json!({
        "op": "copy",
        "sources": ["/:fake/iso/big.iso"],
        "dest": dest,
        "roots": [dir.path()],
        "remotes": [{ "id": "fake", "name": "fake", "kind": "smb", "host": "127.0.0.1", "port": silent_server(), "user": "u", "password": "p" }],
    }))
    .unwrap();
    let id = m.submit(user.clone(), false, s, info).unwrap()["id"].as_u64().unwrap();
    // the worker waits on the server: the job says so instead of waiting in silence
    let deadline = Instant::now() + Duration::from_secs(20);
    let job = loop {
        let job = m.list(&user.name, 0)["jobs"][0].clone();
        if job["stalled_s"].as_u64().unwrap_or(0) >= 5 || Instant::now() > deadline {
            break job;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(job["state"], "running", "{job}");
    assert!(job["stalled_s"].as_u64().unwrap_or(0) >= 5, "{job}");
    // cancel works while the worker is blocked on the network
    let asked = Instant::now();
    m.control(&user.name, control(id, "cancel")).unwrap();
    assert_eq!(m.list(&user.name, 0)["jobs"][0]["cancelling"], true);
    let job = wait(&m, &user.name, "cancelled");
    assert_eq!(job["state"], "cancelled", "{job}");
    assert_eq!(job["code"], "cancelled");
    assert!(asked.elapsed() < Duration::from_secs(8), "cancel took {:?}", asked.elapsed());
    assert!(job.get("cancelling").is_none() && job.get("stalled_s").is_none(), "{job}");
    assert_eq!(std::fs::read_dir(&dest).unwrap().count(), 0);
}

/// Runs a job to its end and returns its view.
fn run(m: &Manager, user: &User, spec: serde_json::Value) -> serde_json::Value {
    let spec: Spec = serde_json::from_value(spec).unwrap();
    let info = Info { sources: vec!["/s".into()], dest: Some("/d".into()), permanent: false };
    let id = m.submit(user.clone(), false, spec, info).unwrap()["id"].as_u64().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let v = m.list(&user.name, 0);
        let job = v["jobs"].as_array().unwrap().iter().find(|j| j["id"] == id).cloned().unwrap();
        if matches!(job["state"].as_str(), Some("done" | "failed" | "cancelled")) || Instant::now() > deadline {
            return job;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn error_codes_of(job: &serde_json::Value) -> Vec<String> {
    job["errors"].as_array().unwrap().iter().map(|e| e["code"].as_str().unwrap_or_default().to_string()).collect()
}

/// Shares "docs" and "other" side by side, and "sys" beside them standing for the system.
fn shares() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let (docs, other, sys) = (base.join("docs"), base.join("other"), base.join("sys"));
    for d in [docs.join("src/sub"), docs.join("dest"), other.clone(), sys.clone()] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(sys.join("shadow"), "root:secret").unwrap();
    std::fs::write(docs.join("src/a.txt"), "a").unwrap();
    std::os::unix::fs::symlink("a.txt", docs.join("src/inside")).unwrap();
    std::os::unix::fs::symlink(&sys, docs.join("src/sub/out")).unwrap();
    std::os::unix::fs::symlink(&sys, docs.join("etc")).unwrap();
    std::os::unix::fs::symlink(&other, docs.join("to-other")).unwrap();
    (dir, docs, other, sys)
}

#[test]
fn copies_never_follow_links_out_of_the_share() {
    let (dir, docs, other, sys) = shares();
    let user = me();
    let m = Manager::new(Config { worker: worker(), journal: dir.path().join("journal"), hooks: Arc::new(NoHooks) });
    let roots = json!([docs, other]);
    // a source reached through a link to the system: refused, nothing copied
    let job = run(&m, &user, json!({ "op": "copy", "sources": [docs.join("etc/shadow")], "dest": docs.join("dest"), "roots": roots }));
    assert_eq!(error_codes_of(&job), vec!["outside_share"], "{job}");
    assert!(!docs.join("dest/shadow").exists());
    // a folder: links inside it are copied as links, whatever they point to
    let job = run(&m, &user, json!({ "op": "copy", "sources": [docs.join("src")], "dest": docs.join("dest"), "roots": roots }));
    assert_eq!(job["state"], "done", "{job}");
    assert_eq!(std::fs::read_link(docs.join("dest/src/inside")).unwrap(), Path::new("a.txt"));
    assert_eq!(std::fs::read_link(docs.join("dest/src/sub/out")).unwrap(), sys);
    // the link itself, selected: copied as a link
    let job = run(&m, &user, json!({ "op": "copy", "sources": [docs.join("etc")], "dest": docs.join("dest"), "roots": roots }));
    assert_eq!(job["state"], "done", "{job}");
    assert_eq!(std::fs::read_link(docs.join("dest/etc")).unwrap(), sys);
    // a destination through a link to another share: the job fails, nothing written there
    let job = run(&m, &user, json!({ "op": "copy", "sources": [docs.join("src/a.txt")], "dest": docs.join("to-other"), "roots": roots }));
    assert_eq!((job["state"].as_str(), job["code"].as_str()), (Some("failed"), Some("outside_share")), "{job}");
    assert_eq!(std::fs::read_dir(&other).unwrap().count(), 0);
    // a path no share of the job holds
    let job = run(&m, &user, json!({ "op": "copy", "sources": [sys.join("shadow")], "dest": docs.join("dest"), "roots": [docs] }));
    assert_eq!(error_codes_of(&job), vec!["outside_share"], "{job}");
    // compressing a folder stores its links as links
    let job = run(&m, &user, json!({ "op": "compress", "sources": [docs.join("src")], "dest": docs.join("dest"), "name": "s", "format": "tar.zst", "roots": roots }));
    assert_eq!(job["state"], "done", "{job}");
    assert_eq!(std::fs::read_to_string(sys.join("shadow")).unwrap(), "root:secret");
}

#[test]
fn extraction_never_writes_through_a_link() {
    if std::process::Command::new("tar").arg("--version").output().is_err() {
        return;
    }
    let (dir, docs, _, sys) = shares();
    // an archive with links out (absolute and relative) and then files below them
    let pack = dir.path().join("pack");
    std::fs::create_dir_all(&pack).unwrap();
    std::os::unix::fs::symlink(&sys, pack.join("abs")).unwrap();
    std::os::unix::fs::symlink("../../sys", pack.join("rel")).unwrap();
    let evil = docs.join("evil.tar");
    let tar = |args: &[&str]| {
        let ok = std::process::Command::new("tar").arg(args[0]).arg(&evil).arg("-C").arg(&pack).args(&args[1..]).status().unwrap();
        assert!(ok.success());
    };
    tar(&["-cf", "abs", "rel"]);
    for d in ["abs", "rel"] {
        std::fs::remove_file(pack.join(d)).unwrap();
        std::fs::create_dir_all(pack.join(d)).unwrap();
        std::fs::write(pack.join(d).join("pwned"), "x").unwrap();
    }
    tar(&["-rf", "abs/pwned", "rel/pwned"]);
    let user = me();
    let m = Manager::new(Config { worker: worker(), journal: dir.path().join("journal"), hooks: Arc::new(NoHooks) });
    let job = run(&m, &user, json!({ "op": "extract", "sources": [evil], "dest": docs.join("dest"), "conflict": "overwrite", "roots": [docs] }));
    assert!(!sys.join("pwned").exists(), "written through a link: {job}");
    assert_eq!(std::fs::read_dir(&sys).unwrap().count(), 1);
    // the extraction folder never stays behind
    assert!(std::fs::read_dir(docs.join("dest")).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with(".rynas-x-")));
}

#[test]
fn the_recycle_bin_is_never_a_link() {
    let (dir, docs, _, sys) = shares();
    std::os::unix::fs::symlink(&sys, docs.join("#recycle")).unwrap();
    let user = me();
    let m = Manager::new(Config { worker: worker(), journal: dir.path().join("journal"), hooks: Arc::new(NoHooks) });
    let job = run(&m, &user, json!({ "op": "delete", "sources": [docs.join("src/a.txt")], "recycle": [{ "name": "docs", "path": docs }], "roots": [docs] }));
    assert_eq!(job["errors"].as_array().unwrap().len(), 1, "{job}");
    assert!(docs.join("src/a.txt").exists(), "kept when the bin cannot take it");
    assert_eq!(std::fs::read_dir(&sys).unwrap().count(), 1);
}
