//! `.rynasbak` export/import: configuration (scheduler tasks included), the notification
//! store with its key inside the encrypted secrets, the scheduler history left out.

use std::path::Path;
use std::sync::Arc;

use rynas_api_types::notify::{NotifyLanguage, TargetConfig, TargetRequest, WebhookMethod};
use rynas_config::{Config, ConfigOptions, MemorySystem};
use rynas_notify::engine::secrets::SecretBox;
use rynas_notify::engine::{Engine, EngineOptions};
use rynas_scheduler::SchedulerDomain;
use rynas_scheduler::units::Paths;
use rynasd::backup::{self, ImportOptions, NotifyRestore};
use rynasd::roles::AdminPolicy;
use serde_json::json;

fn config(state: &Path) -> Config {
    let root = state.join("root");
    std::fs::create_dir_all(&root).unwrap();
    let uid = |u: &str| (u == "alice").then_some(1001);
    Config::open(
        ConfigOptions::new(state, &root),
        Arc::new(MemorySystem::new("nas")),
        vec![Arc::new(SchedulerDomain::new(
            Paths::default(),
            Arc::new(uid),
        ))],
    )
    .unwrap()
}

fn engine(state: &Path) -> Engine {
    Engine::open(EngineOptions::new(state.join("notify"), "nas")).unwrap()
}

/// The secrets of target `name`, opened with the key of `state`.
fn target_secrets(
    state: &Path,
    name: &str,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let c = rusqlite::Connection::open(state.join("notify/notify.db")).unwrap();
    let sealed: Vec<u8> = c
        .query_row("SELECT secrets FROM targets WHERE name = ?1", [name], |r| {
            r.get(0)
        })
        .unwrap();
    SecretBox::open(&state.join("notify"))
        .unwrap()
        .open_sealed(&sealed)
}

fn import(
    state: &Path,
    file: &Path,
    pass: Option<&str>,
    restore_notifications: bool,
) -> backup::ImportReport {
    let opts = ImportOptions {
        owner: "admin".into(),
        restore_secrets: true,
        restore_notifications,
        ..Default::default()
    };
    backup::import(&config(state), state, file, pass, &opts).unwrap()
}

#[test]
fn export_and_import_the_notification_store_and_tasks() {
    let src = tempfile::tempdir().unwrap();
    let s = src.path();
    let cfg = config(s);
    let task = json!({"tasks": [{"id": "hello", "action": {"kind": "command", "command": "echo hi", "user": "alice"},
        "trigger": {"kind": "manual"}}]});
    cfg.stage_values("admin", &[("scheduler".to_string(), task)].into())
        .unwrap();
    cfg.apply("admin", "admin", "test", &|_| {}, &|| false)
        .unwrap();
    {
        let e = engine(s);
        e.create_target(&TargetRequest {
            name: "hook".into(),
            enabled: true,
            language: NotifyLanguage::En,
            config: TargetConfig::Webhook {
                url: "https://example.org/hook".into(),
                method: WebhookMethod::Post,
                headers: vec![],
                body: None,
            },
            secrets: [("token".to_string(), Some("tok-123456".to_string()))].into(),
        })
        .unwrap();
    }
    // the run history stays out of the archive
    std::fs::write(s.join("scheduler.db"), b"history").unwrap();
    std::fs::create_dir_all(s.join("auth/totp")).unwrap();
    std::fs::write(s.join("auth/totp/alice.json"), b"{}").unwrap();

    let out = s.join("nas.rynasbak");
    let r = backup::export(&cfg, s, &out, "nas", Some("correct horse")).unwrap();
    assert_eq!(r.manifest.databases, ["notify.db"]);
    assert_eq!(r.secrets, 2, "TOTP + notification key");
    assert!(r.manifest.keys.contains(&"scheduler".to_string()));
    let plain = s.join("plain.rynasbak");
    let r = backup::export(&cfg, s, &plain, "nas", None).unwrap();
    assert_eq!((r.secrets, r.manifest.secrets), (0, false));
    assert_eq!(r.manifest.databases, ["notify.db"]);

    // a machine with its own (empty) notification store and key
    let dst = tempfile::tempdir().unwrap();
    let d = dst.path();
    drop(engine(d));
    let r = import(d, &out, Some("correct horse"), false);
    assert_eq!(r.notifications, NotifyRestore::Skipped);
    assert!(
        engine(d)
            .targets()
            .unwrap()
            .iter()
            .all(|t| t.name != "hook")
    );
    let r = import(d, &out, Some("correct horse"), true);
    assert_eq!(r.staged, 1, "the scheduler key is staged");
    assert_eq!(
        (r.totp_in_archive, r.totp_restored),
        (1, 0),
        "restored by the first import"
    );
    let NotifyRestore::Restored { key, kept } = r.notifications else {
        panic!("{:?}", r.notifications)
    };
    assert!(key);
    assert_eq!(kept.len(), 2, "previous key and database kept: {kept:?}");
    assert!(kept.iter().all(|p| p.exists()));
    assert!(
        engine(d)
            .targets()
            .unwrap()
            .iter()
            .any(|t| t.name == "hook")
    );
    assert_eq!(
        target_secrets(d, "hook")
            .unwrap()
            .get("token")
            .map(String::as_str),
        Some("tok-123456"),
        "the restored key opens the restored secrets"
    );
    let pending = config(d).pending("admin").unwrap();
    assert!(pending.changes.iter().any(|c| c.key == "scheduler"));
    assert!(!d.join("scheduler.db").exists());

    // without a passphrase: the store comes back, its secrets cannot be opened
    let bare = tempfile::tempdir().unwrap();
    let b = bare.path();
    drop(engine(b));
    let r = import(b, &plain, None, true);
    assert!(matches!(
        r.notifications,
        NotifyRestore::Restored { key: false, .. }
    ));
    assert!(target_secrets(b, "hook").is_err());

    // a wrong passphrase changes nothing
    let opts = ImportOptions {
        owner: "admin".into(),
        restore_notifications: true,
        ..Default::default()
    };
    assert!(backup::import(&config(b), b, &out, Some("wrong"), &opts).is_err());
}

/// An archive that adds a root user task is staged only for an administrator.
#[test]
fn import_of_root_tasks_needs_an_administrator() {
    let src = tempfile::tempdir().unwrap();
    let s = src.path();
    let cfg = config(s);
    let task = json!({"tasks": [{"id": "zap", "action": {"kind": "command", "command": "true", "user": "root"},
        "trigger": {"kind": "manual"}}]});
    cfg.stage_values("admin", &[("scheduler".to_string(), task)].into())
        .unwrap();
    cfg.apply("admin", "admin", "test", &|_| {}, &|| false)
        .unwrap();
    let out = s.join("nas.rynasbak");
    backup::export(&cfg, s, &out, "nas", None).unwrap();

    let dst = tempfile::tempdir().unwrap();
    let d = dst.path();
    let policy = AdminPolicy::new(vec!["rynas-admin".into()], |u| match u {
        "admin" => Some(vec!["rynas-admin".into()]),
        "bob" => Some(vec!["users".into()]),
        _ => None,
    });
    let opts = |owner: &str| ImportOptions {
        owner: owner.into(),
        admins: policy.clone(),
        ..Default::default()
    };
    let e = backup::import(&config(d), d, &out, None, &opts("bob")).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(e.to_string().contains("zap"), "{e}");
    assert!(config(d).pending("bob").unwrap().changes.is_empty());
    let r = backup::import(&config(d), d, &out, None, &opts("admin")).unwrap();
    assert_eq!(r.staged, 1);
    // the same tasks as applied: nothing to check, nothing staged
    let r = backup::import(&cfg, s, &out, None, &opts("bob")).unwrap();
    assert_eq!(r.staged, 0);
}
