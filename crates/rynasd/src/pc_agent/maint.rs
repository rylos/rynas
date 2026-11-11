//! The NAS' side of the PC repositories: pairing, revocation, recovery kits, streamed
//! downloads, run reports, and the maintenance (retention, prune, check) with its memory
//! budget, schedule and notifications.
//!
//! **Memory of a maintenance** (D-PCBACKUP, `docs/research/pc-backup.md` §4.7): prune, check
//! and forget run in `rynas-backup-worker` (`rustic_core`, which loads only the index a
//! command needs) inside a transient unit whose `MemoryMax` is the budget
//! (`maintenance_memory_pct` of MemTotal, `MemoryHigh` 3/4 of it), with `Nice=19` and idle
//! I/O (the worker unit's own properties). Before a prune the blobs are estimated from the
//! size of the index files ([`blobs_estimate`]: nothing loaded; counting them with the worker
//! would need as much memory as the prune) and the need as [`PRUNE_BASE`] + blobs ×
//! [`PRUNE_PER_BLOB`]; a prune that would not fit is postponed with the reason shown on the
//! group (forget still runs: it reads only the snapshots).

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use rynas_api_types::backup::{WorkerEvent, WorkerOp};
use rynas_api_types::error_codes;
use rynas_api_types::notify::{NotifyCategory, NotifySeverity};
use rynas_api_types::pcagent::*;
use rynas_backup::{Launch, Limits, run_worker};
use rynas_notify::event::{code, field, firing, resolved};
use rynas_tasks::{Cmd, TaskCtx};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{Alert, Dev, Grp, HISTORY, Live, Pairing, PcAgent, READ_TIMEOUT, Registry, now_ms};
use crate::ops::{Error, Res};

/// Memory a prune or check needs besides the index (the worker, its buffers).
pub const PRUNE_BASE: u64 = 32 << 20;
/// Memory per blob of the index during prune and check: measured 198 (prune) and 236
/// (check) bytes per blob on 0.3 and 1.2 million blobs (research doc §4.7).
pub const PRUNE_PER_BLOB: u64 = 250;
/// Bytes of the index files per blob on disk (compressed JSON; the 32-byte blob ids do not
/// compress): measured 40.4 on 1.2 million blobs. The estimate reads only file sizes.
pub const INDEX_BYTES_PER_BLOB: u64 = 40;

/// Blobs in a repository, from the size of its index files (nothing decrypted or loaded).
pub fn blobs_estimate(repo: &Path) -> u64 {
    dir_size(&repo.join("index")) / INDEX_BYTES_PER_BLOB
}
/// A maintenance waits this long after the last upload of a group.
const QUIET: Duration = Duration::from_secs(10 * 60);
/// No event from a maintenance worker for this long: it is stopped.
const STALL: Duration = Duration::from_secs(30 * 60);
const BTRFS_SUPER_MAGIC: i64 = 0x9123_683E;

// ------------------------------------------------------------------ memory

/// MemTotal in bytes (0 when unreadable).
pub fn mem_total() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| {
            m.lines()
                .find_map(|l| l.strip_prefix("MemTotal:"))
                .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        })
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

/// The maintenance budget: `pct` % of the RAM, at least 256 MiB.
pub fn memory_budget(pct: u8) -> u64 {
    let pct = if pct == 0 { 40 } else { pct.clamp(10, 80) };
    (mem_total() / 100 * u64::from(pct)).max(256 << 20)
}

/// What a prune of an index with `blobs` blobs is expected to need.
pub fn prune_estimate(blobs: u64) -> u64 {
    PRUNE_BASE + blobs.saturating_mul(PRUNE_PER_BLOB)
}

/// The worker launch of a maintenance: the unit with the budget as its memory limits.
fn maintenance_launch(l: &Launch, budget: u64) -> Launch {
    match l {
        Launch::Unit { exe, .. } => Launch::Unit {
            exe: exe.clone(),
            memory_high: format!("{}M", (budget / 4 * 3) >> 20),
            memory_max: format!("{}M", budget >> 20),
        },
        other => other.clone(),
    }
}

// ------------------------------------------------------------------ used bytes

fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                stack.push(e.path());
            } else if m.is_file() {
                total += m.len();
            }
        }
    }
    total
}

/// The group's bytes on disk (counted once, then kept up to date by the uploads).
pub fn used_bytes(svc: &PcAgent, group: &str, repo: &Path) -> u64 {
    if let Some(u) = svc.live.lock().unwrap_or_else(|p| p.into_inner()).get(group).and_then(|l| l.used) {
        return u;
    }
    let n = dir_size(repo);
    svc.live
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(group.to_string())
        .or_default()
        .used
        .get_or_insert(n);
    n
}

/// Counts every group not counted yet (the console's first look).
pub fn count_used(svc: &PcAgent, reg: &Registry) {
    for (n, g) in &reg.groups {
        used_bytes(svc, n, Path::new(&g.repo));
    }
}

/// A new file of `size` bytes from `device`.
pub fn add_used(svc: &PcAgent, group: &str, device: &str, size: u64) {
    {
        let mut live = svc.live.lock().unwrap_or_else(|p| p.into_inner());
        let l = live.entry(group.to_string()).or_default();
        if let Some(u) = &mut l.used {
            *u += size;
        }
    }
    let _ = svc.edit(|r| {
        if let Some(d) = r.devices.get_mut(device) {
            d.uploaded_bytes += size;
        }
        Ok(())
    });
}

// ------------------------------------------------------------------ worker calls

async fn op(svc: &PcAgent, g: &Grp, name: &str, op: WorkerOp, launch: &Launch, ctx: Option<&TaskCtx>) -> Result<WorkerEvent, String> {
    let req = svc.request(g, op).map_err(|e| e.2)?;
    let limits = Limits {
        stall: Some(STALL),
        timeout: ctx.is_none().then_some(READ_TIMEOUT),
        ..Default::default()
    };
    let cancel = ctx.map(|c| c.token().clone()).unwrap_or_default();
    run_worker(launch, &format!("pc-{name}"), &req, &cancel, &limits, |ev| {
        if let Some(c) = ctx {
            match ev {
                WorkerEvent::Step { title } => c.log(title),
                WorkerEvent::Log { text } => c.log(text),
                _ => {}
            }
        }
    })
    .await
}

// ------------------------------------------------------------------ pairing

fn on_btrfs(p: &Path) -> bool {
    let Ok(c) = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    // SAFETY: statfs fills the struct it is given; the path is a valid C string
    unsafe {
        let mut s: libc::statfs = std::mem::zeroed();
        libc::statfs(c.as_ptr(), &mut s) == 0 && s.f_type as i64 == BTRFS_SUPER_MAGIC
    }
}

/// The repository folder of a new group: a subvolume on btrfs, `0700 root`.
fn make_repo_dir(svc: &PcAgent, base: &Path, name: &str) -> Res<PathBuf> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o750)
        .create(base)
        .map_err(|e| Error::internal(format!("{}: {e}", base.display())))?;
    let p = base.join(name);
    if p.exists() {
        return Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::ALREADY_EXISTS,
            format!("{} exists already", p.display()),
        ));
    }
    if !svc.dry && on_btrfs(base) {
        let o = Cmd::new("btrfs")
            .args(["subvolume", "create"])
            .arg(&p)
            .run_blocking()
            .map_err(|e| Error::internal(format!("btrfs subvolume create: {e}")))?;
        if !o.success() {
            return Err(Error::internal(format!("btrfs subvolume create: {}", o.stderr.trim())));
        }
    } else {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&p)
            .map_err(|e| Error::internal(format!("{}: {e}", p.display())))?;
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).map_err(Error::internal)?;
    Ok(p)
}

fn remove_repo_dir(p: &Path, dry: bool) -> Result<(), String> {
    if !p.exists() {
        return Ok(());
    }
    // a subvolume's root has inode 256
    use std::os::unix::fs::MetadataExt;
    let sub = !dry && std::fs::metadata(p).is_ok_and(|m| m.ino() == 256) && on_btrfs(p);
    if sub {
        let o = Cmd::new("btrfs")
            .args(["subvolume", "delete"])
            .arg(p)
            .run_blocking()
            .map_err(|e| e.to_string())?;
        if !o.success() {
            return Err(o.stderr.trim().to_string());
        }
        Ok(())
    } else {
        std::fs::remove_dir_all(p).map_err(|e| e.to_string())
    }
}

fn unique(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_string();
    }
    (2..1000)
        .map(|i| {
            let suffix = format!("-{i}");
            let keep = 32 - suffix.len();
            format!("{}{suffix}", base[..base.len().min(keep)].trim_end_matches('-'))
        })
        .find(|n| !taken(n))
        .unwrap_or_else(|| format!("pc-{}", now_ms() % 100_000))
}

/// A pairing code was accepted: the device (and its group, unless it joins one), its key
/// file and token.
pub async fn pair(svc: &Arc<PcAgent>, p: &Pairing, settings: &PcAgentSettings, req: &PairRequest) -> Res<PairResponse> {
    let reg = svc.snapshot();
    let device = unique(&device_from_hostname(&req.hostname), |n| reg.devices.contains_key(n));
    let token = super::new_token()?;
    let pc_password = rynas_backup::repo::generate_password().map_err(Error::internal)?;
    let mut created: Option<(String, Grp, PathBuf)> = None;
    let (group, grp) = match &p.group {
        Some(g) => (g.clone(), svc.group(g)?),
        None => {
            let name = unique(&device, |n| reg.groups.contains_key(n));
            let (s2, base, n2) = (svc.clone(), PathBuf::from(&settings.folder), name.clone());
            let dir = tokio::task::spawn_blocking(move || make_repo_dir(&s2, &base, &n2))
                .await
                .map_err(Error::internal)??;
            let nas = rynas_backup::repo::generate_password().map_err(Error::internal)?;
            let g = Grp {
                settings: PcGroupSettings {
                    description: p.description.clone(),
                    quota_gib: p.quota_gib.unwrap_or(settings.default_quota_gib),
                },
                repo: dir.display().to_string(),
                created_ms: now_ms(),
                sealed: svc.seal(&nas)?,
                check_slice: 1,
                ..Default::default()
            };
            let undo = |e: String| {
                let _ = remove_repo_dir(&dir, svc.dry);
                Error::new(StatusCode::BAD_GATEWAY, error_codes::BACKEND_UNAVAILABLE, format!("creating the repository: {e}"))
            };
            match op(svc, &g, &name, WorkerOp::InitPc, &svc.launch, None).await {
                Ok(WorkerEvent::Initialized { .. }) => {}
                Ok(other) => return Err(undo(format!("unexpected answer {other:?}"))),
                Err(e) => return Err(undo(e)),
            }
            created = Some((name.clone(), g.clone(), dir));
            (name, g)
        }
    };
    let added = op(
        svc,
        &grp,
        &group,
        WorkerOp::AddKey {
            password: pc_password.clone(),
            hostname: req.hostname.clone(),
        },
        &svc.launch,
        None,
    )
    .await;
    let pc_key = match added {
        Ok(WorkerEvent::KeyAdded { id }) => id,
        other => {
            if let Some((_, _, dir)) = &created {
                let _ = remove_repo_dir(dir, svc.dry);
            }
            return Err(Error::new(
                StatusCode::BAD_GATEWAY,
                error_codes::BACKEND_UNAVAILABLE,
                format!("adding the PC's key: {other:?}"),
            ));
        }
    };
    let ds = PcDeviceSettings {
        description: p.description.clone(),
        ..Default::default()
    };
    let dev = Dev {
        hostname: req.hostname.clone(),
        os: req.os.chars().take(120).collect(),
        arch: req.arch.chars().take(32).collect(),
        agent_version: req.agent_version.chars().take(32).collect(),
        machine_id: req.machine_id.chars().take(64).collect(),
        created_ms: now_ms(),
        last_seen_ms: Some(now_ms()),
        group: group.clone(),
        token_sha: super::sha_hex(token.as_bytes()),
        pc_key,
        settings: ds,
        rev: 1,
        ..Default::default()
    };
    let plan = AgentPlan {
        rev: dev.rev,
        plan: dev.settings.plan.clone(),
        command: None,
    };
    svc.edit(|r| {
        if let Some((n, g, _)) = &created {
            r.groups.insert(n.clone(), g.clone());
        }
        r.devices.insert(device.clone(), dev);
        Ok(())
    })?;
    Ok(PairResponse {
        device,
        group,
        token,
        repo_password: pc_password,
        nas_name: svc.hostname.clone(),
        plan,
    })
}

// ------------------------------------------------------------------ revocation, groups, kit

pub async fn revoke(svc: &Arc<PcAgent>, device: &str, d: &Dev, ctx: &TaskCtx) -> Result<(), String> {
    let g = svc.group(&d.group).map_err(|e| e.2)?;
    if !d.pc_key.is_empty() {
        ctx.log(&format!("removing the key file of {device}"));
        match op(svc, &g, &d.group, WorkerOp::RemoveKey { id: d.pc_key.clone() }, &svc.launch, Some(ctx)).await {
            Ok(_) => {}
            // already gone: nothing to do
            Err(e) if e.contains("not found") || e.contains("No such file") => {}
            Err(e) => return Err(format!("the token is revoked, but the key file stays: {e}")),
        }
    }
    let _ = svc.edit(|r| {
        if let Some(x) = r.devices.get_mut(device) {
            x.pc_key.clear();
        }
        Ok(())
    });
    if d.alert != Alert::None || d.failed_alerted {
        svc.notifier.emit(resolved(
            NotifyCategory::Backup,
            "pcbackup",
            format!("pcbackup:agent:{device}"),
            format!("{device} revoked"),
        ));
    }
    ctx.log(&format!("{device} revoked; its versions stay in group {}", d.group));
    Ok(())
}

pub async fn delete_group(svc: &Arc<PcAgent>, group: &str, g: &Grp, ctx: &TaskCtx) -> Result<(), String> {
    {
        let live = svc.live.lock().unwrap_or_else(|p| p.into_inner());
        if live.get(group).is_some_and(|l| l.maintaining) {
            return Err("the group is being maintained".into());
        }
    }
    ctx.log(&format!("deleting {}", g.repo));
    let (p, dry) = (PathBuf::from(&g.repo), svc.dry);
    tokio::task::spawn_blocking(move || remove_repo_dir(&p, dry))
        .await
        .map_err(|e| e.to_string())??;
    svc.edit(|r| {
        r.groups.remove(group);
        let gone: Vec<String> = r.devices.iter().filter(|(_, d)| d.group == group).map(|(n, _)| n.clone()).collect();
        for n in gone {
            r.devices.remove(&n);
        }
        Ok(())
    })
    .map_err(|e| e.2)?;
    svc.live.lock().unwrap_or_else(|p| p.into_inner()).remove(group);
    Ok(())
}

/// A new recovery key file for the group (the previous one removed); the password is only
/// in this answer.
pub async fn recovery_kit(svc: &Arc<PcAgent>, group: &str) -> Res<RecoveryKit> {
    let g = svc.group(group)?;
    let password = rynas_backup::repo::generate_password().map_err(Error::internal)?;
    let gw = |e: String| Error::new(StatusCode::BAD_GATEWAY, error_codes::BACKEND_UNAVAILABLE, e);
    let id = match op(
        svc,
        &g,
        group,
        WorkerOp::AddKey {
            password: password.clone(),
            hostname: "recovery-kit".into(),
        },
        &svc.launch,
        None,
    )
    .await
    .map_err(gw)?
    {
        WorkerEvent::KeyAdded { id } => id,
        other => return Err(gw(format!("unexpected answer {other:?}"))),
    };
    if let Some(old) = &g.recovery_key
        && let Err(e) = op(svc, &g, group, WorkerOp::RemoveKey { id: old.clone() }, &svc.launch, None).await
    {
        tracing::warn!("pc backup: the previous recovery key of {group} stays: {e}");
    }
    let now = now_ms();
    let reg = svc.edit(|r| {
        if let Some(x) = r.groups.get_mut(group) {
            x.recovery_key = Some(id.clone());
            x.recovery_kit_ms = Some(now);
        }
        Ok(r.clone())
    })?;
    Ok(RecoveryKit {
        group: group.to_string(),
        description: g.settings.description.clone(),
        devices: reg.devices.iter().filter(|(_, d)| d.group == group).map(|(n, _)| n.clone()).collect(),
        repository: g.repo.clone(),
        password,
        created_ms: now,
        fingerprint: svc.fingerprint(),
    })
}

// ------------------------------------------------------------------ reports

fn trunc(s: &mut String, n: usize) {
    if s.len() > n {
        let mut cut = n;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
}

/// The end of a run, from the agent.
pub fn report(svc: &PcAgent, device: &str, r: AgentReport) -> Res<()> {
    let mut run = r.run;
    trunc(&mut run.run_id, 64);
    if let Some(e) = &mut run.error {
        trunc(e, 2000);
    }
    run.changed_during.truncate(MAX_CHANGED);
    for c in &mut run.changed_during {
        trunc(c, 1024);
    }
    run.captures.truncate(64);
    run.restore = false;
    if run.snapshot.as_ref().is_some_and(|s| s.len() > 64 || !s.bytes().all(|b| b.is_ascii_hexdigit())) {
        run.snapshot = None;
    }
    let manifest = r.manifest.and_then(|m| {
        let b = serde_json::to_vec(&m).ok()?;
        (b.len() <= MAX_MANIFEST).then_some((m, b))
    });
    if let Some((_, b)) = &manifest {
        let p = svc.manifest_path(device);
        if let Some(d) = p.parent() {
            let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(d);
        }
        crate::tls::write_atomic(&p, b, 0o600, None).map_err(Error::internal)?;
    }
    let ok = run.ok;
    let err = run.error.clone();
    let alerted = svc.edit(|reg| {
        let d = reg.devices.get_mut(device).ok_or_else(|| Error::internal("device vanished"))?;
        if ok {
            d.last_ok_ms = Some(run.ended_ms.max(run.started_ms));
            d.versions += 1;
        }
        if !r.agent_version.is_empty() {
            d.agent_version = r.agent_version.chars().take(32).collect();
        }
        if let Some((m, _)) = &manifest {
            d.manifest_ms = Some(m.written_at_ms);
            d.manifest_reasons = m.ready_reasons();
        }
        d.command = None;
        d.rev += 1;
        d.history.push_back(run);
        while d.history.len() > HISTORY {
            d.history.pop_front();
        }
        let was = d.failed_alerted;
        d.failed_alerted = !ok;
        Ok(was)
    })?;
    svc.clear_progress(device);
    svc.changed.notify_waiters();
    let key = format!("pcbackup:agent-failed:{device}");
    if !ok && !alerted {
        let mut ev = firing(
            NotifyCategory::Backup,
            NotifySeverity::Warning,
            "pcbackup",
            key,
            format!("The backup of {device} failed"),
        );
        ev.body = err.unwrap_or_default();
        svc.notifier.emit(field(code(ev, "pcbackup.agent_failed"), "device", device));
    } else if ok && alerted {
        let ev = resolved(NotifyCategory::Backup, "pcbackup", key, format!("{device} backs up again"));
        svc.notifier.emit(field(code(ev, "pcbackup.agent_resumed"), "device", device));
    }
    Ok(())
}

// ------------------------------------------------------------------ maintenance

/// Clears the maintenance flag of a group, however the task ends.
struct Maintaining {
    svc: Arc<PcAgent>,
    group: String,
}

impl Drop for Maintaining {
    fn drop(&mut self) {
        let mut live = self.svc.live.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(l) = live.get_mut(&self.group) {
            l.maintaining = false;
        }
    }
}

/// Takes the group for a maintenance: no upload running or in the last [`QUIET`], no PC
/// reporting progress.
fn take(svc: &Arc<PcAgent>, group: &str) -> Result<Maintaining, String> {
    if svc.group_running(group) {
        return Err("a PC of the group is backing up".into());
    }
    let mut live = svc.live.lock().unwrap_or_else(|p| p.into_inner());
    let l: &mut Live = live.entry(group.to_string()).or_default();
    if l.maintaining {
        return Err("a maintenance of the group is running".into());
    }
    if l.writes > 0 || l.last_write.is_some_and(|t| t.elapsed() < QUIET) {
        return Err("a PC of the group uploaded in the last ten minutes".into());
    }
    l.maintaining = true;
    Ok(Maintaining {
        svc: svc.clone(),
        group: group.to_string(),
    })
}

/// Starts the maintenance task of a group (`scheduled`: from the daily timer).
pub fn start(svc: &Arc<PcAgent>, group: &str, who: &str, scheduled: bool) -> Res<String> {
    svc.group(group)?;
    let guard = take(svc, group).map_err(|e| Error::new(StatusCode::CONFLICT, error_codes::BUSY, e))?;
    let (s2, g2) = (svc.clone(), group.to_string());
    let id = svc.ops.tasks.spawn("pcbackup.maintenance", who, move |ctx| async move {
        let _guard = guard;
        let r = maintain(&s2, &g2, &ctx, scheduled).await;
        let now = now_ms();
        let _ = s2.edit(|reg| {
            if let Some(g) = reg.groups.get_mut(&g2) {
                g.maintenance.last_ms = Some(now);
                g.maintenance.ok = r.is_ok();
                g.maintenance.error = r.as_ref().err().cloned();
            }
            Ok(())
        });
        r
    })?;
    Ok(id)
}

async fn maintain(svc: &Arc<PcAgent>, group: &str, ctx: &TaskCtx, scheduled: bool) -> Result<(), String> {
    let _ = scheduled;
    let g = svc.group(group).map_err(|e| e.2)?;
    let reg = svc.snapshot();
    let budget = memory_budget(reg.settings.maintenance_memory_pct);
    let launch = maintenance_launch(&svc.launch, budget);
    let _permit = match svc.workers.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            ctx.log("waiting for another backup operation to finish");
            tokio::select! {
                p = svc.workers.clone().acquire_owned() => p.map_err(|e| e.to_string())?,
                _ = ctx.token().cancelled() => return Err("cancelled".into()),
            }
        }
    };
    ctx.log(&format!("memory budget {} MiB", budget >> 20));
    // 1. the memory it needs, from the size of the index files
    let blobs = {
        let p = PathBuf::from(&g.repo);
        tokio::task::spawn_blocking(move || blobs_estimate(&p)).await.unwrap_or(0)
    };
    let estimate = prune_estimate(blobs);
    ctx.log(&format!("about {blobs} blobs in the index: prune and check need about {} MiB", estimate >> 20));
    ctx.progress(10);
    // 2. retention, per PC (its tag), with its minimum age; revoked PCs keep their versions
    let mut removed = 0u32;
    for (name, d) in reg.devices.iter().filter(|(_, d)| d.group == group && !d.revoked) {
        let ev = op(
            svc,
            &g,
            group,
            WorkerOp::Forget {
                scope: super::scope(name),
                retention: d.settings.retention,
                dry_run: false,
                min_age_days: d.settings.min_age_days,
            },
            &launch,
            Some(ctx),
        )
        .await?;
        if let WorkerEvent::Forgot { keep, remove } = ev {
            ctx.log(&format!("{name}: {} versions kept, {} forgotten", keep.len(), remove.len()));
            removed += remove.len() as u32;
            let n = keep.len() as u32;
            let _ = svc.edit(|r| {
                if let Some(x) = r.devices.get_mut(name) {
                    x.versions = n;
                }
                Ok(())
            });
        }
    }
    ctx.progress(30);
    // 3. prune and check, only inside the budget
    let mut freed = 0;
    let postponed = if estimate > budget {
        let why = format!(
            "prune and check postponed: they need about {} MiB, over the budget of {} MiB",
            estimate >> 20,
            budget >> 20
        );
        ctx.log(&why);
        Some(why)
    } else {
        if let WorkerEvent::Pruned { removed_packs, freed_bytes } = op(svc, &g, group, WorkerOp::Prune, &launch, Some(ctx)).await? {
            ctx.log(&format!("prune: {removed_packs} packs marked or removed, {freed_bytes} bytes"));
            freed = freed_bytes;
        }
        ctx.progress(60);
        let slice = g.check_slice.clamp(1, 12);
        let ev = op(
            svc,
            &g,
            group,
            WorkerOp::Check {
                read_percent: 0,
                slice: Some((slice, 12)),
            },
            &launch,
            Some(ctx),
        )
        .await?;
        if let WorkerEvent::Checked { errors, warnings } = ev {
            for w in &warnings {
                ctx.log(&format!("check warning: {w}"));
            }
            if !errors.is_empty() {
                return Err(format!("check found {} errors: {}", errors.len(), errors.join("; ")));
            }
            ctx.log(&format!("check: structure and data twelfth {slice} of 12 ok"));
        }
        None
    };
    ctx.progress(90);
    // 4. sizes again
    let (p, n) = (PathBuf::from(&g.repo), group.to_string());
    let used = tokio::task::spawn_blocking(move || dir_size(&p)).await.unwrap_or(0);
    if let Some(l) = svc.live.lock().unwrap_or_else(|p| p.into_inner()).get_mut(&n) {
        l.used = Some(used);
    }
    let blobs_after = {
        let p = PathBuf::from(&g.repo);
        tokio::task::spawn_blocking(move || blobs_estimate(&p)).await.unwrap_or(blobs)
    };
    svc.edit(|r| {
        if let Some(x) = r.groups.get_mut(group) {
            x.maintenance.freed_bytes = freed;
            x.maintenance.removed_versions = removed;
            x.maintenance.blobs = blobs_after;
            x.maintenance.estimate_bytes = prune_estimate(blobs_after);
            x.maintenance.postponed = postponed.clone();
            if postponed.is_none() {
                x.check_slice = x.check_slice.clamp(1, 12) % 12 + 1;
            }
        }
        Ok(())
    })
    .map_err(|e| e.2)?;
    Ok(())
}

// ------------------------------------------------------------------ the daily round

fn local_day_hour() -> (i64, i8) {
    let z = jiff::Timestamp::now().to_zoned(rynas_power::system_tz());
    let days = z.date().since(jiff::civil::date(1970, 1, 1)).map(|s| s.get_days() as i64).unwrap_or(0);
    (days, z.hour())
}

/// Late and critical notifications of one device (state changes only).
fn alerts(svc: &PcAgent, name: &str, d: &Dev) {
    if d.revoked {
        return;
    }
    let want = match super::state_of(d, false, now_ms()) {
        PcState::Late => Alert::Late,
        PcState::Critical => Alert::Critical,
        _ => Alert::None,
    };
    if want == d.alert {
        return;
    }
    let key = format!("pcbackup:agent:{name}");
    let ev = match want {
        Alert::None => field(
            code(resolved(NotifyCategory::Backup, "pcbackup", key, format!("Backups from {name} arrive again")), "pcbackup.agent_resumed"),
            "device",
            name,
        ),
        Alert::Late | Alert::Critical => {
            let (sev, c, h) = if want == Alert::Late {
                (NotifySeverity::Warning, "pcbackup.agent_late", d.settings.alert_hours)
            } else {
                (NotifySeverity::Critical, "pcbackup.agent_critical", d.settings.critical_hours)
            };
            let mut ev = firing(NotifyCategory::Backup, sev, "pcbackup", key, format!("No backup from {name} for {h} hours"));
            ev.body = match d.last_ok_ms {
                Some(_) => format!("The last good version of {name} is older than {h} hours."),
                None => format!("{name} has no version yet."),
            };
            field(field(code(ev, c), "device", name), "hours", h)
        }
    };
    if svc
        .edit(|r| {
            if let Some(x) = r.devices.get_mut(name) {
                x.alert = want;
            }
            Ok(())
        })
        .is_ok()
    {
        svc.notifier.emit(ev);
    }
}

/// Every five minutes: notifications, and each group's maintenance in its hour (retried
/// within the hour while a PC is busy; the reason stays on the group when it cannot run).
pub fn spawn_watch(svc: Arc<PcAgent>) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(90)).await;
        loop {
            let reg = svc.snapshot();
            for (n, d) in &reg.devices {
                alerts(&svc, n, d);
            }
            let (day, hour) = local_day_hour();
            if i16::from(hour) == i16::from(reg.settings.maintenance_hour) {
                for (n, g) in &reg.groups {
                    if g.maintained_day == Some(day) || !reg.devices.values().any(|d| d.group == *n) {
                        continue;
                    }
                    match start(&svc, n, "rynas", true) {
                        Ok(_) => {
                            let _ = svc.edit(|r| {
                                if let Some(x) = r.groups.get_mut(n) {
                                    x.maintained_day = Some(day);
                                }
                                Ok(())
                            });
                        }
                        Err(e) => {
                            let why = format!("maintenance postponed: {}", e.2);
                            let _ = svc.edit(|r| {
                                if let Some(x) = r.groups.get_mut(n) {
                                    x.maintenance.postponed = Some(why.clone());
                                }
                                Ok(())
                            });
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(300)).await;
        }
    });
}

// ------------------------------------------------------------------ downloads

fn content_disposition(name: &str) -> String {
    let ascii: String = name
        .chars()
        .map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' })
        .collect();
    let mut enc = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            enc.push(b as char);
        } else {
            enc.push_str(&format!("%{b:02X}"));
        }
    }
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{enc}")
}

/// A file or a zip of one version, streamed from the worker's stdout (no size limit, never
/// whole in memory; the events of the worker come on its stderr).
pub async fn stream(svc: &Arc<PcAgent>, device: &str, version: String, paths: Vec<String>, zip: bool, name: &str) -> Res<Response> {
    let d = svc.device(device)?;
    let g = svc.group(&d.group)?;
    let permit = svc.streams.clone().try_acquire_owned().map_err(|_| {
        Error::new(StatusCode::TOO_MANY_REQUESTS, error_codes::RATE_LIMITED, "two downloads are running: wait for one to finish".into())
    })?;
    let req = svc.request(
        &g,
        WorkerOp::Stream {
            snapshot: version,
            paths,
            zip,
            scope: Some(super::scope(device)),
        },
    )?;
    let (argv, _) = rynas_backup::runner::command(&svc.launch, &format!("pc-{device}"), &req, None).map_err(Error::internal)?;
    let mut child = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Error::internal(format!("cannot start the worker: {e}")))?;
    let body = serde_json::to_vec(&req).map_err(Error::internal)?;
    let mut stdin = child.stdin.take().ok_or_else(|| Error::internal("no stdin"))?;
    stdin.write_all(&body).await.map_err(Error::internal)?;
    drop(stdin);
    let mut stdout = child.stdout.take().ok_or_else(|| Error::internal("no stdout"))?;
    let mut stderr = child.stderr.take().ok_or_else(|| Error::internal("no stderr"))?;
    // the first bytes decide: data → 200 and the rest streams; nothing → the worker's error
    let mut first = vec![0u8; 64 << 10];
    let n = match tokio::time::timeout(READ_TIMEOUT, stdout.read(&mut first)).await {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => return Err(Error::internal(e)),
        Err(_) => return Err(Error::new(StatusCode::GATEWAY_TIMEOUT, error_codes::TIMEOUT, "the worker did not answer".into())),
    };
    if n == 0 {
        let mut err = String::new();
        let _ = (&mut stderr).take(64 << 10).read_to_string(&mut err).await;
        let _ = child.wait().await;
        let msg = err
            .lines()
            .rev()
            .find_map(|l| match serde_json::from_str::<WorkerEvent>(l) {
                Ok(WorkerEvent::Error { message }) => Some(message),
                _ => None,
            });
        // an empty file is a whole answer
        if msg.is_none() {
            return Ok(attachment(Body::empty(), name, zip));
        }
        let msg = msg.unwrap_or_default();
        let status = if msg.contains("not found") || msg.contains("is not a file") { StatusCode::NOT_FOUND } else { StatusCode::BAD_GATEWAY };
        let code = if status == StatusCode::NOT_FOUND { error_codes::NOT_FOUND } else { error_codes::BACKEND_UNAVAILABLE };
        return Err(Error::new(status, code, msg));
    }
    first.truncate(n);
    let head = futures_util::stream::once(async move { Ok::<_, std::io::Error>(axum::body::Bytes::from(first)) });
    let rest = futures_util::stream::unfold((stdout, child, stderr, permit), |(mut out, child, err, permit)| async move {
        let mut buf = vec![0u8; 256 << 10];
        match out.read(&mut buf).await {
            Ok(0) => None,
            Ok(k) => {
                buf.truncate(k);
                Some((Ok(axum::body::Bytes::from(buf)), (out, child, err, permit)))
            }
            Err(e) => Some((Err(e), (out, child, err, permit))),
        }
    });
    use futures_util::StreamExt;
    Ok(attachment(Body::from_stream(head.chain(rest)), name, zip))
}

fn attachment(body: Body, name: &str, zip: bool) -> Response {
    let mut r = Response::new(body);
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(if zip { "application/zip" } else { "application/octet-stream" }),
    );
    if let Ok(v) = HeaderValue::from_str(&content_disposition(name)) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_and_estimate() {
        assert!(memory_budget(40) >= 256 << 20);
        assert_eq!(prune_estimate(0), PRUNE_BASE);
        assert_eq!(prune_estimate(1_000_000), PRUNE_BASE + 250_000_000);
        let l = maintenance_launch(
            &Launch::Unit {
                exe: "/w".into(),
                memory_high: "1G".into(),
                memory_max: "2G".into(),
            },
            1 << 30,
        );
        match l {
            Launch::Unit { memory_high, memory_max, .. } => {
                assert_eq!((memory_high.as_str(), memory_max.as_str()), ("768M", "1024M"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn unique_names() {
        assert_eq!(unique("pc", |n| n == "pc"), "pc-2");
        assert_eq!(unique("pc", |_| false), "pc");
        let long = "a".repeat(32);
        let u = unique(&long, |n| n == long);
        assert!(u.len() <= 32 && u.ends_with("-2"), "{u}");
    }

    #[test]
    fn dispositions() {
        assert_eq!(
            content_disposition("a b\"è.txt"),
            "attachment; filename=\"a b__.txt\"; filename*=UTF-8''a%20b%22%C3%A8.txt"
        );
    }
}
