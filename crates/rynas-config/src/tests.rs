//! Store tests on a scratch root with the in-memory system.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rynas_api_types::{DiffOp, DriftStatus};
use serde_json::json;
use tempfile::TempDir;

use crate::archive::read_archive;
use crate::*;

struct Env {
    _dir: TempDir,
    root: PathBuf,
    state: PathBuf,
    sys: Arc<MemorySystem>,
    cfg: Config,
}

fn appliers() -> Vec<Arc<dyn Applier>> {
    vec![
        Arc::new(HostnameDomain),
        Arc::new(
            FileDomain::new(
                "test.app",
                "/etc/app/app.conf",
                "port = {{ port }}\nname = {{ name }}\n",
            )
            .unit("app.service")
            .validator(&["app-check", "-s", "{}"]),
        ),
    ]
}

fn env_with(opts: impl FnOnce(&mut ConfigOptions)) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let state = dir.path().join("state");
    std::fs::create_dir_all(root.join("etc")).unwrap();
    std::fs::write(root.join("etc/hostname"), "debian\n").unwrap();
    std::fs::write(
        root.join("etc/hosts"),
        "127.0.0.1\tlocalhost\n127.0.1.1\tdebian\n::1\tlocalhost ip6-localhost\n",
    )
    .unwrap();
    let sys = Arc::new(MemorySystem::new("debian"));
    let mut o = ConfigOptions::new(&state, &root);
    opts(&mut o);
    let cfg = Config::open(o, sys.clone(), appliers()).unwrap();
    Env {
        _dir: dir,
        root,
        state,
        sys,
        cfg,
    }
}

fn env() -> Env {
    env_with(|_| {})
}

fn read(root: &Path, p: &str) -> String {
    std::fs::read_to_string(root.join(p.trim_start_matches('/'))).unwrap()
}

fn apply(e: &Env, owner: &str, comment: &str) -> Result<u64> {
    let lines = Mutex::new(Vec::<String>::new());
    e.cfg.apply(
        owner,
        owner,
        comment,
        &|l| lines.lock().unwrap().push(l.into()),
        &|| false,
    )
}

fn hostname(h: &str) -> serde_json::Value {
    json!({ "hostname": h })
}

#[test]
fn current_value_is_imported_from_the_system() {
    let e = env();
    assert_eq!(e.cfg.applied(HOSTNAME_KEY).unwrap(), None);
    assert_eq!(
        e.cfg.current(HOSTNAME_KEY).unwrap(),
        Some(hostname("debian"))
    );
    assert!(matches!(
        e.cfg.stage("alice", "no.such", Some(json!({}))),
        Err(ConfigError::UnknownKey(_))
    ));
}

#[test]
fn stage_validates_normalizes_and_drops_no_op_changes() {
    let e = env();
    assert!(matches!(
        e.cfg.stage("alice", HOSTNAME_KEY, Some(hostname("-bad"))),
        Err(ConfigError::Invalid(_))
    ));
    assert!(
        e.cfg
            .stage("alice", HOSTNAME_KEY, Some(hostname(" NAS ")))
            .unwrap()
    );
    let p = e.cfg.pending("alice").unwrap();
    assert_eq!(p.changes.len(), 1);
    let c = &p.changes[0];
    assert_eq!(c.value, Some(hostname("nas")));
    assert_eq!(c.base, Some(hostname("debian")));
    assert!(!c.conflict);
    assert_eq!(c.diff.len(), 1);
    assert_eq!(c.diff[0].path, "system.hostname.hostname");
    assert_eq!(c.diff[0].op, DiffOp::Changed);
    // per owner
    assert!(e.cfg.pending("bob").unwrap().changes.is_empty());
    // staging the current value again removes the change
    assert!(
        !e.cfg
            .stage("alice", HOSTNAME_KEY, Some(hostname("debian")))
            .unwrap()
    );
    assert!(e.cfg.pending("alice").unwrap().changes.is_empty());
    e.cfg
        .stage("alice", HOSTNAME_KEY, Some(hostname("nas")))
        .unwrap();
    assert_eq!(e.cfg.discard("alice").unwrap(), 1);
    assert_eq!(apply(&e, "alice", ""), Err(ConfigError::NothingPending));
}

#[test]
fn apply_value_without_changeset_makes_staged_changes_conflict() {
    let e = env();
    e.cfg
        .stage("alice", HOSTNAME_KEY, Some(hostname("staged")))
        .unwrap();
    let rev = e
        .cfg
        .apply_value(
            "bob",
            HOSTNAME_KEY,
            Some(hostname(" Direct ")),
            "by a task",
            &|_| {},
        )
        .unwrap();
    assert_eq!(read(&e.root, "/etc/hostname"), "direct\n");
    assert_eq!(e.sys.hostname().unwrap(), "direct");
    let r = &e.cfg.revisions(1).unwrap()[0];
    assert_eq!(
        (r.id, r.author.as_str(), r.comment.as_str()),
        (rev, "bob", "by a task")
    );
    // alice's change was based on the old value
    let p = e.cfg.pending("alice").unwrap();
    assert!(p.changes[0].conflict);
    // invalid values are refused before anything happens
    assert!(matches!(
        e.cfg
            .apply_value("bob", HOSTNAME_KEY, Some(hostname("-x")), "", &|_| {}),
        Err(ConfigError::Invalid(_))
    ));
}

#[test]
fn apply_update_reads_and_applies_inside_the_apply_lock() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let e = env();
    e.cfg
        .apply_value("a", HOSTNAME_KEY, Some(hostname("one")), "", &|_| {})
        .unwrap();
    let other_done = AtomicBool::new(false);
    std::thread::scope(|sc| {
        let rev = e
            .cfg
            .apply_update("b", HOSTNAME_KEY, "update", &|_| {}, |cur| {
                assert_eq!(cur, Some(hostname("one")));
                // an apply started between the read and the apply waits for this one: it
                // cannot be reverted by the value computed from the read
                sc.spawn(|| {
                    e.cfg
                        .apply_value("c", HOSTNAME_KEY, Some(hostname("three")), "", &|_| {})
                        .unwrap();
                    other_done.store(true, Ordering::SeqCst);
                });
                std::thread::sleep(std::time::Duration::from_millis(300));
                assert!(!other_done.load(Ordering::SeqCst));
                Ok(Some(hostname("two")))
            })
            .unwrap();
        assert!(rev.is_some());
    });
    assert!(other_done.load(Ordering::SeqCst));
    assert_eq!(e.cfg.applied(HOSTNAME_KEY).unwrap(), Some(hostname("three")));
    let authors: Vec<String> = e
        .cfg
        .revisions(10)
        .unwrap()
        .into_iter()
        .map(|r| r.author)
        .collect();
    assert_eq!(authors, ["c", "b", "a"]);
    // nothing to change: nothing applied
    assert_eq!(
        e.cfg
            .apply_update("b", HOSTNAME_KEY, "", &|_| {}, |_| Ok(None))
            .unwrap(),
        None
    );
    assert_eq!(e.cfg.revisions(10).unwrap().len(), 3);
}

#[test]
fn apply_writes_files_reloads_and_records_a_revision() {
    let e = env();
    e.cfg
        .stage("alice", HOSTNAME_KEY, Some(hostname("nas")))
        .unwrap();
    let rev = apply(&e, "alice", "rename").unwrap();
    assert_eq!(rev, 1);
    assert_eq!(read(&e.root, "/etc/hostname"), "nas\n");
    let hosts = read(&e.root, "/etc/hosts");
    assert!(hosts.contains("127.0.0.1\tlocalhost\n"));
    assert!(hosts.contains("# BEGIN rynas hostname "));
    assert!(hosts.contains("127.0.1.1\tnas\n"));
    assert!(!hosts.contains("debian"));
    assert_eq!(e.sys.state().hostname, "nas");
    assert_eq!(e.sys.state().calls, ["set-hostname nas"]);
    assert!(!e.root.join("etc/hostname.rynas-new").exists());
    // the previous files are kept with the revision
    assert_eq!(
        std::fs::read_to_string(e.state.join("backup/1/files/etc/hostname")).unwrap(),
        "debian\n"
    );
    assert!(e.cfg.pending("alice").unwrap().changes.is_empty());
    assert_eq!(e.cfg.applied(HOSTNAME_KEY).unwrap(), Some(hostname("nas")));

    let revs = e.cfg.revisions(10).unwrap();
    assert_eq!(revs.len(), 1);
    assert_eq!(revs[0].author, "alice");
    assert_eq!(revs[0].comment, "rename");
    assert_eq!(revs[0].keys, [HOSTNAME_KEY]);
    let d = e.cfg.revision(1).unwrap();
    assert_eq!(d.diff.len(), 1);
    assert_eq!(d.diff[0].old, Some(json!("debian")));
    assert_eq!(d.diff[0].new, Some(json!("nas")));
    assert_eq!(d.files, ["/etc/hostname", "/etc/hosts"]);
    assert!(matches!(e.cfg.revision(9), Err(ConfigError::NotFound(_))));
    assert!(e.cfg.drift().unwrap().entries.is_empty());
    assert_eq!(e.cfg.drift().unwrap().checked, 2);
}

#[test]
fn apply_keys_applies_only_the_selected_keys() {
    let e = env();
    e.cfg
        .stage("alice", HOSTNAME_KEY, Some(hostname("nas")))
        .unwrap();
    e.cfg
        .stage("alice", "test.app", Some(json!({"port": 8080, "name": "demo"})))
        .unwrap();
    let keys = |k: &[&str]| k.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let run = |k: &[String]| e.cfg.apply_keys("alice", "alice", "", k, &|_| {}, &|| false);
    // keys that are not staged select nothing
    assert_eq!(run(&keys(&["no.such"])), Err(ConfigError::NothingPending));
    // a key that is not staged next to one that is: only the staged one runs
    let rev = run(&keys(&["test.app", "no.such"])).unwrap();
    assert_eq!(e.cfg.revision(rev).unwrap().revision.keys, ["test.app"]);
    assert!(read(&e.root, "/etc/app/app.conf").contains("port = 8080"));
    // the other key stays pending and the system keeps its old value
    let p = e.cfg.pending("alice").unwrap();
    assert_eq!(p.changes.len(), 1);
    assert_eq!(p.changes[0].key, HOSTNAME_KEY);
    assert_eq!(read(&e.root, "/etc/hostname"), "debian\n");
    assert_eq!(run(&keys(&["test.app"])), Err(ConfigError::NothingPending));
    // a conflict on another key does not block the selected one
    e.cfg
        .stage("bob", HOSTNAME_KEY, Some(hostname("bravo")))
        .unwrap();
    e.cfg
        .stage("alice", "test.app", Some(json!({"port": 9090, "name": "demo"})))
        .unwrap();
    apply(&e, "bob", "").unwrap();
    assert!(run(&keys(&["test.app"])).is_ok());
    assert_eq!(
        run(&keys(&[HOSTNAME_KEY])),
        Err(ConfigError::Conflict(vec![HOSTNAME_KEY.to_string()]))
    );
}

#[test]
fn keys_staged_together_are_never_split_by_a_partial_apply() {
    let e = env();
    let keys = |k: &[&str]| k.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let run = |k: &[String]| e.cfg.apply_keys("alice", "alice", "", k, &|_| {}, &|| false);
    let both = || {
        e.cfg
            .stage("alice", HOSTNAME_KEY, Some(hostname("nas")))
            .unwrap();
        e.cfg
            .stage("alice", "test.app", Some(json!({"port": 8080, "name": "demo"})))
            .unwrap();
    };
    // a plain group: only some of its keys is refused, all of them works
    both();
    e.cfg
        .stage_group("alice", &[HOSTNAME_KEY, "test.app"], false)
        .unwrap();
    let split = || ConfigError::GroupSplit(keys(&[HOSTNAME_KEY, "test.app"]));
    assert_eq!(run(&keys(&["test.app"])), Err(split()));
    assert_eq!(e.cfg.pending("alice").unwrap().changes.len(), 2);
    assert_eq!(e.cfg.check_groups("alice", &keys(&[HOSTNAME_KEY])), Err(split()));
    run(&keys(&[HOSTNAME_KEY, "test.app"])).unwrap();
    assert!(e.cfg.pending("alice").unwrap().changes.is_empty());
    // a review group (destructive): no partial apply at all, the full apply commits it
    e.cfg
        .stage("alice", HOSTNAME_KEY, Some(hostname("bravo")))
        .unwrap();
    e.cfg.stage_group("alice", &[HOSTNAME_KEY], true).unwrap();
    assert_eq!(
        run(&keys(&[HOSTNAME_KEY])),
        Err(ConfigError::GroupSplit(keys(&[HOSTNAME_KEY])))
    );
    // a partial apply that does not touch the group still works
    e.cfg
        .stage("alice", "test.app", Some(json!({"port": 9090, "name": "demo"})))
        .unwrap();
    run(&keys(&["test.app"])).unwrap();
    assert!(read(&e.root, "/etc/app/app.conf").contains("port = 9090"));
    apply(&e, "alice", "").unwrap();
    assert_eq!(read(&e.root, "/etc/hostname"), "bravo\n");
    // applied groups are forgotten: a later partial apply of the same keys works
    e.cfg
        .stage("alice", HOSTNAME_KEY, Some(hostname("charlie")))
        .unwrap();
    run(&keys(&[HOSTNAME_KEY])).unwrap();
    // discarded and unstaged groups are forgotten too
    both();
    e.cfg
        .stage_group("alice", &[HOSTNAME_KEY, "test.app"], true)
        .unwrap();
    e.cfg.discard("alice").unwrap();
    both();
    run(&keys(&["test.app"])).unwrap();
    e.cfg
        .stage_group("alice", &[HOSTNAME_KEY, "test.app"], false)
        .unwrap();
    e.cfg
        .stage("alice", HOSTNAME_KEY, Some(hostname("charlie")))
        .unwrap();
    e.cfg
        .stage("alice", "test.app", Some(json!({"port": 1, "name": "x"})))
        .unwrap();
    assert!(run(&keys(&["test.app"])).is_ok());
}

#[test]
fn generic_file_domain_with_header_validator_and_unit() {
    let e = env();
    e.cfg
        .stage(
            "alice",
            "test.app",
            Some(json!({"port": 8080, "name": "demo"})),
        )
        .unwrap();
    apply(&e, "alice", "").unwrap();
    let text = read(&e.root, "/etc/app/app.conf");
    assert!(
        text.starts_with("# Managed by rynas — do not edit (sha256:"),
        "{text}"
    );
    assert!(text.ends_with("port = 8080\nname = demo\n"));
    let calls = e.sys.state().calls.clone();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].starts_with("run app-check -s "));
    assert!(
        calls[0].ends_with("/etc/app/app.conf.rynas-new"),
        "{}",
        calls[0]
    );
    assert_eq!(calls[1], "reload app.service");
    // a missing template variable fails the render, nothing is written
    e.cfg
        .stage("alice", "test.app", Some(json!({"port": 1})))
        .unwrap();
    let err = apply(&e, "alice", "").unwrap_err();
    assert!(
        matches!(&err, ConfigError::Apply(m) if m.contains("render")),
        "{err:?}"
    );
    assert!(read(&e.root, "/etc/app/app.conf").contains("port = 8080"));
}

#[test]
fn validation_failure_leaves_the_system_untouched() {
    let e = env();
    e.sys.state().fail_programs.push("app-check".into());
    e.cfg
        .stage("alice", "test.app", Some(json!({"port": 1, "name": "x"})))
        .unwrap();
    let err = apply(&e, "alice", "").unwrap_err();
    assert!(
        matches!(&err, ConfigError::Apply(m) if m.contains("validation failed")),
        "{err:?}"
    );
    assert!(!e.root.join("etc/app/app.conf").exists());
    assert!(!e.root.join("etc/app/app.conf.rynas-new").exists());
    assert!(!e.sys.state().calls.iter().any(|c| c.starts_with("reload")));
    assert!(e.cfg.revisions(10).unwrap().is_empty());
    // the change stays pending
    assert_eq!(e.cfg.pending("alice").unwrap().changes.len(), 1);
}

#[test]
fn reload_failure_rolls_back_files_and_previous_reloads() {
    let e = env();
    e.cfg
        .stage("alice", "test.app", Some(json!({"port": 1, "name": "one"})))
        .unwrap();
    apply(&e, "alice", "").unwrap();
    let before_app = read(&e.root, "/etc/app/app.conf");
    let before_hosts = read(&e.root, "/etc/hosts");

    // hostname is applied first (keys in order), then the app reload fails
    e.sys.state().fail_reload.push("app.service".into());
    e.sys.state().calls.clear();
    e.cfg
        .stage("alice", HOSTNAME_KEY, Some(hostname("nas")))
        .unwrap();
    e.cfg
        .stage("alice", "test.app", Some(json!({"port": 2, "name": "two"})))
        .unwrap();
    let err = apply(&e, "alice", "").unwrap_err();
    assert!(
        matches!(&err, ConfigError::Apply(m) if m.contains("reload")),
        "{err:?}"
    );
    assert_eq!(read(&e.root, "/etc/app/app.conf"), before_app);
    assert_eq!(read(&e.root, "/etc/hosts"), before_hosts);
    assert_eq!(read(&e.root, "/etc/hostname"), "debian\n");
    assert_eq!(e.sys.state().hostname, "debian", "hostname reloaded back");
    let calls: Vec<String> = e
        .sys
        .state()
        .calls
        .iter()
        .filter(|c| !c.starts_with("run "))
        .cloned()
        .collect();
    assert_eq!(
        calls,
        [
            "set-hostname nas",
            "reload app.service",
            "reload app.service",
            "set-hostname debian"
        ]
    );
    assert_eq!(e.cfg.revisions(10).unwrap().len(), 1);
    assert!(!e.state.join("backup/2").exists());
    assert!(e.cfg.drift().unwrap().entries.is_empty());
}

#[test]
fn health_failure_rolls_back() {
    let e = env();
    e.sys.state().ignore_set_hostname = true;
    e.cfg
        .stage("alice", HOSTNAME_KEY, Some(hostname("nas")))
        .unwrap();
    let err = apply(&e, "alice", "").unwrap_err();
    assert!(
        matches!(&err, ConfigError::Apply(m) if m.contains("health check")),
        "{err:?}"
    );
    assert_eq!(read(&e.root, "/etc/hostname"), "debian\n");
    assert!(read(&e.root, "/etc/hosts").contains("127.0.1.1\tdebian"));
    assert_eq!(e.cfg.applied(HOSTNAME_KEY).unwrap(), None);
}

#[test]
fn cancelled_before_replacing_files() {
    let e = env();
    e.cfg
        .stage("alice", HOSTNAME_KEY, Some(hostname("nas")))
        .unwrap();
    let r = e.cfg.apply("alice", "alice", "", &|_| {}, &|| true);
    assert_eq!(r, Err(ConfigError::Cancelled));
    assert_eq!(read(&e.root, "/etc/hostname"), "debian\n");
    assert!(e.sys.state().calls.is_empty());
}

#[test]
fn stale_changes_conflict() {
    let e = env();
    e.cfg
        .stage("alice", HOSTNAME_KEY, Some(hostname("alpha")))
        .unwrap();
    e.cfg
        .stage("bob", HOSTNAME_KEY, Some(hostname("bravo")))
        .unwrap();
    apply(&e, "bob", "").unwrap();
    let p = e.cfg.pending("alice").unwrap();
    assert!(p.changes[0].conflict);
    assert_eq!(
        apply(&e, "alice", ""),
        Err(ConfigError::Conflict(vec![HOSTNAME_KEY.to_string()]))
    );
    // staging again does not refresh the base: discard first
    e.cfg.discard("alice").unwrap();
    e.cfg
        .stage("alice", HOSTNAME_KEY, Some(hostname("alpha")))
        .unwrap();
    apply(&e, "alice", "").unwrap();
    assert_eq!(e.sys.state().hostname, "alpha");
}

#[test]
fn restore_stages_a_new_changeset() {
    let e = env();
    for h in ["one", "two", "three"] {
        e.cfg
            .stage("alice", HOSTNAME_KEY, Some(hostname(h)))
            .unwrap();
        apply(&e, "alice", h).unwrap();
    }
    assert_eq!(e.cfg.restore("bob", 1).unwrap(), 1);
    let p = e.cfg.pending("bob").unwrap();
    assert_eq!(p.changes[0].value, Some(hostname("one")));
    assert_eq!(p.changes[0].diff[0].old, Some(json!("three")));
    let rev = apply(&e, "bob", "back to one").unwrap();
    assert_eq!(rev, 4);
    assert_eq!(e.sys.state().hostname, "one");
    let revs = e.cfg.revisions(10).unwrap();
    assert_eq!(revs.iter().map(|r| r.id).collect::<Vec<_>>(), [4, 3, 2, 1]);
    assert_eq!(revs[0].restored_from, Some(1));
    // restoring the current state stages nothing
    assert_eq!(e.cfg.restore("bob", 4).unwrap(), 0);
    assert!(matches!(
        e.cfg.restore("bob", 99),
        Err(ConfigError::NotFound(_))
    ));
}

#[test]
fn retention_by_count_keeps_the_latest() {
    let e = env_with(|o| o.keep_revisions = 2);
    for h in ["a1", "a2", "a3", "a4"] {
        e.cfg
            .stage("alice", HOSTNAME_KEY, Some(hostname(h)))
            .unwrap();
        apply(&e, "alice", "").unwrap();
    }
    let ids: Vec<u64> = e.cfg.revisions(10).unwrap().iter().map(|r| r.id).collect();
    assert_eq!(ids, [4, 3]);
    assert!(!e.state.join("backup/1").exists());
    assert!(e.state.join("backup/4").exists());
    // ids are never reused
    let e2 = env_with(|o| o.keep_age = std::time::Duration::ZERO);
    for h in ["b1", "b2"] {
        e2.cfg
            .stage("alice", HOSTNAME_KEY, Some(hostname(h)))
            .unwrap();
        apply(&e2, "alice", "").unwrap();
    }
    let ids: Vec<u64> = e2.cfg.revisions(10).unwrap().iter().map(|r| r.id).collect();
    assert_eq!(ids, [2], "age retention keeps only the latest");
}

#[test]
fn drift_detection_import_and_overwrite() {
    let e = env();
    e.cfg
        .stage("alice", "test.app", Some(json!({"port": 1, "name": "x"})))
        .unwrap();
    e.cfg
        .stage("alice", HOSTNAME_KEY, Some(hostname("nas")))
        .unwrap();
    apply(&e, "alice", "").unwrap();
    assert!(e.cfg.drift().unwrap().entries.is_empty());

    // edits outside the managed block of /etc/hosts are fine
    let hosts = e.root.join("etc/hosts");
    let mut t = std::fs::read_to_string(&hosts).unwrap();
    t.push_str("192.0.2.10\tprinter\n");
    std::fs::write(&hosts, &t).unwrap();
    assert!(e.cfg.drift().unwrap().entries.is_empty());

    // hand edits of managed content are reported
    std::fs::write(e.root.join("etc/hostname"), "handmade\n").unwrap();
    let app = e.root.join("etc/app/app.conf");
    let edited = std::fs::read_to_string(&app)
        .unwrap()
        .replace("port = 1", "port = 9");
    std::fs::write(&app, edited).unwrap();
    let r = e.cfg.drift().unwrap();
    assert_eq!(r.checked, 3);
    let paths: Vec<(&str, DriftStatus)> = r
        .entries
        .iter()
        .map(|d| (d.path.as_str(), d.status))
        .collect();
    assert_eq!(
        paths,
        [
            ("/etc/app/app.conf", DriftStatus::Modified),
            ("/etc/hostname", DriftStatus::Modified)
        ]
    );
    assert_eq!(r.entries[1].key, HOSTNAME_KEY);

    // import: the hand-made hostname becomes a pending change
    assert_eq!(e.cfg.drift_import("alice", HOSTNAME_KEY).unwrap(), 1);
    let p = e.cfg.pending("alice").unwrap();
    assert_eq!(p.changes[0].value, Some(hostname("handmade")));
    apply(&e, "alice", "import").unwrap();
    assert_eq!(e.sys.state().hostname, "handmade");
    // the printer line survived the rewrite of the block
    assert!(read(&e.root, "/etc/hosts").contains("192.0.2.10\tprinter\n"));

    // overwrite: the applied configuration is written again, without a revision
    std::fs::remove_file(&app).unwrap();
    assert_eq!(
        e.cfg.drift().unwrap().entries[0].status,
        DriftStatus::Missing
    );
    e.cfg.drift_overwrite("test.app", &|_| {}).unwrap();
    assert!(read(&e.root, "/etc/app/app.conf").contains("port = 1\n"));
    assert!(e.cfg.drift().unwrap().entries.is_empty());
    assert_eq!(e.cfg.revisions(10).unwrap().len(), 2);
    // a domain without import support cannot import
    assert!(e.cfg.drift_import("alice", "test.app").is_err());
}

#[test]
fn unchanged_files_are_not_rewritten() {
    let e = env();
    e.cfg
        .stage("alice", "test.app", Some(json!({"port": 1, "name": "x"})))
        .unwrap();
    apply(&e, "alice", "").unwrap();
    // same rendered file, different value field unused by the template
    e.cfg
        .stage(
            "alice",
            "test.app",
            Some(json!({"port": 1, "name": "x", "unused": true})),
        )
        .unwrap();
    apply(&e, "alice", "").unwrap();
    assert!(!e.state.join("backup/2/files/etc/app/app.conf").exists());
    assert!(e.cfg.drift().unwrap().entries.is_empty());
}

#[test]
fn removing_a_key_stops_managing_its_files() {
    let e = env();
    e.cfg
        .stage("alice", "test.app", Some(json!({"port": 1, "name": "x"})))
        .unwrap();
    apply(&e, "alice", "").unwrap();
    e.cfg.stage("alice", "test.app", None).unwrap();
    apply(&e, "alice", "unmanage").unwrap();
    assert_eq!(e.cfg.applied("test.app").unwrap(), None);
    assert_eq!(e.cfg.drift().unwrap().checked, 0);
    assert!(
        e.root.join("etc/app/app.conf").exists(),
        "files are left in place"
    );
    assert_eq!(e.cfg.revision(2).unwrap().diff[0].op, DiffOp::Removed);
}

#[test]
fn export_and_import_through_pending() {
    let e = env();
    e.cfg
        .stage("alice", HOSTNAME_KEY, Some(hostname("nas")))
        .unwrap();
    e.cfg
        .stage("alice", "test.app", Some(json!({"port": 1, "name": "x"})))
        .unwrap();
    apply(&e, "alice", "").unwrap();
    let out = e.state.join("nas.rynasbak");
    let mut secrets = BTreeMap::new();
    secrets.insert(
        "auth/totp/alice.json".to_string(),
        b"{\"secret\":1}".to_vec(),
    );
    let m = e
        .cfg
        .export(&out, "nas", Some(("correct horse", &secrets)))
        .unwrap();
    assert!(m.secrets);
    assert_eq!(m.keys, [HOSTNAME_KEY, "test.app"]);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&out).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let a = read_archive(&out, Some("correct horse"), &e.state).unwrap();
    assert_eq!(a.manifest.hostname, "nas");
    assert_eq!(a.values[HOSTNAME_KEY], hostname("nas"));
    assert_eq!(
        a.secrets.unwrap()["auth/totp/alice.json"],
        b"{\"secret\":1}"
    );
    assert!(read_archive(&out, Some("wrong"), &e.state).is_err());
    let a = read_archive(&out, None, &e.state).unwrap();
    assert!(a.secrets.is_none());

    // restore on a fresh machine: everything arrives as pending changes
    let fresh = env();
    assert_eq!(fresh.cfg.stage_values("root", &a.values).unwrap(), 2);
    assert_eq!(fresh.cfg.pending("root").unwrap().changes.len(), 2);
    apply(&fresh, "root", "import").unwrap();
    assert_eq!(read(&fresh.root, "/etc/hostname"), "nas\n");

    // without a passphrase there are no secrets
    let out2 = e.state.join("plain.rynasbak");
    assert!(!e.cfg.export(&out2, "nas", None).unwrap().secrets);
}

#[test]
fn extra_databases_travel_and_restore_through_the_backup_api() {
    use crate::archive::{restore_database, valid_database_name};
    assert!(valid_database_name("notify.db"));
    for bad in ["config.db", "../x.db", ".db", "Notify.db", "notify.sqlite"] {
        assert!(!valid_database_name(bad), "{bad}");
    }
    let e = env();
    let src = e.state.join("extra.db");
    let c = rusqlite::Connection::open(&src).unwrap();
    c.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t(v); INSERT INTO t VALUES (42);")
        .unwrap();
    let out = e.state.join("x.rynasbak");
    let missing = e.state.join("missing.db");
    let m = e
        .cfg
        .export_with(
            &out,
            "nas",
            &[("extra.db", src.as_path()), ("gone.db", missing.as_path())],
            None,
        )
        .unwrap();
    assert_eq!(m.databases, ["extra.db"], "missing databases are skipped");
    assert!(
        e.cfg
            .export_with(&out, "nas", &[("config.db", src.as_path())], None)
            .is_err()
    );
    let e2 = env();
    let out = e.state.join("y.rynasbak");
    e.cfg
        .export_with(&out, "nas", &[("extra.db", src.as_path())], None)
        .unwrap();
    let a = read_archive(&out, None, &e2.state).unwrap();
    let dst = e2.state.join("extra.db");
    std::fs::write(&dst, b"").unwrap();
    let kept = restore_database(&a.databases["extra.db"], &dst, &e2.state).unwrap();
    assert!(kept.is_some_and(|k| k.exists()));
    let v: i64 = rusqlite::Connection::open(&dst)
        .unwrap()
        .query_row("SELECT v FROM t", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 42);
    assert!(restore_database(b"not a database", &dst, &e2.state).is_err());
}

#[test]
fn pending_generation_follows_stage_discard_and_apply() {
    let e = env();
    let seen = Arc::new(Mutex::new(Vec::<u64>::new()));
    let s = seen.clone();
    e.cfg
        .on_pending_change(Box::new(move |g| s.lock().unwrap().push(g)));
    assert_eq!(e.cfg.pending_generation(), 0);
    e.cfg
        .stage("alice", HOSTNAME_KEY, Some(hostname("nas")))
        .unwrap();
    assert_eq!(e.cfg.pending_generation(), 1);
    // discarding an empty changeset changes nothing
    assert_eq!(e.cfg.discard("bob").unwrap(), 0);
    assert_eq!(e.cfg.pending_generation(), 1);
    apply(&e, "alice", "").unwrap();
    assert_eq!(e.cfg.pending_generation(), 2);
    e.cfg
        .stage("alice", HOSTNAME_KEY, Some(hostname("nas2")))
        .unwrap();
    assert_eq!(e.cfg.discard("alice").unwrap(), 1);
    assert_eq!(*seen.lock().unwrap(), [1, 2, 3, 4]);
}

#[test]
fn rollback_puts_files_back_with_their_owner_and_mode() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let e = env();
    let path = e.root.join("etc/app/app.conf");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "port = 0\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    // a supplementary group of the test user (a non-root user may give its file to one):
    // root:nut in production
    let egid = std::fs::metadata(&path).unwrap().gid();
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let group = status
        .lines()
        .find_map(|l| l.strip_prefix("Groups:"))
        .and_then(|g| {
            g.split_whitespace()
                .filter_map(|g| g.parse::<u32>().ok())
                .find(|g| *g != egid)
        });
    if let Some(g) = group {
        std::os::unix::fs::chown(&path, None, Some(g)).unwrap();
    }
    let want_gid = std::fs::metadata(&path).unwrap().gid();
    e.sys.state().fail_reload.push("app.service".into());
    e.cfg
        .stage("alice", "test.app", Some(json!({"port": 2, "name": "two"})))
        .unwrap();
    assert!(apply(&e, "alice", "").is_err());
    let m = std::fs::metadata(&path).unwrap();
    assert_eq!(read(&e.root, "/etc/app/app.conf"), "port = 0\n");
    assert_eq!(m.mode() & 0o7777, 0o640);
    assert_eq!(m.gid(), want_gid, "group kept (test group {group:?})");
    assert!(!e.root.join("etc/app/app.conf.rynas-new").exists());
}

/// A domain whose removal puts an "off" block back and removes a file.
struct LeavesBehind;

impl Applier for LeavesBehind {
    fn key(&self) -> &str {
        "test.leaves"
    }

    fn validate(
        &self,
        value: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, Invalid> {
        Ok(value)
    }

    fn render(
        &self,
        value: &serde_json::Value,
        _ctx: &ApplyCtx,
    ) -> std::result::Result<Vec<ManagedFile>, String> {
        Ok(vec![
            ManagedFile::block("/etc/hosts", "leaves", format!("# on {value}")),
            ManagedFile::whole("/etc/leaves/open.conf", "open\n".into(), true),
        ])
    }

    fn render_removed(
        &self,
        old: Option<&serde_json::Value>,
        ctx: &ApplyCtx,
    ) -> std::result::Result<Vec<ManagedFile>, String> {
        assert!(old.is_some(), "the removed value is passed");
        assert!(ctx.read(Path::new("/etc/leaves/open.conf")).is_some());
        Ok(vec![
            ManagedFile::block("/etc/hosts", "leaves", "# off".into()),
            ManagedFile::absent("/etc/leaves/open.conf"),
        ])
    }

    fn reconcile(
        &self,
        _from: Option<&serde_json::Value>,
        to: Option<&serde_json::Value>,
        ctx: &ApplyCtx,
    ) -> std::result::Result<(), String> {
        if to.is_none() {
            assert!(ctx.replaced(Path::new("/etc/hosts")));
            assert!(ctx.replaced(Path::new("/etc/leaves/open.conf")));
            assert!(!ctx.replaced(Path::new("/etc/hostname")));
        }
        Ok(())
    }
}

#[test]
fn removing_a_key_writes_what_it_leaves_behind() {
    let e = env();
    let mut domains = appliers();
    domains.push(Arc::new(LeavesBehind));
    let cfg = Config::open(
        ConfigOptions::new(e.state.join("2"), &e.root),
        e.sys.clone(),
        domains,
    )
    .unwrap();
    cfg.stage("alice", "test.leaves", Some(json!(1))).unwrap();
    cfg.apply("alice", "alice", "", &|_| {}, &|| false).unwrap();
    assert!(read(&e.root, "/etc/hosts").contains("# on 1\n"));
    assert_eq!(cfg.drift().unwrap().checked, 2);
    // e.g. restoring a revision from before the key existed
    cfg.stage("alice", "test.leaves", None).unwrap();
    cfg.apply("alice", "alice", "", &|_| {}, &|| false).unwrap();
    let hosts = read(&e.root, "/etc/hosts");
    assert!(
        hosts.contains("# off\n") && !hosts.contains("# on"),
        "{hosts}"
    );
    assert!(hosts.starts_with("127.0.0.1\tlocalhost\n"));
    assert!(!e.root.join("etc/leaves/open.conf").exists());
    assert_eq!(cfg.applied("test.leaves").unwrap(), None);
    assert_eq!(cfg.drift().unwrap().checked, 0, "no longer tracked");
}
