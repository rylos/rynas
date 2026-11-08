//! Internal actions of scheduler tasks, run as rynasd tasks (UPID, log, cancel; visible in
//! the task panel). Started by `rynasctl job run <id>` from the task's unit, which follows
//! the task to its end so that the unit's result is the task's.

use std::sync::Arc;
use std::time::Duration;

use rynas_api_types::scheduler::{ScheduledTask, SmartTestKind, TaskAction};
use rynas_api_types::snapshots::SnapshotsConfig;
use rynas_btrfs::SNAPSHOTS_KEY;
use rynas_btrfs::btrbk::parse_report;
use rynas_tasks::Cmd;

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
            spawn_cmd(p, "btrfs.scrub", user, cmd)
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
