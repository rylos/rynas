//! The `scheduler` domain in a real configuration store on a scratch root, with the
//! in-memory system. Golden unit files: `src/golden/`, regenerated with
//! `RYNAS_UPDATE_GOLDEN=1 cargo test -p rynas-scheduler`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rynas_api_types::scheduler::{
    NotifyPolicy, ScheduledTask, SchedulerConfig, SmartTestKind, TaskAction, TaskTrigger,
};
use rynas_config::{Config, ConfigError, ConfigOptions, MemorySystem};
use tempfile::TempDir;

use crate::domain::{
    SCHEDULER_KEY, SchedulerDomain, normalize_task, root_only, root_task_changes, valid_task_id,
};
use crate::units::Paths;

fn golden(name: &str, actual: &str) {
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
    cfg: Config,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let sys = Arc::new(MemorySystem::new("nas"));
    let uid = |u: &str| match u {
        "alice" => Some(1001),
        "daemon" => Some(1),
        _ => None,
    };
    let cfg = Config::open(
        ConfigOptions::new(dir.path().join("state"), &root),
        sys.clone(),
        vec![Arc::new(SchedulerDomain::new(
            Paths::default(),
            Arc::new(uid),
        ))],
    )
    .unwrap();
    Env {
        _dir: dir,
        root,
        sys,
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

fn read(root: &Path, p: &str) -> String {
    std::fs::read_to_string(root.join(p.trim_start_matches('/'))).unwrap()
}

fn calendar(c: &str) -> TaskTrigger {
    TaskTrigger::Calendar {
        on_calendar: c.into(),
        cron: None,
        persistent: true,
        randomized_delay_s: 0,
    }
}

fn task(id: &str, action: TaskAction, trigger: TaskTrigger) -> ScheduledTask {
    ScheduledTask {
        id: id.into(),
        name: String::new(),
        description: String::new(),
        enabled: true,
        action,
        trigger,
        timeout_s: None,
        nice: None,
        io_weight: None,
        notify: NotifyPolicy::OnFailure,
        notify_output: false,
    }
}

fn sample() -> Vec<ScheduledTask> {
    let mut rsync = task(
        "rsync-photos",
        TaskAction::Command {
            command: "rsync -a /srv/vol1/photos/ /mnt/usb/photos/".into(),
            user: "alice".into(),
        },
        calendar("Mon..Fri 02:30"),
    );
    rsync.name = "rsync photos (100% of them)".into();
    rsync.timeout_s = Some(7200);
    rsync.io_weight = Some(50);
    let report = task(
        "report",
        TaskAction::Script {
            script: "#!/usr/bin/env python3\nprint('hello')".into(),
            user: "root".into(),
        },
        TaskTrigger::Boot { delay_s: 120 },
    );
    let snap = task(
        "snapshot-docs",
        TaskAction::Snapshot { job: "docs".into() },
        calendar("hourly"),
    );
    let smart = task(
        "smart-sda",
        TaskAction::SmartTest {
            disk: "sda".into(),
            test: SmartTestKind::Long,
        },
        TaskTrigger::Manual,
    );
    vec![rsync, report, snap, smart]
}

fn stage(e: &Env, tasks: Vec<ScheduledTask>) -> Result<bool, ConfigError> {
    e.cfg.stage(
        "admin",
        SCHEDULER_KEY,
        Some(serde_json::to_value(SchedulerConfig { tasks }).unwrap()),
    )
}

#[test]
fn apply_writes_units_scripts_and_starts_timers() {
    let e = env();
    stage(&e, sample()).unwrap();
    apply(&e).unwrap();
    let units = "/etc/systemd/system";
    golden(
        "rsync-photos.service",
        &read(&e.root, &format!("{units}/rynas-task-rsync-photos.service")),
    );
    golden(
        "rsync-photos.timer",
        &read(&e.root, &format!("{units}/rynas-task-rsync-photos.timer")),
    );
    golden(
        "report.service",
        &read(&e.root, &format!("{units}/rynas-task-report.service")),
    );
    golden(
        "report.timer",
        &read(&e.root, &format!("{units}/rynas-task-report.timer")),
    );
    golden(
        "snapshot-docs.service",
        &read(
            &e.root,
            &format!("{units}/rynas-task-snapshot-docs.service"),
        ),
    );
    golden(
        "rynas-tasks.conf",
        &read(
            &e.root,
            "/etc/systemd/system/timers.target.d/rynas-tasks.conf",
        ),
    );
    assert!(
        !e.root
            .join("etc/systemd/system/rynas-task-smart-sda.timer")
            .exists(),
        "manual: no timer"
    );
    let script = e.root.join("var/lib/rynas/tasks/report/run");
    assert_eq!(
        std::fs::read_to_string(&script).unwrap(),
        "#!/usr/bin/env python3\nprint('hello')\n"
    );
    assert_eq!(
        std::fs::metadata(&script).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(read(&e.root, "/var/lib/rynas/tasks/rsync-photos/run").starts_with("#!/bin/sh\n"));
    let calls = e.sys.state().calls.clone();
    assert!(
        calls.contains(&"run systemctl daemon-reload".to_string()),
        "{calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c == "run systemctl restart rynas-task-rsync-photos.timer"),
        "{calls:?}"
    );
    assert!(calls.iter().any(|c| c.starts_with("run systemd-analyze calendar --iterations=1 -- Mon..Fri 02:30")));
    assert!(e.cfg.drift().unwrap().entries.is_empty());

    // disable one, remove another: timer stopped, units of the removed one gone
    e.sys.state().calls.clear();
    let mut tasks = sample();
    tasks[0].enabled = false;
    tasks.retain(|t| t.id != "report");
    stage(&e, tasks).unwrap();
    apply(&e).unwrap();
    let calls = e.sys.state().calls.clone();
    assert!(
        calls.contains(&"run systemctl stop rynas-task-rsync-photos.timer".to_string()),
        "{calls:?}"
    );
    assert!(
        calls.contains(&"run systemctl stop rynas-task-report.timer".to_string()),
        "{calls:?}"
    );
    assert!(
        calls
            .contains(&"run systemctl start --no-block rynas-task-snapshot-docs.timer".to_string()),
        "{calls:?}"
    );
    assert!(
        !e.root
            .join("etc/systemd/system/rynas-task-report.service")
            .exists()
    );
    assert!(
        !e.root
            .join("etc/systemd/system/rynas-task-report.timer")
            .exists()
    );
    assert!(!e.root.join("var/lib/rynas/tasks/report/run").exists());
    assert!(
        !read(
            &e.root,
            "/etc/systemd/system/timers.target.d/rynas-tasks.conf"
        )
        .contains("rsync-photos")
    );
}

#[test]
fn failed_checks_roll_back() {
    let e = env();
    // unknown user
    let mut t = sample();
    t[0].action = TaskAction::Command {
        command: "true".into(),
        user: "mallory".into(),
    };
    stage(&e, t).unwrap();
    assert!(
        matches!(apply(&e), Err(ConfigError::Apply(m)) if m.contains("mallory does not exist"))
    );
    e.cfg.discard("admin").unwrap();
    // system account
    let mut t = sample();
    t[0].action = TaskAction::Command {
        command: "true".into(),
        user: "daemon".into(),
    };
    stage(&e, t).unwrap();
    assert!(matches!(apply(&e), Err(ConfigError::Apply(m)) if m.contains("system account")));
    e.cfg.discard("admin").unwrap();
    // systemd refuses the calendar
    e.sys.state().fail_programs.push("systemd-analyze".into());
    stage(&e, sample()).unwrap();
    assert!(matches!(apply(&e), Err(ConfigError::Apply(m)) if m.contains("invalid calendar")));
    assert!(
        !e.root
            .join("etc/systemd/system/rynas-task-report.service")
            .exists()
    );
    e.sys.state().fail_programs.clear();
    // an inactive timer after the reload fails the health check and rolls back
    e.sys
        .state()
        .inactive
        .push("rynas-task-snapshot-docs.timer".into());
    assert!(matches!(apply(&e), Err(ConfigError::Apply(m)) if m.contains("not active")));
    assert!(
        !e.root
            .join("etc/systemd/system/rynas-task-snapshot-docs.service")
            .exists()
    );
}

#[test]
fn validation() {
    let base = || {
        task(
            "x",
            TaskAction::Command {
                command: "true".into(),
                user: "root".into(),
            },
            calendar("daily"),
        )
    };
    let ok = normalize_task(base()).unwrap();
    assert_eq!(ok.name, "x");
    let mut cron = base();
    cron.trigger = TaskTrigger::Calendar {
        on_calendar: String::new(),
        cron: Some("*/10 * * * *".into()),
        persistent: true,
        randomized_delay_s: 0,
    };
    assert_eq!(
        normalize_task(cron).unwrap().trigger,
        calendar("*-*-* *:00/10:00")
    );
    let mut reboot = base();
    reboot.trigger = TaskTrigger::Calendar {
        on_calendar: String::new(),
        cron: Some("@reboot".into()),
        persistent: true,
        randomized_delay_s: 0,
    };
    assert_eq!(
        normalize_task(reboot).unwrap().trigger,
        TaskTrigger::Boot { delay_s: 60 }
    );

    type Mutate = Box<dyn Fn(&mut ScheduledTask)>;
    let bad: Vec<Mutate> = vec![
        Box::new(|t| t.id = "Bad_Id".into()),
        Box::new(|t| t.id = "preview-calendar".into()),
        Box::new(|t| {
            t.action = TaskAction::Command {
                command: " ".into(),
                user: "root".into(),
            }
        }),
        Box::new(|t| {
            t.action = TaskAction::Command {
                command: "x".into(),
                user: "Root".into(),
            }
        }),
        Box::new(|t| {
            t.action = TaskAction::Script {
                script: "#!bash\necho".into(),
                user: "root".into(),
            }
        }),
        Box::new(|t| {
            t.action = TaskAction::SmartTest {
                disk: "../sda".into(),
                test: SmartTestKind::Short,
            }
        }),
        Box::new(|t| {
            t.action = TaskAction::Scrub {
                filesystem: "vol1".into(),
            }
        }),
        Box::new(|t| t.trigger = calendar("daily\nExecStart=/bin/sh")),
        Box::new(|t| t.trigger = calendar("")),
        Box::new(|t| t.timeout_s = Some(30 * 86400)),
        Box::new(|t| t.nice = Some(40)),
        Box::new(|t| t.io_weight = Some(0)),
    ];
    for (i, f) in bad.iter().enumerate() {
        let mut t = base();
        f(&mut t);
        assert!(normalize_task(t).is_err(), "case {i}");
    }
    assert!(valid_task_id("backup-1") && !valid_task_id("-x") && !valid_task_id(""));

    let e = env();
    let twice = vec![base(), base()];
    assert!(matches!(stage(&e, twice), Err(ConfigError::Invalid(_))));
}

#[test]
fn root_tasks_only_run_root_owned_files() {
    // the test runs unprivileged: everything it creates belongs to a normal user
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("script.sh");
    std::fs::write(&f, "echo").unwrap();
    if std::fs::metadata(&f)
        .map(|m| std::os::unix::fs::MetadataExt::uid(&m))
        .unwrap()
        != 0
    {
        assert!(root_only(&f).is_err());
    }
    assert!(root_only(Path::new("/bin/sh")).is_ok());
}

#[test]
fn root_task_changes_between_values() {
    let doc = |tasks: Vec<ScheduledTask>| serde_json::to_value(SchedulerConfig { tasks }).unwrap();
    let base = doc(sample());
    // same tasks, nothing to check; internal and non-root tasks never count
    assert!(root_task_changes(Some(&base), Some(&base)).is_empty());
    let mut t = sample();
    t[0].timeout_s = Some(60); // alice's task
    t[2].enabled = false; // internal snapshot task
    t.remove(3); // internal SMART task removed
    assert!(root_task_changes(Some(&base), Some(&doc(t))).is_empty());
    // a root script changed, a new root task, alice's task turned into a root one
    let mut t = sample();
    t[1].timeout_s = Some(60);
    t[0].action = TaskAction::Command {
        command: "true".into(),
        user: "root".into(),
    };
    t.push(task(
        "zap",
        TaskAction::Command {
            command: "rm -rf /tmp/x".into(),
            user: "root".into(),
        },
        TaskTrigger::Manual,
    ));
    assert_eq!(
        root_task_changes(Some(&base), Some(&doc(t))),
        vec!["report", "rsync-photos", "zap"]
    );
    // a root task removed, or turned into a plain user's: changes of a root task too
    let mut t = sample();
    t.remove(1);
    assert_eq!(
        root_task_changes(Some(&base), Some(&doc(t))),
        vec!["report"]
    );
    let mut t = sample();
    t[1].action = TaskAction::Script {
        script: "#!/bin/sh\n".into(),
        user: "alice".into(),
    };
    assert_eq!(
        root_task_changes(Some(&base), Some(&doc(t))),
        vec!["report"]
    );
    // from nothing (key never applied) and to nothing (key removed)
    assert_eq!(root_task_changes(None, Some(&base)), vec!["report"]);
    assert_eq!(root_task_changes(Some(&base), None), vec!["report"]);
    assert!(root_task_changes(None, None).is_empty());
}
