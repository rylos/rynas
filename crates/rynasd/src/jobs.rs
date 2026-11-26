//! Internal actions of scheduler tasks. Started by `rynasctl job run <id>` from the task's
//! unit: an action that is one command (scrub, SMART self-test, a snapshot job without
//! targets that follow it) is handed to the unit as a [`JobPlan`] and runs there, outside
//! rynasd, in the unit's sandbox (`rynas_scheduler::units::JOB_SANDBOX`), with an entry in
//! the task panel that follows it and stops the unit when cancelled ([`track`]); the others
//! run as rynasd tasks (UPID, log, cancel; visible in the task panel),
//! which the unit follows to their end so that the unit's result is the task's.

use std::sync::Arc;
use std::time::Duration;

use rynas_api_types::scheduler::{ScheduledTask, SmartTestKind, TaskAction};
use rynas_api_types::snapshots::SnapshotsConfig;
use rynas_btrfs::SNAPSHOTS_KEY;
use rynas_btrfs::btrbk::parse_report;
use rynas_tasks::Cmd;
use serde::{Deserialize, Serialize};

use crate::ops::{Error, Res};
use crate::protection::{Protection, invalid, not_found};

const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(6 * 3600);
const SCRUB_TIMEOUT: Duration = Duration::from_secs(72 * 3600);
const SMART_TIMEOUT: Duration = Duration::from_secs(120);

/// Starts the rynasd task of an internal action; returns its UPID.
pub async fn spawn(p: &Arc<Protection>, t: &ScheduledTask, user: &str) -> Res<String> {
    match &t.action {
        TaskAction::Snapshot { job } => spawn_snapshot(p, job, user).await,
        TaskAction::Replicate { job, target } => {
            crate::replication::spawn_transfer(p, job, target, user).await
        }
        TaskAction::Scrub { filesystem } => {
            let cmd = Cmd::new("btrfs")
                .args(["scrub", "start", "-B", "-d", "-c", "3"])
                .arg(filesystem)
                .env_clear()
                .timeout(SCRUB_TIMEOUT);
            let fs = filesystem.clone();
            Ok(p.ops.tasks.spawn_on("btrfs.scrub", Some(filesystem.as_str()), user, move |ctx| async move {
                // deduplication of the filesystem gives way (D-DEDUP)
                let _fs = crate::fslock::FsLocks::global().heavy_paths(&[&fs], "btrfs.scrub").await;
                let run = ctx.run(&cmd);
                tokio::pin!(run);
                let mut tick = scrub_ticker();
                loop {
                    tokio::select! {
                        r = &mut run => return r.map(|_| ()).map_err(|e| e.to_string()),
                        _ = tick.tick() => scrub_progress(&ctx, &fs).await,
                    }
                }
            })?)
        }
        TaskAction::SmartTest { disk, test } => {
            let kind = match test {
                SmartTestKind::Short => "short",
                SmartTestKind::Long => "long",
            };
            let cmd = Cmd::new("smartctl")
                .args(["-t", kind])
                .arg(format!("/dev/{disk}"))
                .env_clear()
                .timeout(SMART_TIMEOUT);
            spawn_cmd(p, "smart.test", user, cmd)
        }
        TaskAction::RecycleCleanup { share, days } => {
            crate::services::spawn_recycle_cleanup(&p.ops, user, share.clone(), *days).await
        }
        TaskAction::PowerShutdown { .. } => Err(invalid(
            "scheduled shutdowns are started by rynasctl power scheduled-shutdown",
        )),
        TaskAction::Backup { job } => crate::restic_backup::spawn_run(user, job),
        TaskAction::Dedup { volume } => crate::dedup::spawn_scheduled(volume, user).await,
        // the jobs rynasd ran in its own loops before S3
        TaskAction::UpdatesCheck => crate::updates::spawn_scheduled_check(user),
        TaskAction::KernelUpdate => crate::kernel_updates::spawn_auto(user),
        TaskAction::DockerImageCheck => {
            crate::docker_auto::spawn_image_check(p, t, user).map_err(Error::internal)
        }
        TaskAction::DockerAutoUpdate => {
            crate::docker_auto::spawn_auto_window(p, t, user).map_err(Error::internal)
        }
        TaskAction::AppsCatalog => crate::apps::online::spawn_refresh(p, user).map_err(Error::internal),
        TaskAction::PcMaintenance => crate::pc_agent::maint::spawn_nightly(p, user).map_err(Error::internal),
        TaskAction::PcRetention => crate::pc_backup::spawn_retention(p, user).map_err(Error::internal),
        TaskAction::SystemUpgrade => Err(invalid(
            "the automatic system updates run on the system's own timer",
        )),
        TaskAction::MdCheck { .. } => Err(invalid("the RAID check runs on the system's own timers")),
        TaskAction::Command { .. } | TaskAction::Script { .. } => {
            Err(invalid("not an internal action"))
        }
    }
}

fn spawn_cmd(p: &Arc<Protection>, kind: &str, user: &str, cmd: Cmd) -> Res<String> {
    Ok(p.ops.tasks.spawn(kind, user, move |ctx| async move {
        ctx.run(&cmd).await.map(|_| ()).map_err(|e| e.to_string())
    })?)
}

/// A command the task unit runs itself (`rynasctl job run`), outside rynasd: a restart of
/// rynasd (a package upgrade) no longer ends a running scrub or snapshot (I-11 of
/// `docs/research/scheduling.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobPlan {
    /// Program (looked up in `PATH`) and arguments.
    pub argv: Vec<String>,
    /// The whole environment of the command (`PATH` added when missing).
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// The command is stopped after this long.
    pub timeout_s: u64,
    #[serde(default)]
    pub report: PlanReport,
    /// Exit codes that are a success too (`btrfs scrub resume` gives 2 when the scrub had
    /// nothing left to resume).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ok_codes: Vec<i32>,
    /// rynasd's state directory when the run may be asked to give way (a scrub, S4): a
    /// failed exit of a run listed there gave way, and the unit ends as a success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yield_state: Option<String>,
    /// The run was asked to give way before its command started: nothing runs, the unit ends
    /// as a run that gave way.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gave_way: bool,
}

/// What the unit prints of the command's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlanReport {
    /// As it comes.
    #[default]
    Plain,
    /// `btrbk --format=raw` on standard output: a summary of what it created and deleted.
    Btrbk,
}

/// The command an internal action is, when it is one command, prepared for the unit to run:
/// a scrub (its filesystem lock taken now, held by the run's lease until it ends: a
/// deduplication there gives way first), a SMART self-test, a snapshot job without targets
/// that follow it. `None`: the action runs as a rynasd task ([`spawn`]).
pub async fn plan(p: &Arc<Protection>, t: &ScheduledTask) -> Res<Option<JobPlan>> {
    match &t.action {
        TaskAction::Scrub { filesystem } => {
            let guard = crate::fslock::FsLocks::global()
                .heavy_paths(&[filesystem], "btrfs.scrub")
                .await;
            p.gate.attach_guard(&t.id, || crate::gate::holder_of(p, t), guard);
            let inv = p.gate.lease_invocation(&t.id);
            // asked to give way before it began: it gives way now, nothing to cancel (the
            // signal may have come before the command existed)
            if p.gate.yield_pending(&t.id, &inv) {
                return Ok(Some(JobPlan {
                    argv: vec!["true".into()],
                    env: Vec::new(),
                    timeout_s: 60,
                    report: PlanReport::Plain,
                    ok_codes: Vec::new(),
                    yield_state: Some(p.state_dir.display().to_string()),
                    gave_way: true,
                }));
            }
            // a scrub that gave way to more urgent work goes on from where it stopped (S4);
            // its entry goes now, so this run holds a normal lease that can give way again
            let resume = p.gate.yields().get(&t.id).is_some_and(|e| e.filesystem == *filesystem && e.invocation_id != inv);
            if resume {
                p.gate.drop_yield(&t.id);
            }
            let verb = if resume { "resume" } else { "start" };
            Ok(Some(JobPlan {
                // `-c 3`: the idle I/O class, the disks serve everything else first
                argv: ["btrfs", "scrub", verb, "-B", "-d", "-c", "3"]
                    .iter()
                    .map(|s| s.to_string())
                    .chain([filesystem.clone()])
                    .collect(),
                env: Vec::new(),
                timeout_s: SCRUB_TIMEOUT.as_secs(),
                report: PlanReport::Plain,
                ok_codes: if resume { vec![2] } else { Vec::new() },
                yield_state: Some(p.state_dir.display().to_string()),
                gave_way: false,
            }))
        }
        TaskAction::SmartTest { disk, test } => {
            let kind = match test {
                SmartTestKind::Short => "short",
                SmartTestKind::Long => "long",
            };
            Ok(Some(JobPlan {
                argv: vec!["smartctl".into(), "-t".into(), kind.into(), format!("/dev/{disk}")],
                env: Vec::new(),
                timeout_s: SMART_TIMEOUT.as_secs(),
                report: PlanReport::Plain,
                ok_codes: Vec::new(),
                yield_state: None,
                gave_way: false,
            }))
        }
        TaskAction::Snapshot { job } => {
            let p2 = p.clone();
            let doc: SnapshotsConfig = tokio::task::spawn_blocking(move || {
                crate::services::from_value(p2.ops.config.applied(SNAPSHOTS_KEY)?)
            })
            .await
            .map_err(Error::internal)??;
            let j = doc
                .jobs
                .into_iter()
                .find(|j| j.name == *job)
                .ok_or_else(|| not_found(format!("applied snapshot job {job}")))?;
            // targets that follow the snapshots transfer inside rynasd; pinned snapshots
            // need rynas's own pruning after the run (spawn_snapshot)
            if j.replicas.iter().any(|r| r.enabled && r.after_snapshots) || !pinned_of(&j).is_empty() {
                return Ok(None);
            }
            let dir = j.snapshot_dir.clone();
            let host = p.snapshots.clone();
            tokio::task::spawn_blocking(move || host.host().ensure_dir(&dir))
                .await
                .map_err(Error::internal)?
                .map_err(Error::internal)?;
            let cmd = rynas_btrfs::replication::snapshot_cmd(&j, SNAPSHOT_TIMEOUT);
            Ok(Some(JobPlan {
                argv: cmd.argv_lossy(),
                env: cmd.env_lossy(),
                timeout_s: SNAPSHOT_TIMEOUT.as_secs(),
                report: PlanReport::Btrbk,
                ok_codes: Vec::new(),
                yield_state: None,
                gave_way: false,
            }))
        }
        _ => Ok(None),
    }
}

/// The task panel's entry of a run working in its own unit (a planned scrub, SMART test or
/// snapshot), as when it ran inside rynasd: its log follows the unit's output in the journal,
/// it ends with the run (told by the run's end, [`crate::gate::Gate::release`]), and
/// cancelling it stops the unit.
pub fn track(p: &Arc<Protection>, t: &ScheduledTask) {
    let kind = match &t.action {
        TaskAction::Scrub { .. } => "btrfs.scrub",
        TaskAction::SmartTest { .. } => "smart.test",
        TaskAction::Snapshot { .. } => "snapshot.run",
        _ => return,
    };
    let unit = rynas_scheduler::units::service_name(&t.id);
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    let inv = p
        .gate
        .attach_tracker(&t.id, || crate::gate::holder_of(p, t), tx);
    let p2 = p.clone();
    // a scrub's progress, read from the kernel as the Storage page's scrub does
    let scrub_fs = match &t.action {
        TaskAction::Scrub { filesystem } => Some(filesystem.clone()),
        _ => None,
    };
    let subject = scrub_fs.clone();
    let r = p.ops.tasks.spawn_on(kind, subject.as_deref(), crate::scheduler::SCHEDULER_USER, move |ctx| async move {
        ctx.log(&format!("runs in {unit}"));
        let mut feed = journal_lines(&inv);
        let mut rx = rx;
        let mut stopping = false;
        let mut tick = scrub_ticker();
        loop {
            tokio::select! {
                _ = tick.tick(), if scrub_fs.is_some() && !stopping => {
                    if let Some(fs) = &scrub_fs {
                        scrub_progress(&ctx, fs).await;
                    }
                }
                end = &mut rx => {
                    // the last lines journald still had
                    if let Some((_, lines)) = feed.as_mut() {
                        while let Ok(Ok(Some(l))) =
                            tokio::time::timeout(Duration::from_millis(300), lines.next_line()).await
                        {
                            ctx.log(&l);
                        }
                    }
                    return match end {
                        Ok(r) if r == "success" => Ok(()),
                        Ok(r) if stopping => Err(format!("cancelled: the unit stopped ({r})")),
                        Ok(r) => Err(format!("the run failed ({r})")),
                        Err(_) => Err("the run ended (its result is in the task's history)".into()),
                    };
                }
                _ = ctx.token().cancelled(), if !stopping => {
                    stopping = true;
                    ctx.log(&format!("stopping {unit}"));
                    let (p3, u) = (p2.clone(), unit.clone());
                    let _ = tokio::task::spawn_blocking(move || {
                        p3.ops.config.system().run(&Cmd::new("systemctl").args(["stop", "--no-block", &u]))
                    })
                    .await;
                }
                line = next_line(&mut feed) => match line {
                    Some(l) => ctx.log(&l),
                    None => feed = None,
                },
            }
        }
    });
    if let Err(e) = r {
        tracing::warn!("scheduler: task {}: task panel entry: {e}", t.id);
    }
}

/// How often a running scrub is asked how far it is (an ioctl: no disk I/O).
const SCRUB_POLL: Duration = Duration::from_secs(10);

/// The ticks of [`scrub_progress`]: the first one after [`SCRUB_POLL`], a late one never
/// bunched up.
fn scrub_ticker() -> tokio::time::Interval {
    let mut t = tokio::time::interval_at(tokio::time::Instant::now() + SCRUB_POLL, SCRUB_POLL);
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    t
}

/// The task's progress from `btrfs scrub status -R` of the running scrub of `fs` (bytes
/// scrubbed of the bytes used), as the Storage page's scrub reads it; nothing logged.
async fn scrub_progress(ctx: &rynas_tasks::TaskCtx, fs: &str) {
    let cmd = Cmd::new("btrfs").args(["scrub", "status", "-R"]).arg(fs).env_clear().timeout(Duration::from_secs(30));
    let Ok(out) = cmd.run(None, None).await else { return };
    let st = rynas_storage::btrfs::parse_scrub(&out.stdout);
    if let Some(f) = rynas_storage::btrfs::scrub_fraction(&st) {
        ctx.progress((f * 100.0).clamp(0.0, 99.0) as u8);
    }
}

type JournalFeed = (
    tokio::process::Child,
    tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
);

/// `journalctl --follow` of one run's output (messages only); `None` without an invocation
/// id or without journalctl. The child goes with the feed.
fn journal_lines(invocation_id: &str) -> Option<JournalFeed> {
    use tokio::io::AsyncBufReadExt;
    if invocation_id.is_empty() || !invocation_id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut child = tokio::process::Command::new("journalctl")
        .args(["--no-pager", "-o", "cat", "--follow", "-n", "all"])
        .arg(format!("_SYSTEMD_INVOCATION_ID={invocation_id}"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let out = child.stdout.take()?;
    Some((child, tokio::io::BufReader::new(out).lines()))
}

/// The next line of the feed; never ready without one.
async fn next_line(feed: &mut Option<JournalFeed>) -> Option<String> {
    match feed {
        Some((_, lines)) => lines.next_line().await.ok().flatten(),
        None => std::future::pending().await,
    }
}

/// The pinned snapshots (Storage > Space) of a job: absolute paths in its snapshot folder.
fn pinned_of(j: &SnapshotsJobRef) -> std::collections::BTreeSet<String> {
    let dir = std::path::Path::new(&j.snapshot_dir);
    crate::space::pins::get()
        .map(|p| p.all())
        .unwrap_or_default()
        .into_iter()
        .filter(|p| std::path::Path::new(p).parent() == Some(dir))
        .collect()
}

type SnapshotsJobRef = rynas_api_types::snapshots::SnapshotJob;

/// Deletes what the job's retention would delete now, except the pinned snapshots (under
/// the job's lock, never the newest); the names deleted.
fn prune_except(
    state_dir: &std::path::Path,
    j: &SnapshotsJobRef,
    pinned: &std::collections::BTreeSet<String>,
) -> Result<Vec<String>, String> {
    let dir = std::path::Path::new(&j.snapshot_dir);
    let latest: Vec<Option<String>> = j
        .replicas
        .iter()
        .filter(|t| t.enabled)
        .map(|t| rynas_btrfs::replication::read_status(state_dir, &j.name, &t.id).latest)
        .collect();
    let mut gone = Vec::new();
    for n in prune_candidates(&crate::snapshots::would_prune(state_dir, j)?, j, pinned, &latest) {
        if rynas_btrfs::scan::delete(dir, &j.name, &n, &rynas_btrfs::scan::lock_path(&j.name)).is_ok() {
            gone.push(n);
        }
    }
    Ok(gone)
}

/// What rynas's own prune may delete of the names btrbk's dry run gave: never a pinned
/// snapshot, never one a replica target still needs (its latest replica and everything newer,
/// which it has not received yet; all of them while a target has none: as btrbk keeps them).
fn prune_candidates(
    names: &[String],
    j: &SnapshotsJobRef,
    pinned: &std::collections::BTreeSet<String>,
    latest: &[Option<String>],
) -> Vec<String> {
    let dir = std::path::Path::new(&j.snapshot_dir);
    names
        .iter()
        .filter(|n| rynas_btrfs::name::split_snapshot_name(n).is_some_and(|(p, _)| p == j.name))
        .filter(|n| !pinned.contains(&dir.join(n).display().to_string()))
        .filter(|n| !crate::space::discover::replica_needed(n, latest))
        .cloned()
        .collect()
}

/// Task `snapshot.run`: `btrbk -c /etc/rynas/btrbk/<job>.conf run --format=raw`, with a
/// summary of what it created and deleted.
pub async fn spawn_snapshot(p: &Arc<Protection>, job: &str, user: &str) -> Res<String> {
    let p2 = p.clone();
    let doc: SnapshotsConfig = tokio::task::spawn_blocking(move || {
        crate::services::from_value(p2.ops.config.applied(SNAPSHOTS_KEY)?)
    })
    .await
    .map_err(Error::internal)??;
    let j = doc
        .jobs
        .into_iter()
        .find(|j| j.name == job)
        .ok_or_else(|| not_found(format!("applied snapshot job {job}")))?;
    let p = p.clone();
    Ok(p.ops
        .tasks
        .clone()
        .spawn("snapshot.run", user, move |ctx| async move {
            let dir = j.snapshot_dir.clone();
            let host = p.snapshots.clone();
            tokio::task::spawn_blocking(move || host.host().ensure_dir(&dir))
                .await
                .map_err(|e| e.to_string())??;
            // `run`, or `snapshot` when the job has replica targets (they transfer after);
            // with pinned snapshots btrbk deletes none and rynas prunes the rest below
            let pinned = pinned_of(&j);
            let mut cmd = rynas_btrfs::replication::snapshot_cmd(&j, SNAPSHOT_TIMEOUT);
            if !pinned.is_empty() {
                cmd = cmd.arg("--preserve-snapshots");
            }
            ctx.log(&format!("$ {}", cmd.display()));
            let out = cmd
                .run(Some(ctx.token()), None)
                .await
                .map_err(|e| format!("btrbk: {e}"))?;
            let r = parse_report(&out.stdout);
            for n in &r.created {
                ctx.log(&format!("created {n}"));
            }
            for n in &r.deleted {
                ctx.log(&format!("deleted {n} (retention)"));
            }
            for l in out.stderr.lines().filter(|l| !l.trim().is_empty()) {
                ctx.log(&format!("stderr: {l}"));
            }
            if !r.errors.is_empty() {
                return Err(format!("btrbk: {}", r.errors.join("; ")));
            }
            if !out.success() {
                return Err(format!("btrbk exited with code {:?}", out.code));
            }
            let mut deleted = r.deleted.len();
            if !pinned.is_empty() {
                let (sd, j2) = (p.state_dir.clone(), j.clone());
                // the snapshot is taken: a failed prune only warns, never fails the run nor
                // holds back the replicas that follow
                match tokio::task::spawn_blocking(move || prune_except(&sd, &j2, &pinned)).await {
                    Ok(Ok(gone)) => {
                        for n in &gone {
                            ctx.log(&format!("deleted {n} (retention; pinned snapshots kept)"));
                        }
                        deleted += gone.len();
                    }
                    Ok(Err(e)) => ctx.log(&format!("warning: old snapshots not pruned: {e}")),
                    Err(e) => ctx.log(&format!("warning: old snapshots not pruned: {e}")),
                }
            }
            ctx.log(&format!(
                "snapshot job {}: {} created, {} deleted",
                j.name,
                r.created.len(),
                deleted
            ));
            // targets that follow the snapshots: their failures notify on their own and
            // never fail the snapshot run
            crate::replication::after_snapshots(&ctx, &p, &j).await;
            Ok(())
        })?)
}

#[cfg(test)]
mod prune_tests {
    use super::*;

    #[test]
    fn own_prune_takes_only_this_folder_and_keeps_what_targets_need() {
        // a job whose local replica target keeps fewer replicas than the job keeps snapshots
        let j: SnapshotsJobRef = serde_json::from_value(serde_json::json!({
            "name": "docs",
            "subvolume": "/vol1/docs",
            "snapshot_dir": "/vol1/.snapshots/docs",
            "retention": {"hourly": 48, "daily": 14},
            "replicas": [{"id": "copy", "destination": {"kind": "local", "path": "/vol2/.replicas/docs"}, "retention": {"daily": 2}}]
        }))
        .unwrap();
        let line = |t: &str, url: &str| format!("format=\"transaction\" type=\"{t}\" status=\"dryrun_success\" target_url=\"{url}\"\n");
        let out = [
            line("delete_snapshot", "/vol1/.snapshots/docs/docs.20261101T0100"),
            line("delete_snapshot", "/vol1/.snapshots/docs/docs.20261102T0100"),
            line("delete_snapshot", "/vol1/.snapshots/docs/docs.20261103T0100"),
            // the target's shorter retention: backups, never taken
            line("delete_target", "/vol2/.replicas/docs/docs.20261104T0100"),
            // same name in another folder, a nested path, a remote URL
            line("delete_snapshot", "/vol2/.replicas/docs/docs.20261105T0100"),
            line("delete_snapshot", "/vol1/.snapshots/docs/x/docs.20261106T0100"),
            line("delete_snapshot", "ssh://nas/vol1/.snapshots/docs/docs.20261107T0100"),
        ]
        .concat();
        let names = rynas_btrfs::btrbk::snapshot_deletions(&out, std::path::Path::new(&j.snapshot_dir));
        assert_eq!(names, ["docs.20261101T0100", "docs.20261102T0100", "docs.20261103T0100"]);
        let pinned: std::collections::BTreeSet<String> = ["/vol1/.snapshots/docs/docs.20261101T0100".to_string()].into();
        // the target last received 0102: 0102 and newer stay
        let v = prune_candidates(&names, &j, &pinned, &[Some("docs.20261102T0100".into())]);
        assert!(v.is_empty(), "{v:?}");
        let v = prune_candidates(&names, &j, &Default::default(), &[Some("docs.20261103T0100".into())]);
        assert_eq!(v, ["docs.20261101T0100", "docs.20261102T0100"]);
        // a target without any replica yet: nothing goes
        assert!(prune_candidates(&names, &j, &Default::default(), &[None]).is_empty());
        // no target: only the pinned one stays
        assert_eq!(prune_candidates(&names, &j, &pinned, &[]), ["docs.20261102T0100", "docs.20261103T0100"]);
    }
}
