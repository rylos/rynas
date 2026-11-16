//! The tasks of the jobs rynasd ran in its own loops before S3: built from the settings an
//! older release stored, they run at the same times, in the same windows and at the same
//! intervals (units checked against golden files `src/golden/system/`), and the gate keeps
//! the loops' rules (a window missed while the NAS was off, the late PC maintenance, the
//! interval counted from the last run).

use jiff::Timestamp;
use jiff::tz::TimeZone;
use rynas_api_types::docker::{AutoUpdateSettings, StackAutoUpdate};
use rynas_api_types::scheduler::system::{self, PC_MAINTENANCE_CATCH_UP_H};
use rynas_api_types::scheduler::{
    LockedField, MissedPolicy, OwnerFeature, RunTrigger, SchedulerConfig, SchedulerPause,
    SkipReason, TaskOwner, TaskTrigger,
};
use rynas_api_types::updates::{AutoMode, AutoUpdates};

use super::{apply, env, golden, read};
use crate::domain::{SCHEDULER_KEY, normalize_task};
use crate::gate::{Decision, Start, decide, interval_due_ms};
use crate::units::render_timer;

/// `docker.auto_update` as 0.8.15 stored it.
const DOCKER_0_8_15: &str = r#"{"hour":4,"minute":30,"window_min":90,"stacks":{"web":{"enabled":true,"policy":null}}}"#;
/// `updates.auto` as 0.8.15 stored it.
const UPDATES_0_8_15: &str = r#"{"mode":"security","hour":3,"minute":0,"window_min":60,"reboot":false,"reboot_hour":4,"reboot_minute":0}"#;

fn docker() -> AutoUpdateSettings {
    serde_json::from_str(DOCKER_0_8_15).unwrap()
}

#[test]
fn tasks_from_older_settings_keep_their_schedule() {
    let d = docker();
    let t = normalize_task(system::docker_auto_update(&d)).unwrap();
    assert!(t.enabled, "a stack takes automatic updates");
    // the window of the loop: 04:30, a random start in its first third (at most 20 min)
    assert_eq!(t.trigger.on_calendar().as_deref(), Some("*-*-* 04:30:00"));
    let p = t.effective_policy();
    assert_eq!((p.jitter_s, p.window_min), (20 * 60, 90));
    assert_eq!(system::docker_window_delay_s(30), 10 * 60);
    assert_eq!(system::docker_window_delay_s(10), 3 * 60);
    let mut off = d.clone();
    off.stacks.insert("web".into(), StackAutoUpdate::default());
    assert!(!system::docker_auto_update(&off).enabled, "no stack: the task is off");
    // updates: the window's start, the whole window as the random delay
    let u: AutoUpdates = serde_json::from_str(UPDATES_0_8_15).unwrap();
    let a = system::system_upgrade(&u);
    assert_eq!(a.trigger.on_calendar().as_deref(), Some("*-*-* 03:00:00"));
    assert_eq!(a.effective_policy().jitter_s, 3600);
    assert!(a.enabled && !system::system_upgrade(&AutoUpdates { mode: AutoMode::Off, ..u }).enabled);
    assert!(normalize_task(a).unwrap_err().contains("system's own timer"), "never stored");
    // PC maintenance at its hour, caught up for six hours
    let m = normalize_task(system::pc_maintenance(3)).unwrap();
    assert_eq!(m.trigger.on_calendar().as_deref(), Some("*-*-* 03:00:00"));
    assert_eq!(m.effective_policy().missed, MissedPolicy::RunOnce { within_h: Some(PC_MAINTENANCE_CATCH_UP_H) });
    // owners and locks
    for t in [system::updates_check(), system::docker_image_check(), system::apps_catalog(true), system::pc_retention(), m.clone()] {
        let t = normalize_task(t).unwrap();
        assert!(matches!(t.owner, TaskOwner::Feature { .. }), "{}", t.id);
        assert!(t.owner.locked().contains(&LockedField::Schedule));
        assert!(t.owner.locked().contains(&LockedField::Missed));
    }
    assert!(matches!(m.owner, TaskOwner::Feature { feature: OwnerFeature::PcBackup, .. }));
    // what the user set on the Task Scheduler page stays
    let mut mine = system::updates_check();
    mine.enabled = false;
    mine.notify = rynas_api_types::scheduler::NotifyPolicy::Always;
    let kept = system::keep_options(system::updates_check(), Some(&mine), false);
    assert!(!kept.enabled && kept.notify == rynas_api_types::scheduler::NotifyPolicy::Always);
    let kept = system::keep_options(system::docker_auto_update(&d), Some(&mine), true);
    assert!(kept.enabled, "the feature decides whether it is on");
}

#[test]
fn units_of_the_system_tasks() {
    let e = env();
    let u: AutoUpdates = serde_json::from_str(UPDATES_0_8_15).unwrap();
    let _ = u;
    let tasks = vec![
        system::updates_check(),
        system::docker_image_check(),
        system::docker_auto_update(&docker()),
        system::apps_catalog(true),
        system::pc_maintenance(3),
        system::pc_retention(),
    ];
    let ids: Vec<String> = tasks.iter().map(|t| t.id.clone()).collect();
    e.cfg
        .stage("admin", SCHEDULER_KEY, Some(serde_json::to_value(SchedulerConfig { tasks }).unwrap()))
        .unwrap();
    apply(&e).unwrap();
    for id in ids {
        for ext in ["service", "timer"] {
            let text = read(&e.root, &format!("/etc/systemd/system/rynas-task-{id}.{ext}"));
            golden(&format!("system/{id}.{ext}"), &text);
        }
    }
    // the interval: after boot, then after each run's end
    let t = render_timer(&system::updates_check()).unwrap();
    assert!(t.contains("OnBootSec=600\nOnUnitInactiveSec=86400\n"), "{t}");
    let t = render_timer(&system::pc_retention()).unwrap();
    assert!(t.contains("OnBootSec=120\nOnUnitInactiveSec=3600\n"), "{t}");
    // a deduplication window: one line per start hour, its exact days
    let mut w = vec![b'0'; 168];
    for d in [0usize, 6] {
        for h in 1..4 {
            w[d * 24 + h] = b'1';
        }
    }
    let mut dd = super::task("dedup-vol1", rynas_api_types::scheduler::TaskAction::Dedup { volume: "vol1".into() }, TaskTrigger::Window { window: String::from_utf8(w).unwrap(), stop_at_end: true });
    dd.policy.missed = Some(MissedPolicy::Skip);
    let t = render_timer(&normalize_task(dd).unwrap()).unwrap();
    assert!(t.contains("OnCalendar=Sun,Sat *-*-* 01:00:00\nOnCalendar=Sun,Sat *-*-* 02:00:00\nAccuracySec=1s\n"), "{t}");
    assert!(!t.contains("Persistent"), "{t}");
}

fn ms(s: &str) -> u64 {
    let t: Timestamp = format!("{s}Z").parse().unwrap();
    t.as_millisecond() as u64
}

fn start<'a>(
    task: &'a rynas_api_types::scheduler::ScheduledTask,
    now: u64,
    last: Option<(u64, u64)>,
    pause: &'a SchedulerPause,
    tz: &'a TimeZone,
) -> Start<'a> {
    Start { task, now_ms: now, by_timer: true, requested: None, last_run: last, pause, running_elsewhere: false, tz }
}

#[test]
fn the_gate_keeps_the_loops_rules() {
    let (pause, tz) = (SchedulerPause::default(), TimeZone::UTC);
    // Docker: a boot inside the window still runs it, after the window not
    let d = normalize_task(system::docker_auto_update(&docker())).unwrap();
    let inside = decide(&start(&d, ms("2026-11-16T05:10:00"), None, &pause, &tz));
    assert_eq!(inside, Decision::Run { trigger: RunTrigger::CatchUp, attempt: 0 });
    let after = decide(&start(&d, ms("2026-11-16T09:00:00"), None, &pause, &tz));
    assert!(matches!(after, Decision::Skip { reason: SkipReason::Window, .. }), "{after:?}");
    // a retry after the window is skipped too
    let mut retry = start(&d, ms("2026-11-16T09:00:00"), None, &pause, &tz);
    retry.by_timer = false;
    retry.requested = Some((RunTrigger::Retry, 1));
    assert!(matches!(decide(&retry), Decision::Skip { reason: SkipReason::Window, .. }));
    // PC maintenance: late up to six hours
    let m = normalize_task(system::pc_maintenance(3)).unwrap();
    let late = decide(&start(&m, ms("2026-11-16T08:30:00"), None, &pause, &tz));
    assert_eq!(late, Decision::Run { trigger: RunTrigger::CatchUp, attempt: 0 });
    let too_late = decide(&start(&m, ms("2026-11-16T09:30:00"), None, &pause, &tz));
    assert!(matches!(too_late, Decision::Skip { reason: SkipReason::Missed, .. }), "{too_late:?}");
    // an interval counts from the last run's end, across reboots
    let c = normalize_task(system::updates_check()).unwrap();
    let end = ms("2026-11-16T01:00:00");
    assert_eq!(interval_due_ms(&c, end), Some(end + 24 * 3_600_000));
    let soon = decide(&start(&c, ms("2026-11-16T09:00:00"), Some((end - 5000, end)), &pause, &tz));
    assert!(matches!(soon, Decision::Skip { reason: SkipReason::NotDue, .. }), "{soon:?}");
    let due = decide(&start(&c, ms("2026-11-17T01:00:00"), Some((end - 5000, end)), &pause, &tz));
    assert_eq!(due, Decision::Run { trigger: RunTrigger::Timer, attempt: 0 });
    assert_eq!(decide(&start(&c, ms("2026-11-16T01:10:00"), None, &pause, &tz)), Decision::Run { trigger: RunTrigger::Timer, attempt: 0 });
    // the random part is stable per run and within the policy's delay
    let i = normalize_task(system::docker_image_check()).unwrap();
    let due = interval_due_ms(&i, end).unwrap();
    assert_eq!(interval_due_ms(&i, end), Some(due));
    assert!((end + 24 * 3_600_000..end + 25 * 3_600_000).contains(&due));
    // run now is never "not due"
    let mut now = start(&c, ms("2026-11-16T09:00:00"), Some((end - 5000, end)), &pause, &tz);
    now.by_timer = false;
    now.requested = Some((RunTrigger::Manual, 0));
    assert_eq!(decide(&now), Decision::Run { trigger: RunTrigger::Manual, attempt: 0 });
}
