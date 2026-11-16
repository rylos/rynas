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
                .args(["scrub", "start", "-B", "-d"])
                .arg(filesystem)
                .env_clear()
                .timeout(SCRUB_TIMEOUT);
            let fs = filesystem.clone();
            Ok(p.ops.tasks.spawn("btrfs.scrub", user, move |ctx| async move {
                // deduplication of the filesystem gives way (D-DEDUP)
                let _fs = crate::fslock::FsLocks::global().heavy_paths(&[&fs], "btrfs.scrub").await;
                ctx.run(&cmd).await.map(|_| ()).map_err(|e| e.to_string())
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
            p.gate.attach_guard(&t.id, guard);
            Ok(Some(JobPlan {
                argv: ["btrfs", "scrub", "start", "-B", "-d"]
                    .iter()
                    .map(|s| s.to_string())
                    .chain([filesystem.clone()])
                    .collect(),
                env: Vec::new(),
                timeout_s: SCRUB_TIMEOUT.as_secs(),
                report: PlanReport::Plain,
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
            // targets that follow the snapshots transfer inside rynasd
            if j.replicas.iter().any(|r| r.enabled && r.after_snapshots) {
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
        .attach_tracker(&t.id, crate::gate::heavy_action(&t.action), tx);
    let p2 = p.clone();
    let r = p.ops.tasks.spawn(kind, crate::scheduler::SCHEDULER_USER, move |ctx| async move {
        ctx.log(&format!("runs in {unit}"));
        let mut feed = journal_lines(&inv);
        let mut rx = rx;
        let mut stopping = false;
        loop {
            tokio::select! {
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
            // `run`, or `snapshot` when the job has replica targets (they transfer after)
            let cmd = rynas_btrfs::replication::snapshot_cmd(&j, SNAPSHOT_TIMEOUT);
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
            ctx.log(&format!(
                "snapshot job {}: {} created, {} deleted",
                j.name,
                r.created.len(),
                r.deleted.len()
            ));
            // targets that follow the snapshots: their failures notify on their own and
            // never fail the snapshot run
            crate::replication::after_snapshots(&ctx, &p, &j).await;
            Ok(())
        })?)
}
