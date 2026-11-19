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

/// The group's bytes on disk (counted once, then kept up to date by the uploads). Walks the
/// repository the first time: call it from a blocking thread.
pub fn used_bytes(svc: &PcAgent, group: &str, repo: &Path) -> u64 {
    if let Some(u) = svc.live.lock().unwrap_or_else(|p| p.into_inner()).get(group).and_then(|l| l.used) {
        return u;
    }
    let n = dir_size(repo);
    *svc.live
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(group.to_string())
        .or_default()
        .used
        .get_or_insert(n)
}

/// [`used_bytes`] for the async paths: a first count runs on a blocking thread.
pub async fn ensure_used(svc: &Arc<PcAgent>, group: &str, repo: &Path) -> u64 {
    if let Some(u) = svc.live.lock().unwrap_or_else(|p| p.into_inner()).get(group).and_then(|l| l.used) {
        return u;
    }
    let (s2, g2, r2) = (svc.clone(), group.to_string(), repo.to_path_buf());
    tokio::task::spawn_blocking(move || used_bytes(&s2, &g2, &r2)).await.unwrap_or(0)
}

/// Counts every group not counted yet (the console's first look, rynasd's start).
pub fn count_used(svc: &PcAgent, reg: &Registry) {
    for (n, g) in &reg.groups {
        used_bytes(svc, n, Path::new(&g.repo));
    }
}

/// A new file of `size` bytes from `device`: counted in memory, written to the registry
/// within [`super::FLUSH_EVERY`] (never a registry write per uploaded file).
pub fn add_used(svc: &PcAgent, group: &str, device: &str, size: u64) {
    {
        let mut live = svc.live.lock().unwrap_or_else(|p| p.into_inner());
        let l = live.entry(group.to_string()).or_default();
        if let Some(u) = &mut l.used {
            *u += size;
        }
    }
    svc.touch(|r| {
        if let Some(d) = r.devices.get_mut(device) {
            d.uploaded_bytes += size;
        }
    });
}

// ------------------------------------------------------------------ memory of reads

/// Memory reserved against the budget, given back however the work ends.
pub struct MemSlot {
    svc: Arc<PcAgent>,
    bytes: u64,
}

impl Drop for MemSlot {
    fn drop(&mut self) {
        let mut m = self.svc.mem_used.lock().unwrap_or_else(|p| p.into_inner());
        *m = m.saturating_sub(self.bytes);
    }
}

/// Reserves `need` bytes of `budget` when they fit with what runs already.
pub fn try_reserve(svc: &Arc<PcAgent>, need: u64, budget: u64) -> Option<MemSlot> {
    let mut m = svc.mem_used.lock().unwrap_or_else(|p| p.into_inner());
    if m.saturating_add(need) > budget {
        return None;
    }
    *m += need;
    Some(MemSlot { svc: svc.clone(), bytes: need })
}

/// A read of a group's repository (versions, browse, a download or zip) loads its whole
/// index: admitted only when its estimate fits in the maintenance budget with what runs
/// already, never while the group is maintained; it runs in a unit limited to the budget.
pub async fn admit_read(svc: &Arc<PcAgent>, group: &str, g: &Grp) -> Res<(MemSlot, Launch)> {
    let busy = |msg: String| Error::new(StatusCode::LOCKED, error_codes::BUSY, msg);
    if svc.live.lock().unwrap_or_else(|p| p.into_inner()).get(group).is_some_and(|l| l.maintaining) {
        return Err(busy("the NAS is maintaining this group's repository: browsing and downloads work again when it ends".into()));
    }
    let budget = memory_budget(svc.snapshot().settings.maintenance_memory_pct);
    let p = PathBuf::from(&g.repo);
    let blobs = tokio::task::spawn_blocking(move || blobs_estimate(&p)).await.unwrap_or(0);
    let need = prune_estimate(blobs);
    if need > budget {
        return Err(busy(format!(
            "reading this repository needs about {} MiB, over the memory budget of {} MiB: raise the maintenance memory share",
            need >> 20,
            budget >> 20
        )));
    }
    let slot = try_reserve(svc, need, budget).ok_or_else(|| {
        busy(format!(
            "reading this repository needs about {} MiB of the {} MiB budget, which maintenances and other downloads use now: try again in a few minutes",
            need >> 20,
            budget >> 20
        ))
    })?;
    Ok((slot, maintenance_launch(&svc.launch, budget)))
}

// ------------------------------------------------------------------ restore sessions

/// A key of its own for a restore session; its password is the only one the agent gets.
pub async fn add_session_key(svc: &Arc<PcAgent>, group: &str, g: &Grp, device: &str) -> Res<(String, String)> {
    let password = rynas_backup::repo::generate_password().map_err(Error::internal)?;
    let gw = |e: String| Error::new(StatusCode::BAD_GATEWAY, error_codes::BACKEND_UNAVAILABLE, format!("adding the restore key: {e}"));
    match op(
        svc,
        g,
        group,
        WorkerOp::AddKey {
            password: password.clone(),
            hostname: format!("restore-{device}"),
        },
        &svc.launch,
        None,
    )
    .await
    .map_err(gw)?
    {
        WorkerEvent::KeyAdded { id } => Ok((id, password)),
        other => Err(gw(format!("unexpected answer {other:?}"))),
    }
}

/// Ends the sessions that expired or reported: their key files go, then the records. A key
/// that cannot be removed now is tried again on the next round.
pub async fn end_sessions(svc: &Arc<PcAgent>) {
    let now = now_ms();
    let ended: Vec<super::Session> = svc.snapshot().sessions.into_iter().filter(|s| s.expires_ms <= now).collect();
    for s in ended {
        if !s.key_id.is_empty()
            && let Ok(g) = svc.group(&s.key_group)
        {
            match op(svc, &g, &s.key_group, WorkerOp::RemoveKey { id: s.key_id.clone() }, &svc.launch, None).await {
                Ok(_) => {}
                Err(e) if e.contains("not found") || e.contains("No such file") => {}
                Err(e) => {
                    tracing::warn!("pc backup: the restore key of {} stays for now: {e}", s.key_group);
                    continue;
                }
            }
        }
        let _ = svc.edit(|r| {
            r.sessions.retain(|x| x.token_sha != s.token_sha);
            Ok(())
        });
    }
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

fn group_gone(group: &str) -> Error {
    Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, format!("group {group} was deleted meanwhile"))
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

/// A pairing for the btrfs method: the device takes the folder's name, gets no repository
/// and no key file (`group` stays empty), and sends `btrfs send` streams into the folder.
fn pair_btrfs(svc: &Arc<PcAgent>, p: &Pairing, target: &str, req: &PairRequest) -> Res<PairResponse> {
    let Some(folder) = svc.btrfs_side()?.agent_folder_id(target) else {
        return Err(Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, format!("the folder {target} is gone")));
    };
    let token = super::new_token()?;
    let ds = PcDeviceSettings {
        description: p.description.clone(),
        method: PcMethod::Btrfs,
        ..Default::default()
    };
    let dev = Dev {
        hostname: req.hostname.clone(),
        os: req.os.chars().take(120).collect(),
        arch: req.arch.chars().take(32).collect(),
        agent_version: req.agent_version.chars().take(32).collect(),
        machine_id: req.machine_id.chars().take(64).collect(),
        root_btrfs: req.root_btrfs,
        created_ms: now_ms(),
        last_seen_ms: Some(now_ms()),
        online_since_ms: Some(now_ms()),
        group: String::new(),
        token_sha: super::sha_hex(token.as_bytes()),
        settings: ds,
        rev: 1,
        btrfs_folder: Some(folder),
        ..Default::default()
    };
    let plan = svc.edit(|r| {
        match r.devices.get_mut(target) {
            // paired again: the same PC (or its new system) takes over its folder, history and
            // settings with a new token; the old token stops here
            Some(old) if old.group.is_empty() => {
                old.hostname = dev.hostname;
                old.os = dev.os;
                old.arch = dev.arch;
                old.agent_version = dev.agent_version;
                old.machine_id = dev.machine_id;
                old.root_btrfs = dev.root_btrfs;
                old.last_seen_ms = dev.last_seen_ms;
                old.online_since_ms = dev.online_since_ms;
                old.token_sha = dev.token_sha;
                // bound to the folder the pairing was made for (it may be a new one)
                old.btrfs_folder = dev.btrfs_folder;
                old.revoked = false;
                old.command = None;
                old.command_at_ms = None;
                old.rev += 1;
                Ok(super::plan_of(old, svc.paused_all()))
            }
            Some(_) => Err(Error::new(StatusCode::CONFLICT, error_codes::ALREADY_EXISTS, format!("a PC named {target} is already paired"))),
            None => {
                let plan = super::plan_of(&dev, svc.paused_all());
                r.devices.insert(target.to_string(), dev);
                Ok(plan)
            }
        }
    })?;
    Ok(PairResponse {
        device: target.to_string(),
        group: String::new(),
        token,
        repo_password: String::new(),
        nas_name: svc.hostname(),
        plan,
        trust: None,
    })
}

/// A pairing code was accepted: the device (and its group, unless it joins one), its key
/// file and token.
pub async fn pair(svc: &Arc<PcAgent>, p: &Pairing, settings: &PcAgentSettings, req: &PairRequest) -> Res<PairResponse> {
    if let Some(t) = &p.btrfs_target {
        return pair_btrfs(svc, p, t, req);
    }
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
                chunk_mib: settings.chunking.mib(mem_total()),
                ..Default::default()
            };
            let undo = |e: String| {
                let _ = remove_repo_dir(&dir, svc.dry);
                Error::new(StatusCode::BAD_GATEWAY, error_codes::BACKEND_UNAVAILABLE, format!("creating the repository: {e}"))
            };
            match op(svc, &g, &name, WorkerOp::InitPc { chunk_mib: g.chunk_mib }, &svc.launch, None).await {
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
    let dev_key = pc_key.clone();
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
        root_btrfs: req.root_btrfs,
        created_ms: now_ms(),
        last_seen_ms: Some(now_ms()),
        online_since_ms: Some(now_ms()),
        group: group.clone(),
        token_sha: super::sha_hex(token.as_bytes()),
        pc_key,
        settings: ds,
        rev: 1,
        ..Default::default()
    };
    let plan = super::plan_of(&dev, svc.paused_all());
    // the name is picked again against the registry as it is now: another pairing may have
    // taken it meanwhile (two PCs with the same host name at once)
    let stored = svc.edit(|r| {
        if let Some((n, g, _)) = &created {
            r.groups.insert(n.clone(), g.clone());
        } else if !r.groups.contains_key(&group) {
            return Err(group_gone(&group));
        }
        let device = unique(&device, |n| r.devices.contains_key(n));
        r.devices.insert(device.clone(), dev);
        Ok(device)
    });
    let device = match stored {
        Ok(d) => d,
        Err(e) => {
            if let Some((_, _, dir)) = &created {
                let _ = remove_repo_dir(dir, svc.dry);
            } else if let Ok(g) = svc.group(&group) {
                let _ = op(svc, &g, &group, WorkerOp::RemoveKey { id: dev_key }, &svc.launch, None).await;
            }
            return Err(e);
        }
    };
    Ok(PairResponse {
        device,
        group,
        token,
        repo_password: pc_password,
        nas_name: svc.hostname(),
        plan,
        trust: None,
    })
}

// ------------------------------------------------------------------ revocation, groups, kit

pub async fn revoke(svc: &Arc<PcAgent>, device: &str, d: &Dev, ctx: &TaskCtx) -> Result<(), String> {
    // a PC of the btrfs method only: no repository, no key file; its snapshots stay in its
    // folder of the btrfs method
    if d.group.is_empty() {
        ctx.log(&format!("{device} revoked; its snapshots stay in its folder"));
        return Ok(());
    }
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
        let ev = resolved(NotifyCategory::Backup, "pcbackup", format!("pcbackup:agent:{device}"), format!("{device} revoked"));
        svc.notifier.emit(field(code(ev, "pcbackup.revoked"), "device", device));
    }
    ctx.log(&format!("{device} revoked; its versions stay in group {}", d.group));
    Ok(())
}

/// Deletes a group the caller took with [`take`] (so nothing else runs on it).
pub async fn delete_group(svc: &Arc<PcAgent>, group: &str, g: &Grp, ctx: &TaskCtx) -> Result<(), String> {
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

/// Bounds the strings of a btrfs step from an agent (names, code, error text).
pub fn bound_step(b: &mut BtrfsStep) {
    trunc(&mut b.subvolume, 128);
    trunc(&mut b.snapshot, 160);
    if let Some(p) = &mut b.parent {
        trunc(p, 160);
    }
    if let Some(c) = &mut b.error_code {
        trunc(c, 64);
    }
    if let Some(e) = &mut b.error {
        trunc(e, 500);
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
    for c in &mut run.captures {
        if let Some(b) = &mut c.btrfs {
            bound_step(b);
        }
    }
    run.restore = false;
    if run.snapshot.as_ref().is_some_and(|s| s.len() > 64 || !s.bytes().all(|b| b.is_ascii_hexdigit())) {
        run.snapshot = None;
    }
    // a scheduled start the agent skipped (S6): one row in the scheduler's history, nothing
    // else changes (not a run: no version, no alert, the command stays)
    if let Some(reason) = run.skipped {
        svc.device(device)?;
        record_skip(svc, device, &run, reason);
        return Ok(());
    }
    let manifest = r.manifest.and_then(|mut m| {
        // a PV an older agent named by a udev alias (`/dev/gpt-auto-root`): its partition,
        // when certain (the readiness and the restore read the same names)
        m.normalize_lvm_aliases();
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
    let run_row = run.clone();
    let alerted = svc.edit(|reg| {
        let d = reg.devices.get_mut(device).ok_or_else(|| Error::internal("device vanished"))?;
        if ok {
            d.last_ok_ms = Some(run.ended_ms.max(run.started_ms));
            d.last_ok_start_ms = Some(run.started_ms);
            d.versions += 1;
        }
        if !r.agent_version.is_empty() {
            d.agent_version = r.agent_version.chars().take(32).collect();
        }
        if r.root_btrfs.is_some() {
            d.root_btrfs = r.root_btrfs;
        }
        if let Some((m, _)) = &manifest {
            d.manifest_ms = Some(m.written_at_ms);
            d.manifest_reasons = m.ready_reasons();
            // the lines behind "manual layout": which group or volume, and why
            d.manifest_details = if m.md.is_empty() { Vec::new() } else { vec!["md RAID arrays are not made again by the restore".to_string()] };
            d.manifest_details.extend(m.lvm_unsupported());
            d.manifest_details.truncate(20);
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
    record_run(svc, device, &run_row);
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

/// A run of a PC's backup in the scheduler's history (task `pc:<device>`, S6): the unified
/// history of the Schedule page and the lengths of the overview. Reported twice (the agent
/// retries a report it saw fail), it is recorded once.
fn record_run(svc: &PcAgent, device: &str, run: &PcRun) {
    let Some(link) = svc.scheduler.get() else { return };
    let row = rynas_scheduler::history::NewRun {
        task_id: super::task_id_of(device),
        invocation_id: format!("{}:{}", super::task_id_of(device), run.run_id),
        start_ms: Some(run.started_ms),
        end_ms: run.ended_ms.max(run.started_ms),
        result: if run.ok { "success".into() } else { "failed".into() },
        trigger: run.trigger,
        detail: run.error.clone(),
        ..Default::default()
    };
    if let Err(e) = link.history.record_once(&row) {
        tracing::warn!("PC backup {device}: scheduler history: {e}");
    }
}

/// A scheduled start the agent skipped, in the scheduler's history: paused (by the PC's own
/// pause or Pause all on the NAS), or due outside the allowed window (it waits for it).
fn record_skip(svc: &PcAgent, device: &str, run: &PcRun, reason: rynas_api_types::scheduler::SkipReason) {
    use rynas_api_types::scheduler::{RunTrigger, SkipReason};
    let Some(link) = svc.scheduler.get() else { return };
    let at = if run.started_ms > 0 { run.started_ms } else { now_ms() };
    let (reason, detail) = match reason {
        SkipReason::Paused | SkipReason::PausedAll if svc.paused_all() => (SkipReason::PausedAll, None),
        SkipReason::Paused | SkipReason::PausedAll => (SkipReason::Paused, None),
        // the interface names it (the row's task says it is a PC's backup)
        SkipReason::Window => (SkipReason::Window, None),
        // nothing else is a reason an agent gives
        _ => return,
    };
    let row = rynas_scheduler::history::NewRun::skipped(
        &super::task_id_of(device),
        &format!("{}:{}", super::task_id_of(device), run.run_id),
        at,
        run.trigger.unwrap_or(RunTrigger::Timer),
        reason,
        detail,
    );
    if let Err(e) = link.history.record_once(&row) {
        tracing::warn!("PC backup {device}: scheduler history: {e}");
    }
}

// ------------------------------------------------------------------ maintenance

/// Clears the maintenance flag of a group, however the task ends.
pub struct Maintaining {
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
pub(super) fn take(svc: &Arc<PcAgent>, group: &str) -> Result<Maintaining, String> {
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

/// Waits (up to 30 minutes) until `need` bytes of the budget are free of reads; `None` when
/// they never were, `Err` when the task is cancelled.
async fn wait_memory(svc: &Arc<PcAgent>, need: u64, budget: u64, ctx: &TaskCtx) -> Result<Option<MemSlot>, String> {
    let until = std::time::Instant::now() + Duration::from_secs(30 * 60);
    let mut said = false;
    loop {
        if let Some(s) = try_reserve(svc, need, budget) {
            return Ok(Some(s));
        }
        if std::time::Instant::now() >= until {
            return Ok(None);
        }
        if !said {
            ctx.log("waiting for downloads or browsing to give back memory of the budget");
            said = true;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(10)) => {}
            _ = ctx.token().cancelled() => return Err("cancelled".into()),
        }
    }
}

/// Removes upload temporaries (`.tmp-*`) older than `age` in the repository's folders.
pub fn sweep_temp(repo: &Path, age: Duration) -> usize {
    let mut dirs: Vec<PathBuf> = ["keys", "locks", "snapshots", "index"].iter().map(|t| repo.join(t)).collect();
    if let Ok(rd) = std::fs::read_dir(repo.join("data")) {
        dirs.extend(rd.flatten().map(|e| e.path()));
    }
    let mut n = 0;
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            if !e.file_name().to_string_lossy().starts_with(".tmp-") {
                continue;
            }
            let old = e
                .metadata()
                .ok()
                .filter(std::fs::Metadata::is_file)
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|el| el >= age);
            if old && std::fs::remove_file(e.path()).is_ok() {
                n += 1;
            }
        }
    }
    n
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
    // 3. prune and check, only inside the budget, which reads of other groups share
    let mut freed = 0;
    let room = if estimate > budget { None } else { wait_memory(svc, estimate, budget, ctx).await? };
    let postponed = if estimate > budget {
        let why = format!(
            "prune and check postponed: they need about {} MiB, over the budget of {} MiB",
            estimate >> 20,
            budget >> 20
        );
        ctx.log(&why);
        Some(why)
    } else if room.is_none() {
        let why = "prune and check postponed: downloads and browsing kept the memory budget busy for 30 minutes".to_string();
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
    drop(room);
    ctx.progress(90);
    // 4. upload leftovers (rynasd stopped in the middle of an upload; no upload runs now),
    // then the sizes again
    let (p, n) = (PathBuf::from(&g.repo), group.to_string());
    let used = tokio::task::spawn_blocking(move || {
        let swept = sweep_temp(&p, Duration::from_secs(86_400));
        (swept, dir_size(&p))
    })
    .await
    .unwrap_or((0, 0));
    if used.0 > 0 {
        ctx.log(&format!("{} unfinished upload files removed", used.0));
    }
    let used = used.1;
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

/// How long after its hour a missed maintenance still runs (the NAS was off or busy then):
/// short enough that a maintenance missed at night does not start in the afternoon.
const MAINT_CATCH_UP: jiff::SignedDuration = jiff::SignedDuration::from_hours(6);
/// Past this after its hour, a maintenance is a catch-up: it gives way to PC backups.
const MAINT_ON_TIME: jiff::SignedDuration = jiff::SignedDuration::from_hours(1);
/// A PC that asked for work this recently is online (agents poll every few minutes).
const AGENT_ONLINE_MS: u64 = 15 * 60 * 1000;

/// The maintenance due at `now`: the local day (days since 1970-01-01) of the last
/// `hour:00` at most [`MAINT_CATCH_UP`] ago, with systemd's DST rules (an hour that does not
/// exist that day has no maintenance, a repeated one counts once), and whether it is a
/// catch-up (more than [`MAINT_ON_TIME`] after its hour).
fn maint_due(hour: u8, now: jiff::Timestamp, tz: &jiff::tz::TimeZone) -> Option<(i64, bool)> {
    let spec = rynas_schedule::CalendarSpec::parse(&format!("*-*-* {:02}:00:00", hour.min(23))).ok()?;
    let slot = spec.next_after(tz, now.checked_sub(MAINT_CATCH_UP).ok()?)?;
    if slot > now {
        return None;
    }
    let late = now.duration_since(slot) >= MAINT_ON_TIME;
    let date = slot.to_zoned(tz.clone()).date();
    date.since(jiff::civil::date(1970, 1, 1)).ok().map(|s| (i64::from(s.get_days()), late))
}

/// A PC of `group` is about to back up: online (asked for work in the last
/// [`AGENT_ONLINE_MS`]) with a command waiting for it, or with a scheduled backup due (no
/// good version for its interval, plan not paused). A catch-up maintenance waits for it:
/// it would lock the repository right when the backup starts. A running backup or a recent
/// upload is refused by [`take`] already.
fn backup_pending(reg: &Registry, group: &str, now: u64, paused_all: bool) -> bool {
    reg.devices.values().any(|d| {
        let online = d.last_seen_ms.is_some_and(|t| now.saturating_sub(t) < AGENT_ONLINE_MS);
        // a backup due outside its allowed window waits for the window, not now; while
        // every task is paused the agents' plans are paused too (S6)
        // (the agent's own rules; "also at every start" is not foreseeable here)
        let p = &d.settings.plan;
        let tz = rynas_power::system_tz();
        // what this PC's agent follows (an older one: the calendar's spacing as its interval)
        let (trigger, every_h) = super::plan_trigger(p, &tz, now, agent_follows_calendar(&d.agent_version));
        let spec = match &trigger {
            rynas_api_types::scheduler::TaskTrigger::Calendar { on_calendar, .. } => rynas_schedule::CalendarSpec::parse(on_calendar).ok(),
            _ => None,
        };
        let due = !p.paused
            && !paused_all
            && rynas_schedule::window::plan_due(spec.as_ref(), u64::from(every_h) * 3_600_000, &tz, super::last_ok(d), 0, 0) <= now
            && p.window.as_deref().is_none_or(|w| rynas_schedule::window::is_open(w, &tz, now));
        d.group == group && !d.revoked && online && (d.command.is_some() || due)
    })
}

/// The notification a device should have open: late or critical only when a backup was
/// missed while the PC was online ([`super::missed_online`]), and only once its last good
/// version (or the pairing) is older than `alert_hours` (critical: `critical_hours`, see
/// [`super::state_of`]). A PC that is merely off never notifies.
pub(super) fn want_alert(d: &Dev, paused_all: bool, tz: &jiff::tz::TimeZone, now: u64) -> Alert {
    let missed = super::missed_online(d, paused_all, tz, now);
    let since = d.last_ok_ms.unwrap_or(d.created_ms);
    let old = |h: u32| h > 0 && now.saturating_sub(since) >= u64::from(h) * 3_600_000;
    match super::state_of(d, false, missed, now) {
        PcState::Critical => Alert::Critical,
        PcState::Late if old(d.settings.alert_hours) => Alert::Late,
        _ => Alert::None,
    }
}

/// Late and critical notifications of one device (state changes only).
fn alerts(svc: &PcAgent, name: &str, d: &Dev) {
    // the btrfs method's alerts are the folder's (`pcbackup.late` and the rest)
    if d.revoked || (d.settings.method == PcMethod::Btrfs && d.alert == Alert::None) {
        return;
    }
    let want = want_alert(d, svc.paused_all(), &rynas_power::system_tz(), now_ms());
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
                Some(_) => format!("A backup was due while {name} was on and none ran; the last good version is older than {h} hours."),
                None => format!("A backup was due while {name} was on and none ran; {name} has no version yet."),
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

/// Every five minutes: the sessions and the late notifications. Each group's maintenance
/// once a day is the scheduler task `pc-maintenance` ([`spawn_nightly`]): from its hour on,
/// retried while a PC is busy, and run late when the NAS was off or busy at that hour, up to
/// 6 hours after it, as a catch-up that gives way to any PC of the group about to back up;
/// the reason stays on the group when it cannot run.
pub fn spawn_watch(svc: Arc<PcAgent>) {
    // the registry's touched counters, written now and then off the async threads
    let s2 = svc.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(super::FLUSH_EVERY).await;
            let s3 = s2.clone();
            let _ = tokio::task::spawn_blocking(move || s3.flush()).await;
        }
    });
    // the groups' sizes counted once at the start, so no upload walks a repository
    let s2 = svc.clone();
    tokio::task::spawn_blocking(move || count_used(&s2, &s2.snapshot()));
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(90)).await;
        loop {
            end_sessions(&svc).await;
            let reg = svc.snapshot();
            for (n, d) in &reg.devices {
                alerts(&svc, n, d);
            }
            tokio::time::sleep(Duration::from_secs(300)).await;
        }
    });
}

/// One pass over the groups due for tonight's maintenance (`day`): each one not maintained
/// yet is started, or its reason kept when it cannot start (a catch-up gives way to a PC
/// of the group about to back up). Returns the tasks started and how many groups wait.
fn maintain_due(svc: &Arc<PcAgent>, day: i64, late: bool) -> (Vec<(String, String)>, usize) {
    let reg = svc.snapshot();
    let (mut started, mut waiting) = (Vec::new(), 0);
    for (n, g) in &reg.groups {
        if g.maintained_day == Some(day) || !reg.devices.values().any(|d| d.group == *n) {
            continue;
        }
        // a catch-up never takes the repository from a PC about to back up: tried again
        // in five minutes, until the catch-up window ends
        let r = if late && backup_pending(&reg, n, now_ms(), svc.paused_all()) {
            Err(Error::new(StatusCode::CONFLICT, error_codes::BUSY, "a PC of the group is about to back up (late maintenance gives way)".into()))
        } else {
            start(svc, n, "rynas", true)
        };
        match r {
            Ok(id) => {
                let _ = svc.edit(|r| {
                    if let Some(x) = r.groups.get_mut(n) {
                        x.maintained_day = Some(day);
                    }
                    Ok(())
                });
                started.push((n.clone(), id));
            }
            Err(e) => {
                waiting += 1;
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
    (started, waiting)
}

/// The scheduled nightly maintenance (scheduler task `pc-maintenance` at the maintenance
/// hour, caught up after boot up to six hours late; before S3 a loop of rynasd): the task
/// `pcbackup.nightly` starts each group's maintenance (`pcbackup.maintenance`), trying a
/// busy group again every five minutes until the catch-up window ends, and waits for them;
/// it fails when one of them failed.
pub fn spawn_nightly(p: &crate::protection::Protection, who: &str) -> Result<String, String> {
    let svc = p
        .ops
        .pc_agent
        .get()
        .and_then(std::sync::Weak::upgrade)
        .ok_or("PC backups are not enabled in this rynasd")?;
    let s = svc.clone();
    svc.ops
        .tasks
        .spawn("pcbackup.nightly", who, move |ctx| async move {
            let tz = rynas_power::system_tz();
            let mut started: Vec<(String, String)> = Vec::new();
            loop {
                let hour = s.snapshot().settings.maintenance_hour;
                let Some((day, late)) = maint_due(hour, jiff::Timestamp::now(), &tz) else {
                    ctx.log(&format!("the maintenance window of {hour:02}:00 is over"));
                    break;
                };
                let (now, waiting) = maintain_due(&s, day, late);
                for (g, id) in &now {
                    ctx.log(&format!("maintenance of backup group {g} started ({id})"));
                }
                started.extend(now);
                if waiting == 0 {
                    break;
                }
                ctx.log(&format!("{waiting} backup groups busy: trying again in five minutes"));
                tokio::select! {
                    _ = ctx.token().cancelled() => return Err("cancelled".into()),
                    _ = tokio::time::sleep(Duration::from_secs(300)) => {}
                }
            }
            if started.is_empty() {
                ctx.log("no backup group to maintain");
            }
            let mut failed = Vec::new();
            for (g, id) in &started {
                loop {
                    match s.ops.tasks.get(id) {
                        Some(t) if t.status.is_finished() => {
                            if t.status != rynas_api_types::TaskStatus::Ok {
                                failed.push(g.clone());
                            }
                            break;
                        }
                        None => break,
                        _ => tokio::time::sleep(Duration::from_secs(5)).await,
                    }
                }
            }
            if failed.is_empty() {
                Ok(())
            } else {
                Err(format!("the maintenance of {} failed", failed.join(", ")))
            }
        })
        .map_err(|e| e.to_string())
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
/// whole in memory; the events of the worker come on its stderr). Inside the memory budget
/// and never while the group is maintained ([`admit_read`]).
pub async fn stream(svc: &Arc<PcAgent>, device: &str, version: String, paths: Vec<String>, zip: bool, name: &str) -> Res<Response> {
    let d = svc.device(device)?;
    let g = svc.group(&d.group)?;
    let permit = svc.streams.clone().try_acquire_owned().map_err(|_| {
        Error::new(StatusCode::TOO_MANY_REQUESTS, error_codes::RATE_LIMITED, "two downloads are running: wait for one to finish".into())
    })?;
    let (mem, launch) = admit_read(svc, &d.group, &g).await?;
    let req = svc.request(
        &g,
        WorkerOp::Stream {
            snapshot: version,
            paths,
            zip,
            scope: Some(super::scope(device)),
        },
    )?;
    let (argv, _) = rynas_backup::runner::command(&launch, &format!("pc-{device}"), &req, None).map_err(Error::internal)?;
    let child = tokio::process::Command::new(&argv[0])
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
    let (stream, size) = from_worker(child, &body, !zip, (permit, mem)).await?;
    let mut r = attachment(Body::from_stream(stream), name, zip);
    if let Some(n) = size {
        // a body shorter than this is a broken download to the browser too
        r.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from(n));
    }
    Ok(r)
}

/// The worker's stderr: the size of a single file (sent before its data) and the last error.
struct Events {
    size: Option<tokio::sync::oneshot::Receiver<u64>>,
    done: tokio::task::JoinHandle<Option<String>>,
}

fn read_events(stderr: tokio::process::ChildStderr) -> Events {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let done = tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut tx = Some(tx);
        let mut last = None;
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        // read to the end (a worker writing to a full pipe would stop), keep what matters
        while let Ok(Some(l)) = lines.next_line().await {
            match serde_json::from_str::<WorkerEvent>(&l) {
                Ok(WorkerEvent::StreamSize { bytes }) => {
                    if let Some(t) = tx.take() {
                        let _ = t.send(bytes);
                    }
                }
                Ok(WorkerEvent::Error { message }) => last = Some(message),
                _ => {}
            }
        }
        last
    });
    Events { size: Some(rx), done }
}

/// The body of a download from a started worker (`want_size`: a single file, whose size the
/// worker sends first). The stream yields an error, and so breaks the download in the
/// browser, when the worker ends badly after the first bytes: a missing pack, a read error,
/// the memory limit. `hold` is kept until the stream ends.
pub async fn from_worker<H: Send + 'static>(
    mut child: tokio::process::Child,
    request: &[u8],
    want_size: bool,
    hold: H,
) -> Res<(futures_util::stream::BoxStream<'static, std::io::Result<axum::body::Bytes>>, Option<u64>)> {
    use futures_util::StreamExt;
    let mut stdin = child.stdin.take().ok_or_else(|| Error::internal("no stdin"))?;
    stdin.write_all(request).await.map_err(Error::internal)?;
    drop(stdin);
    let mut stdout = child.stdout.take().ok_or_else(|| Error::internal("no stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| Error::internal("no stderr"))?;
    let mut events = read_events(stderr);
    // the first bytes decide: data → 200 and the rest streams; nothing → the worker's error
    let mut first = vec![0u8; 64 << 10];
    let n = match tokio::time::timeout(READ_TIMEOUT, stdout.read(&mut first)).await {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => return Err(Error::internal(e)),
        Err(_) => return Err(Error::new(StatusCode::GATEWAY_TIMEOUT, error_codes::TIMEOUT, "the worker did not answer".into())),
    };
    if n == 0 {
        let msg = (&mut events.done).await.ok().flatten();
        let status = child.wait().await.ok();
        let msg = match msg {
            Some(m) => m,
            // an empty file is a whole answer
            None if status.is_some_and(|s| s.success()) => {
                return Ok((futures_util::stream::iter(Vec::new()).boxed(), want_size.then_some(0)));
            }
            None => "the worker ended without an answer".into(),
        };
        let status = if msg.contains("not found") || msg.contains("is not a file") { StatusCode::NOT_FOUND } else { StatusCode::BAD_GATEWAY };
        let code = if status == StatusCode::NOT_FOUND { error_codes::NOT_FOUND } else { error_codes::BACKEND_UNAVAILABLE };
        return Err(Error::new(status, code, msg));
    }
    first.truncate(n);
    // the size event was written (and flushed) before the first byte of data
    let size = match (want_size, events.size.take()) {
        (true, Some(rx)) => tokio::time::timeout(Duration::from_secs(2), rx).await.ok().and_then(Result::ok),
        _ => None,
    };
    let head = futures_util::stream::once(async move { Ok::<_, std::io::Error>(axum::body::Bytes::from(first)) });
    struct St<H> {
        out: tokio::process::ChildStdout,
        child: tokio::process::Child,
        events: Events,
        _hold: H,
        over: bool,
    }
    let st = St {
        out: stdout,
        child,
        events,
        _hold: hold,
        over: false,
    };
    let rest = futures_util::stream::unfold(st, |mut st| async move {
        if st.over {
            return None;
        }
        let mut buf = vec![0u8; 256 << 10];
        match st.out.read(&mut buf).await {
            Ok(0) => {
                // the end of the data: only a worker that ended well makes it a whole file
                st.over = true;
                let status = st.child.wait().await;
                let msg = (&mut st.events.done).await.ok().flatten();
                match status {
                    Ok(s) if s.success() => None,
                    other => {
                        let why = msg.unwrap_or_else(|| format!("the worker ended with {other:?}"));
                        tracing::warn!("pc backup: a download broke off: {why}");
                        Some((Err(std::io::Error::other(why)), st))
                    }
                }
            }
            Ok(k) => {
                buf.truncate(k);
                Some((Ok(axum::body::Bytes::from(buf)), st))
            }
            Err(e) => {
                st.over = true;
                Some((Err(e), st))
            }
        }
    });
    Ok((head.chain(rest).boxed(), size))
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
    fn maintenance_catches_up_within_6_hours() {
        let Ok(z) = jiff::tz::TimeZone::get("Europe/Rome") else { return };
        let at = |s: &str| s.parse::<jiff::civil::DateTime>().unwrap().to_zoned(z.clone()).unwrap().timestamp();
        let day = |s: &str| s.parse::<jiff::civil::Date>().unwrap().since(jiff::civil::date(1970, 1, 1)).unwrap().get_days() as i64;
        // in its hour (on time), and later the same morning as a catch-up (the NAS was off)
        assert_eq!(maint_due(3, at("2026-11-16T03:02"), &z), Some((day("2026-11-16"), false)));
        assert_eq!(maint_due(3, at("2026-11-16T03:59"), &z), Some((day("2026-11-16"), false)));
        assert_eq!(maint_due(3, at("2026-11-16T04:00"), &z), Some((day("2026-11-16"), true)));
        assert_eq!(maint_due(3, at("2026-11-16T08:59"), &z), Some((day("2026-11-16"), true)));
        // more than 6 hours later (an 08:00 boot after a 01:00 maintenance hour): skipped
        assert_eq!(maint_due(3, at("2026-11-16T09:01"), &z), None);
        assert_eq!(maint_due(1, at("2026-11-16T08:00"), &z), None);
        assert_eq!(maint_due(3, at("2026-11-16T02:59"), &z), None);
        // across midnight
        assert_eq!(maint_due(23, at("2026-11-17T01:00"), &z), Some((day("2026-11-16"), true)));
        // the spring-forward day has no 02:00
        assert_eq!(maint_due(2, at("2026-03-29T05:00"), &z), None);
        assert_eq!(maint_due(2, at("2026-03-30T05:00"), &z), Some((day("2026-03-30"), true)));
    }

    #[test]
    fn late_maintenance_gives_way_to_pc_backups() {
        let now = 1_800_000_000_000u64;
        let hour = 3_600_000u64;
        let dev = |group: &str, seen: Option<u64>, ok: Option<u64>| {
            let mut d: Dev = serde_json::from_value(serde_json::json!({
                "created_ms": 0, "group": group,
                "settings": serde_json::to_value(PcDeviceSettings::default()).unwrap()
            }))
            .unwrap();
            d.last_seen_ms = seen;
            d.last_ok_ms = ok;
            d
        };
        let mut reg = Registry::default();
        // online, last good version 2 h ago, daily plan: not due
        reg.devices.insert("a".into(), dev("g", Some(now - 60_000), Some(now - 2 * hour)));
        assert!(!backup_pending(&reg, "g", now, false));
        // due (25 h) but offline: nothing will start
        reg.devices.insert("b".into(), dev("g", Some(now - 2 * hour), Some(now - 25 * hour)));
        assert!(!backup_pending(&reg, "g", now, false));
        // online and due: about to back up
        reg.devices.get_mut("b").unwrap().last_seen_ms = Some(now - 60_000);
        assert!(backup_pending(&reg, "g", now, false));
        assert!(!backup_pending(&reg, "other", now, false), "another group's PC");
        // every task paused on the NAS: the PC's plan is paused too
        assert!(!backup_pending(&reg, "g", now, true));
        // its allowed window closed now: it waits for the window
        let closed: String = {
            let cell = rynas_schedule::window::cell_of(&jiff::Timestamp::from_millisecond(now as i64).unwrap().to_zoned(rynas_power::system_tz()).datetime());
            (0..168).map(|c| if c == cell { '0' } else { '1' }).collect()
        };
        reg.devices.get_mut("b").unwrap().settings.plan.window = Some(closed);
        assert!(!backup_pending(&reg, "g", now, false));
        reg.devices.get_mut("b").unwrap().settings.plan.window = None;
        // paused plan: no scheduled backup
        reg.devices.get_mut("b").unwrap().settings.plan.paused = true;
        assert!(!backup_pending(&reg, "g", now, false));
        // a command waiting ("Back up now") for an online PC
        reg.devices.get_mut("a").unwrap().command = Some(AgentCommand::BackupNow { id: 1 });
        assert!(backup_pending(&reg, "g", now, false));
    }

    #[test]
    fn unique_names() {
        assert_eq!(unique("pc", |n| n == "pc"), "pc-2");
        assert_eq!(unique("pc", |_| false), "pc");
        let long = "a".repeat(32);
        let u = unique(&long, |n| n == long);
        assert!(u.len() <= 32 && u.ends_with("-2"), "{u}");
    }

    fn fake_worker(script: &str) -> tokio::process::Child {
        let size = serde_json::to_string(&WorkerEvent::StreamSize { bytes: 204_800 }).unwrap();
        let error = serde_json::to_string(&WorkerEvent::Error { message: "pack 1234 is missing".into() }).unwrap();
        tokio::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .env("SIZE", size)
            .env("ERROR", error)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    async fn collect(s: futures_util::stream::BoxStream<'static, std::io::Result<axum::body::Bytes>>) -> (usize, Option<String>) {
        use futures_util::StreamExt;
        let mut s = s;
        let mut n = 0;
        while let Some(c) = s.next().await {
            match c {
                Ok(b) => n += b.len(),
                Err(e) => return (n, Some(e.to_string())),
            }
        }
        (n, None)
    }

    #[tokio::test]
    async fn a_worker_failing_mid_stream_breaks_the_download() {
        // half the file, then the worker fails: the body ends with an error, not cleanly
        let c = fake_worker(r#"cat >/dev/null; printf '%s\n' "$SIZE" >&2; head -c 102400 /dev/zero; printf '%s\n' "$ERROR" >&2; exit 1"#);
        let (body, size) = from_worker(c, b"{}", true, ()).await.unwrap();
        assert_eq!(size, Some(204_800));
        let (n, err) = collect(body).await;
        assert_eq!(n, 102_400);
        assert_eq!(err.as_deref(), Some("pack 1234 is missing"));
        // killed without a word (the memory limit): broken as well
        let c = fake_worker(r#"cat >/dev/null; head -c 70000 /dev/zero; kill -9 $$"#);
        let (body, size) = from_worker(c, b"{}", false, ()).await.unwrap();
        assert_eq!(size, None);
        let (n, err) = collect(body).await;
        assert_eq!(n, 70_000);
        assert!(err.is_some());
        // a whole file ends cleanly
        let c = fake_worker(r#"cat >/dev/null; printf '%s\n' "$SIZE" >&2; head -c 204800 /dev/zero"#);
        let (body, size) = from_worker(c, b"{}", true, ()).await.unwrap();
        assert_eq!(size, Some(204_800));
        assert_eq!(collect(body).await, (204_800, None));
        // nothing but an error: refused before any byte
        let c = fake_worker(r#"cat >/dev/null; printf '%s\n' "$ERROR" >&2; exit 1"#);
        let e = from_worker(c, b"{}", true, ()).await.err().unwrap();
        assert_eq!(e.0, StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn upload_leftovers_are_swept() {
        let dir = std::env::temp_dir().join(format!("rynas-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("data/ab")).unwrap();
        std::fs::create_dir_all(dir.join("index")).unwrap();
        std::fs::write(dir.join("data/ab/.tmp-1-x"), b"x").unwrap();
        std::fs::write(dir.join("index/.tmp-1-y"), b"y").unwrap();
        std::fs::write(dir.join("data/ab/abcd"), b"keep").unwrap();
        // too young: kept
        assert_eq!(sweep_temp(&dir, Duration::from_secs(3600)), 0);
        assert_eq!(sweep_temp(&dir, Duration::ZERO), 2);
        assert!(dir.join("data/ab/abcd").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dispositions() {
        assert_eq!(
            content_disposition("a b\"è.txt"),
            "attachment; filename=\"a b__.txt\"; filename*=UTF-8''a%20b%22%C3%A8.txt"
        );
    }
}
