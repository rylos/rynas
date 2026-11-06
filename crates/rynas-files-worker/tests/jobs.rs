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

fn spec(dest: &Path, sources: Vec<PathBuf>) -> Spec {
    serde_json::from_value(json!({ "op": "copy", "sources": sources, "dest": dest })).unwrap()
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
