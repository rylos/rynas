//! The `snapshots` domain in a real configuration store on a scratch root, with the
//! in-memory system and filesystem. Golden files: `src/golden/`, regenerated with
//! `RYNAS_UPDATE_GOLDEN=1 cargo test -p rynas-btrfs`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rynas_api_types::snapshots::{RetentionPreset, SnapshotJob, SnapshotsConfig, SystemSnapshots};
use rynas_config::{Config, ConfigError, ConfigOptions, MemorySystem};
use tempfile::TempDir;

use crate::domain::{APT_HOOK, MemorySnapshots, SNAPSHOTS_KEY, SnapshotsDomain};

pub fn golden(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/golden")
        .join(name);
    if std::env::var_os("RYNAS_UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing golden file {name}: run with RYNAS_UPDATE_GOLDEN=1"));
    assert_eq!(actual, want, "golden file {name} differs");
}

struct Env {
    _dir: TempDir,
    root: PathBuf,
    sys: Arc<MemorySystem>,
    host: Arc<MemorySnapshots>,
    cfg: Config,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let sys = Arc::new(MemorySystem::new("nas"));
    let host = Arc::new(MemorySnapshots::new());
    host.add_subvolume("/srv/vol1/docs");
    host.add_subvolume("/");
    let cfg = Config::open(
        ConfigOptions::new(dir.path().join("state"), &root),
        sys.clone(),
        vec![Arc::new(SnapshotsDomain::new(host.clone()))],
    )
    .unwrap();
    Env {
        _dir: dir,
        root,
        sys,
        host,
        cfg,
    }
}

fn apply(e: &Env) -> Result<u64, ConfigError> {
    let lines = Mutex::new(Vec::<String>::new());
    let r = e.cfg.apply(
        "admin",
        "admin",
        "test",
        &|l| lines.lock().unwrap().push(l.into()),
        &|| false,
    );
    if r.is_err() {
        eprintln!("apply log:\n{}", lines.lock().unwrap().join("\n"));
    }
    r
}

fn job(name: &str, sv: &str) -> SnapshotJob {
    SnapshotJob {
        name: name.into(),
        share: Some(name.into()),
        subvolume: sv.into(),
        snapshot_dir: String::new(),
        retention: RetentionPreset::Standard.retention(),
        replicas: Vec::new(),
    }
}

fn stage(e: &Env, c: &SnapshotsConfig) -> Result<bool, ConfigError> {
    e.cfg.stage(
        "admin",
        SNAPSHOTS_KEY,
        Some(serde_json::to_value(c).unwrap()),
    )
}

#[test]
fn validation_fills_defaults_and_refuses_bad_jobs() {
    let e = env();
    let mut c = SnapshotsConfig {
        jobs: vec![job("docs", "/srv/vol1/docs")],
        system: SystemSnapshots::default(),
    };
    c.jobs[0].retention.preset = Some(RetentionPreset::Long);
    c.jobs[0].retention.daily = 1;
    stage(&e, &c).unwrap();
    let staged: SnapshotsConfig = serde_json::from_value(
        e.cfg
            .pending_value("admin", SNAPSHOTS_KEY)
            .unwrap()
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(staged.jobs[0].snapshot_dir, "/srv/vol1/.snapshots/docs");
    assert_eq!(staged.jobs[0].retention.daily, 31, "the preset wins");

    for (bad, why) in [
        (job("bad name", "/srv/vol1/docs"), "name"),
        (job("x", "srv/vol1/docs"), "relative"),
        (job("x", "/"), "root"),
        (job("x", "/srv/../etc"), "dots"),
    ] {
        let c = SnapshotsConfig {
            jobs: vec![bad],
            ..Default::default()
        };
        assert!(
            matches!(stage(&e, &c), Err(ConfigError::Invalid(_))),
            "{why}"
        );
    }
    let mut inside = job("x", "/srv/vol1/docs");
    inside.snapshot_dir = "/srv/vol1/docs/.snap".into();
    let mut min = job("y", "/srv/vol1/docs");
    min.retention.preset = None;
    min.retention.min = "no".into();
    let twice = SnapshotsConfig {
        jobs: vec![
            job("docs", "/srv/vol1/docs"),
            job("DOCS", "/srv/vol1/other"),
        ],
        ..Default::default()
    };
    let same = SnapshotsConfig {
        jobs: vec![job("a", "/srv/vol1/docs"), job("b", "/srv/vol1/docs")],
        ..Default::default()
    };
    for c in [
        SnapshotsConfig {
            jobs: vec![inside],
            ..Default::default()
        },
        SnapshotsConfig {
            jobs: vec![min],
            ..Default::default()
        },
        twice,
        same,
    ] {
        assert!(matches!(stage(&e, &c), Err(ConfigError::Invalid(_))));
    }
    let mut sys = SnapshotsConfig::default();
    sys.system.enabled = true;
    sys.system.keep = 0;
    assert!(matches!(stage(&e, &sys), Err(ConfigError::Invalid(_))));
}

#[test]
fn apply_writes_configurations_hook_and_directories() {
    let e = env();
    let mut c = SnapshotsConfig {
        jobs: vec![job("docs", "/srv/vol1/docs")],
        system: SystemSnapshots {
            enabled: true,
            ..Default::default()
        },
    };
    stage(&e, &c).unwrap();
    apply(&e).unwrap();
    let conf = std::fs::read_to_string(e.root.join("etc/rynas/btrbk/docs.conf")).unwrap();
    assert!(conf.starts_with("# Managed by rynas — do not edit (sha256:"));
    assert!(conf.contains("\nsnapshot_preserve       24h 14d 8w 12m\n"));
    let hook = std::fs::read_to_string(e.root.join(APT_HOOK.trim_start_matches('/'))).unwrap();
    golden("80rynas-snapshot", &hook);
    let calls = e.sys.state().calls.clone();
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("run btrbk -c ")
                && c.ends_with("docs.conf.rynas-new config print")),
        "{calls:?}"
    );
    let dirs = e.host.state.lock().unwrap().dirs.clone();
    assert_eq!(dirs, ["/srv/vol1/.snapshots/docs", "/.snapshots/rootfs"]);
    assert!(e.cfg.drift().unwrap().entries.is_empty());

    // removing the job removes its configuration; disabling restore points the hook
    c.jobs.clear();
    c.system.enabled = false;
    stage(&e, &c).unwrap();
    apply(&e).unwrap();
    assert!(!e.root.join("etc/rynas/btrbk/docs.conf").exists());
    assert!(!e.root.join(APT_HOOK.trim_start_matches('/')).exists());
}

#[test]
fn a_job_on_a_plain_directory_is_rolled_back() {
    let e = env();
    let c = SnapshotsConfig {
        jobs: vec![job("media", "/srv/vol1/media")],
        ..Default::default()
    };
    stage(&e, &c).unwrap();
    assert!(matches!(apply(&e), Err(ConfigError::Apply(m)) if m.contains("not a btrfs subvolume")));
    assert!(!e.root.join("etc/rynas/btrbk/media.conf").exists());
}

#[test]
fn a_foreign_file_in_the_configuration_directory_is_left_alone() {
    let e = env();
    let d = e.root.join("etc/rynas/btrbk");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("mine.conf"), "volume /x\n").unwrap();
    let c = SnapshotsConfig {
        jobs: vec![job("docs", "/srv/vol1/docs")],
        ..Default::default()
    };
    stage(&e, &c).unwrap();
    apply(&e).unwrap();
    assert!(d.join("mine.conf").exists());
}
