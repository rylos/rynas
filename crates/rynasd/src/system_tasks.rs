//! The scheduler tasks of rynas's own recurring jobs (S3 of `docs/research/scheduling.md`):
//! the "updates available" check, the Docker image check and automatic updates, the app
//! catalog refresh, the PC backup maintenance and retention, and the deduplication windows.
//! Before S3 most of them were loops inside rynasd; now each is a task owned by its feature
//! (`rynas_api_types::scheduler::system`), so it goes through the scheduling gate, shows on
//! the Task Scheduler page with its history, and is paused with the others.
//!
//! - **Migration** ([`ensure`], at every start of rynasd): the tasks the features want are
//!   built from their applied settings (the same times, windows and intervals the loops had)
//!   and applied when the stored ones differ: the first start after the upgrade creates
//!   them, later starts find nothing to do. What a user changed on the Task Scheduler page
//!   (notifications, priority, timeout, retry; whether a task the feature does not switch is
//!   on) is kept.
//! - **Changes**: a feature that changes its schedule stages its tasks with its settings
//!   ([`stage_docker`]) or applies them with its settings ([`apply_pc`]).

use rynas_api_types::dedup::{DEDUP_KEY, DedupConfig};
use rynas_api_types::docker::DockerSettings;
use rynas_api_types::scheduler::system::{self, keep_options};
use rynas_api_types::scheduler::{ScheduledTask, SchedulerConfig, TaskAction};
use rynas_config::Config;
use rynas_scheduler::SCHEDULER_KEY;
use rynas_scheduler::domain::normalize_task;

use crate::ops::Res;
use crate::services::{docs, stage};

/// What the features' settings say about their tasks.
#[derive(Debug, Clone, Default)]
pub struct Wanted {
    /// The Docker settings (`None`: Docker's tasks are not touched).
    pub docker: Option<DockerSettings>,
    /// The PC backups' maintenance hour (`None`: no file-method PC backups in this rynasd).
    pub pc_hour: Option<u8>,
    /// The deduplication settings (`None`: not touched).
    pub dedup: Option<DedupConfig>,
    /// The updates check and the PC folders' retention.
    pub always: bool,
}

/// The tasks to store, each with whether its feature decides when it is on.
pub fn tasks(w: &Wanted) -> Vec<(ScheduledTask, bool)> {
    let mut out = Vec::new();
    if w.always {
        out.push((system::updates_check(), false));
        out.push((system::pc_retention(), false));
    }
    if let Some(d) = &w.docker {
        out.push((system::docker_image_check(), false));
        out.push((system::docker_auto_update(&d.auto_update), true));
        out.push((system::apps_catalog(d.apps_online_catalog), true));
    }
    if let Some(h) = w.pc_hour {
        out.push((system::pc_maintenance(h), false));
    }
    if let Some(dd) = &w.dedup {
        for (key, s) in &dd.jobs {
            out.push((crate::dedup::job_task(key, s), true));
        }
    }
    out
}

/// Puts the wanted tasks into `doc`; `true` when something changed. A task the user made
/// with the same id (another action) is left alone. A deduplication task is only rewritten
/// when it exists (its settings create it).
pub fn merge(doc: &mut SchedulerConfig, wanted: Vec<(ScheduledTask, bool)>, log: &dyn Fn(&str)) -> bool {
    let mut changed = false;
    for (want, feature_enables) in wanted {
        let pos = doc.tasks.iter().position(|t| t.id == want.id);
        let cur = pos.map(|i| doc.tasks[i].clone());
        if let Some(c) = &cur
            && std::mem::discriminant(&c.action) != std::mem::discriminant(&want.action)
        {
            log(&format!("task {} is a task of its own: not replaced by the job of rynas", want.id));
            continue;
        }
        if cur.is_none() && matches!(want.action, TaskAction::Dedup { .. }) {
            continue;
        }
        let new = keep_options(want, cur.as_ref(), feature_enables);
        let new = match normalize_task(new) {
            Ok(t) => t,
            Err(e) => {
                log(&format!("task: {e}"));
                continue;
            }
        };
        let same = cur.as_ref().is_some_and(|c| {
            let mut c = c.clone();
            c.migrate();
            normalize_task(c).is_ok_and(|c| c == new)
        });
        if same {
            continue;
        }
        log(&format!("task {}: {}", new.id, if cur.is_some() { "brought to its feature's settings" } else { "created" }));
        match pos {
            Some(i) => doc.tasks[i] = new,
            None => doc.tasks.push(new),
        }
        changed = true;
    }
    changed
}

/// At the start of rynasd: the tasks of the jobs, applied when they differ from the stored
/// ones (the first start after the upgrade creates them). `Ok(None)`: nothing to change.
pub fn ensure(cfg: &Config, pc_hour: Option<u8>, log: &dyn Fn(&str)) -> Result<Option<u64>, String> {
    let applied = |key: &str| cfg.applied(key).map_err(|e| e.to_string());
    let docker: DockerSettings = match applied(crate::docker::DOCKER_KEY)? {
        Some(v) => serde_json::from_value(v).map_err(|e| e.to_string())?,
        None => DockerSettings::default(),
    };
    let dedup: Option<DedupConfig> = applied(DEDUP_KEY)?.and_then(|v| serde_json::from_value(v).ok());
    let wanted = tasks(&Wanted { docker: Some(docker), pc_hour, dedup, always: true });
    cfg.apply_update("rynas", SCHEDULER_KEY, "scheduled tasks of rynas's own jobs", log, |cur| {
        let mut doc: SchedulerConfig = match cur {
            Some(v) => serde_json::from_value(v).map_err(|e| e.to_string())?,
            None => SchedulerConfig::default(),
        };
        if !merge(&mut doc, wanted, log) {
            return Ok(None);
        }
        serde_json::to_value(doc).map(Some).map_err(|e| e.to_string())
    })
    .map_err(|e| e.to_string())
}

/// Docker settings staged by `owner`: their tasks are staged with them (both go live
/// together).
pub fn stage_docker(cfg: &Config, owner: &str, d: &DockerSettings) -> Res<()> {
    // a rynasd without the Task Scheduler (some test setups): nothing to stage
    if !cfg.keys().iter().any(|k| k == SCHEDULER_KEY) {
        return Ok(());
    }
    let (mut sched, _, _): (SchedulerConfig, SchedulerConfig, _) = docs(cfg, owner, SCHEDULER_KEY)?;
    let wanted = tasks(&Wanted { docker: Some(d.clone()), ..Default::default() });
    if merge(&mut sched, wanted, &|l| tracing::debug!("scheduler: {l}")) {
        stage(cfg, owner, SCHEDULER_KEY, &sched)?;
        cfg.stage_group(owner, &[crate::docker::DOCKER_KEY, SCHEDULER_KEY], false)?;
    }
    Ok(())
}

/// The PC backups' maintenance hour changed (their settings apply at once): its task is
/// applied at once too.
pub fn apply_pc(cfg: &Config, who: &str, hour: u8) -> Result<Option<u64>, String> {
    if !cfg.keys().iter().any(|k| k == SCHEDULER_KEY) {
        return Ok(None);
    }
    let wanted = tasks(&Wanted { pc_hour: Some(hour), ..Default::default() });
    cfg.apply_update(who, SCHEDULER_KEY, "PC backup maintenance hour", &|l| tracing::info!("scheduler: {l}"), |cur| {
        let mut doc: SchedulerConfig = match cur {
            Some(v) => serde_json::from_value(v).map_err(|e| e.to_string())?,
            None => SchedulerConfig::default(),
        };
        if !merge(&mut doc, wanted, &|l| tracing::info!("scheduler: {l}")) {
            return Ok(None);
        }
        serde_json::to_value(doc).map(Some).map_err(|e| e.to_string())
    })
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::scheduler::NotifyPolicy;

    #[test]
    fn merge_creates_keeps_and_leaves_alone() {
        let mut doc = SchedulerConfig::default();
        let w = Wanted { docker: Some(DockerSettings::default()), pc_hour: Some(3), always: true, ..Default::default() };
        assert!(merge(&mut doc, tasks(&w), &|_| {}));
        let ids: Vec<&str> = doc.tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            ids,
            ["updates-check", "pcbackup-retention", "docker-update-check", "docker-auto-update", "apps-catalog-refresh", "pc-maintenance"]
        );
        // a second pass changes nothing (no apply at every start)
        assert!(!merge(&mut doc, tasks(&w), &|_| {}));
        // the user's options stay; a new hour moves the task
        doc.tasks[0].notify = NotifyPolicy::Always;
        doc.tasks[0].enabled = false;
        let w2 = Wanted { pc_hour: Some(5), always: true, ..Default::default() };
        assert!(merge(&mut doc, tasks(&w2), &|_| {}));
        assert_eq!(doc.tasks[0].notify, NotifyPolicy::Always);
        assert!(!doc.tasks[0].enabled);
        let m = doc.tasks.iter().find(|t| t.id == "pc-maintenance").unwrap();
        assert_eq!(m.trigger.on_calendar().as_deref(), Some("*-*-* 05:00:00"));
        // a user task with the same id is not replaced
        let mut mine = SchedulerConfig { tasks: vec![doc.tasks[0].clone()] };
        mine.tasks[0].action = TaskAction::Command { command: "true".into(), user: "root".into() };
        mine.tasks[0].owner = Default::default();
        let before = mine.clone();
        assert!(!merge(&mut mine, tasks(&Wanted { always: true, ..Default::default() }).into_iter().take(1).collect(), &|_| {}));
        assert_eq!(mine, before);
    }

    #[test]
    fn deduplication_tasks_move_to_their_window() {
        let mut dd = DedupConfig::default();
        dd.jobs.insert("vol1".into(), serde_json::from_str("{}").unwrap());
        // as 0.8.15 stored it: one calendar expression
        let old: ScheduledTask = serde_json::from_str(
            r#"{"id":"dedup-vol1","name":"Deduplication vol1","description":"","enabled":true,"action":{"kind":"dedup","volume":"vol1"},"trigger":{"kind":"calendar","on_calendar":"*-*-* 01:00:00","persistent":false,"randomized_delay_s":0},"notify":"never"}"#,
        )
        .unwrap();
        let mut doc = SchedulerConfig { tasks: vec![old] };
        assert!(merge(&mut doc, tasks(&Wanted { dedup: Some(dd.clone()), ..Default::default() }), &|_| {}));
        assert!(matches!(&doc.tasks[0].trigger, rynas_api_types::scheduler::TaskTrigger::Window { stop_at_end: true, .. }));
        assert!(!doc.tasks[0].effective_policy().missed.catches_up());
        // a job without a task (off and never saved) gets none
        let mut empty = SchedulerConfig::default();
        assert!(!merge(&mut empty, tasks(&Wanted { dedup: Some(dd), ..Default::default() }), &|_| {}));
    }

    /// The first start after the upgrade applies the tasks (units written, timers started);
    /// the next start finds nothing to do.
    #[test]
    fn ensure_applies_once() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let sys = Arc::new(rynas_config::MemorySystem::new("nas"));
        let cfg = Config::open(
            rynas_config::ConfigOptions::new(dir.path().join("state"), &root),
            sys.clone(),
            vec![Arc::new(rynas_scheduler::SchedulerDomain::new(
                rynas_scheduler::units::Paths::default(),
                Arc::new(|_: &str| None),
            ))],
        )
        .unwrap();
        assert!(ensure(&cfg, Some(3), &|_| {}).unwrap().is_some());
        let doc: SchedulerConfig = serde_json::from_value(cfg.applied(SCHEDULER_KEY).unwrap().unwrap()).unwrap();
        assert_eq!(doc.tasks.len(), 6);
        let timer = std::fs::read_to_string(root.join("etc/systemd/system/rynas-task-pc-maintenance.timer")).unwrap();
        assert!(timer.contains("OnCalendar=*-*-* 03:00:00\nPersistent=true\n"), "{timer}");
        assert!(sys.state().calls.iter().any(|c| c.contains("rynas-task-updates-check.timer")));
        assert_eq!(ensure(&cfg, Some(3), &|_| {}).unwrap(), None);
        // a new maintenance hour, at once
        assert!(apply_pc(&cfg, "admin", 4).unwrap().is_some());
        let timer = std::fs::read_to_string(root.join("etc/systemd/system/rynas-task-pc-maintenance.timer")).unwrap();
        assert!(timer.contains("OnCalendar=*-*-* 04:00:00\n"), "{timer}");
    }
}
