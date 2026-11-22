//! A btrfs filesystem of several devices that lost one (`docs/phase2/btrfs-degraded.md`):
//! replace the missing device, remove it, convert the chunks a degraded mount wrote with a
//! weaker profile back, mount a configured volume degraded, and the per-filesystem setting
//! of the degraded mount after boot. The plans and their runner are
//! [`rynas_storage::btrfs_repair`]; the checks here are the usual ones of a storage write: a
//! valid plan on a fresh inventory, the typed name (and the new disk's serial for a
//! replace), the disk free for the kernel, no other operation on the disk or the volume.
//!
//! Filesystems are named by UUID in the routes: a root filesystem or an unlabelled one has
//! no volume label. The confirmation is the label, or the UUID without one.

use std::collections::HashMap;
use std::path::PathBuf;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::error_codes;
use rynas_api_types::storage::{
    BtrfsBlocker, BtrfsFilesystem, BtrfsReplaceRequest, BtrfsRepairRequest, BtrfsSettings, BtrfsSettingsState, StoragePlan,
};
use rynas_storage::btrfs_repair::{self as br, Guard, MountCandidate};
use rynas_storage::domain::TUNING_KEY;
use rynas_storage::exec::Machine;

use super::{
    StoState, StorageOps, bad, check_free_now, confirm, conflict, disks_of, heavy_on, invalid_plan, json_body, spawn, started, sto, tuning_for,
};
use crate::ops::{Error, Res, user};

/// The routes, merged into the storage write side's router (its state).
pub fn router() -> Router<StoState> {
    Router::new()
        .route("/v1/storage/btrfs/{uuid}/replace", post(replace))
        .route("/v1/storage/btrfs/{uuid}/remove-missing", post(remove_missing))
        .route("/v1/storage/btrfs/{uuid}/convert", post(convert))
        .route("/v1/storage/btrfs/{uuid}/mount-degraded", post(mount_degraded))
        .route("/v1/storage/btrfs/{uuid}/settings", get(settings_get).put(settings_put))
        .route("/v1/storage/btrfs/{uuid}/reset-errors", post(reset_errors))
}

/// `POST /v1/storage/btrfs/{uuid}/reset-errors`: sets the error counters of every device of a
/// mounted filesystem back to zero (`btrfs device stats -z`), as a task. Administrators only;
/// 409 while a device is missing (btrfs cannot reset a device it does not have). The counters
/// are what keeps the volume at "Warning" after errors
/// ([`rynas_storage::inventory::device_verdict`]): the reset is their acknowledgement, and
/// the task log keeps the values from before it. The volume is reserved like any other
/// operation on it, and the task checks again that the mount point still holds this
/// filesystem: on an empty mount point `-z` would reset the counters of the one below.
async fn reset_errors(State(st): State<StoState>, h: HeaderMap, UrlPath(uuid): UrlPath<String>) -> Res<Response> {
    let (s, user) = (sto(&st)?, user(&h)?);
    s.ops.admins.require_admin(&user, "resetting btrfs error counters")?;
    check_uuid(&uuid)?;
    let inv = s.inventory(false).await?;
    let fs = find(&inv, &uuid)?;
    let name = name_of(&fs);
    let mnt = fs.mountpoint.clone().ok_or_else(|| conflict(format!("filesystem {name} is not mounted")))?;
    if fs.devices.iter().any(|d| d.missing) || fs.repair.as_ref().is_some_and(|r| r.degraded()) {
        return Err(conflict(format!("{name}: a device is missing; replace or remove it before resetting the error counters")));
    }
    let held = s.reserve(vec![format!("volume:{name}"), format!("btrfs-stats:{}", fs.uuid)])?;
    let want = fs.uuid.clone();
    let id = spawn(&s, "storage.btrfs-reset-errors", &user, held, move |m| async move {
        let mut o = rynas_storage::exec::RunOpts::new(std::time::Duration::from_secs(30));
        o.cancellable = false;
        let argv = ["findmnt", "-n", "-r", "-o", "UUID,FSTYPE", "--mountpoint", &mnt].map(String::from).to_vec();
        let found = m.run(argv, o.clone()).await.unwrap_or_default();
        if !mounted_as(&found.stdout, &want) {
            return Err(format!("{mnt} no longer holds btrfs filesystem {want}: nothing reset"));
        }
        let mut o = rynas_storage::exec::RunOpts::new(std::time::Duration::from_secs(60));
        o.cancellable = false;
        // `-z` prints the values it resets: they stay in the task log
        m.run(vec!["btrfs".into(), "device".into(), "stats".into(), "-z".into(), mnt], o).await.map(|_| ())
    })?;
    Ok(started(id))
}

/// `findmnt -n -r -o UUID,FSTYPE --mountpoint <dir>` says the btrfs filesystem `uuid` is
/// mounted there (the last line: the mount on top).
fn mounted_as(findmnt: &str, uuid: &str) -> bool {
    findmnt.lines().rfind(|l| !l.trim().is_empty()).is_some_and(|l| {
        let mut w = l.split_whitespace();
        matches!((w.next(), w.next()), (Some(u), Some("btrfs")) if u.eq_ignore_ascii_case(uuid))
    })
}

/// What the storage pages call the filesystem, and what a confirmation types: its label, or
/// its UUID.
pub fn name_of(fs: &BtrfsFilesystem) -> String {
    fs.label.clone().filter(|l| !l.is_empty()).unwrap_or_else(|| fs.uuid.clone())
}

fn check_uuid(uuid: &str) -> Res<()> {
    let ok = uuid.len() == 36 && uuid.chars().all(|c| c == '-' || c.is_ascii_hexdigit());
    if ok { Ok(()) } else { Err(Error::bad_request("invalid filesystem UUID")) }
}

fn find(inv: &rynas_storage::Inventory, uuid: &str) -> Res<BtrfsFilesystem> {
    inv.filesystems
        .iter()
        .find(|f| f.uuid.eq_ignore_ascii_case(uuid))
        .cloned()
        .ok_or_else(|| Error::not_found(format!("btrfs filesystem {uuid}")))
}

fn typed(fs: &BtrfsFilesystem, confirm: &str) -> Res<()> {
    let name = name_of(fs);
    if confirm.trim() == name {
        Ok(())
    } else {
        Err(bad(error_codes::CONFIRMATION_REQUIRED, format!("type the volume name ({name}) in \"confirm\" to confirm")))
    }
}

/// The file of the degraded mounts done ([`Guard`]).
fn guard_path(s: &StorageOps) -> Option<PathBuf> {
    s.btrfs_guard.get().cloned()
}

/// Mount point and fstab options of every configured btrfs volume that is not mounted.
pub fn fstab_volumes(inv: &rynas_storage::Inventory, fstab: &str) -> HashMap<String, (String, Vec<String>)> {
    br::unmounted(&br::parse_fstab(fstab), &inv.graph, &inv.mounts, &[])
        .into_iter()
        .map(|u| (u.uuid, (u.mountpoint, u.options)))
        .collect()
}

/// A plan that cannot run is an error; a dry run answers with it.
fn checked(p: StoragePlan, dry_run: bool) -> Res<Option<Response>> {
    if !p.valid {
        return Err(invalid_plan(&p));
    }
    Ok(dry_run.then(|| Json(p).into_response()))
}

/// Forgets the degraded mount of `uuid` after a replace or a removal left nothing missing: the
/// missing device was rebuilt from the copies or is gone, so no device with older copies is
/// left (a device that merely came back keeps the record until a clean scrub instead).
fn forget_if_whole(guard: Option<PathBuf>, uuid: &str, inv: &rynas_storage::Inventory) {
    let Some(path) = guard else { return };
    let whole = inv.filesystems.iter().any(|f| f.uuid.eq_ignore_ascii_case(uuid) && f.mountpoint.is_some() && f.repair.as_ref().is_none_or(|r| !r.degraded()));
    if whole && let Err(e) = Guard::update(&path, |g| g.forget(uuid)) {
        tracing::warn!("btrfs degraded-mount record: {e}");
    }
}

async fn replace(State(st): State<StoState>, h: HeaderMap, UrlPath(uuid): UrlPath<String>, body: Result<Json<BtrfsReplaceRequest>, JsonRejection>) -> Res<Response> {
    let (s, user) = (sto(&st)?, user(&h)?);
    let req = json_body(body)?;
    check_uuid(&uuid)?;
    let inv = s.inventory(true).await?;
    let fs = find(&inv, &uuid)?;
    let disks = disks_of(&inv, std::slice::from_ref(&req.new_disk))?;
    let disk = disks[0];
    // leftovers (old partitions or signatures) are erased; live use and the system disk are not
    if disk.in_use || disk.system {
        return Err(conflict(format!("disk {} is in use: {}", disk.name, disk.used_by.join(", "))));
    }
    let holds = br::disk_holds(&inv, &disk.name, &fs.uuid);
    let p = br::plan_replace(&fs, req.devid, &disk.name, disk.size, &holds);
    if let Some(r) = checked(p.clone(), req.dry_run)? {
        return Ok(r);
    }
    let name = name_of(&fs);
    confirm(&name, req.confirm.trim(), &disks, &req.serials)?;
    check_free_now(&s, &inv, std::slice::from_ref(&req.new_disk))?;
    let held = s.reserve(vec![format!("disk:{}", disk.name), format!("volume:{name}")])?;
    let mnt = fs.mountpoint.clone();
    let ops = s.clone();
    let guard = guard_path(&s);
    let checked = inv.clone();
    let new_disk = req.new_disk.clone();
    let id = spawn(&s, "storage.btrfs-replace", &user, held, move |m| async move {
        // deduplication of the volume gives way (D-DEDUP)
        let _fs = heavy_on(mnt.as_deref(), "storage.btrfs-replace").await;
        // the disk may be another one under the same kernel name since the request
        let now = ops.inventory(true).await.map_err(|e| e.2)?;
        super::same_disks(&checked, &now, std::slice::from_ref(&new_disk))?;
        br::run_plan(&m, &p).await?;
        if let Ok(inv) = ops.inventory(true).await {
            forget_if_whole(guard, &uuid, &inv);
        }
        Ok(())
    })?;
    Ok(started(id))
}

async fn remove_missing(State(st): State<StoState>, h: HeaderMap, UrlPath(uuid): UrlPath<String>, body: Result<Json<BtrfsRepairRequest>, JsonRejection>) -> Res<Response> {
    let (s, user) = (sto(&st)?, user(&h)?);
    let req = json_body(body)?;
    check_uuid(&uuid)?;
    let inv = s.inventory(true).await?;
    let fs = find(&inv, &uuid)?;
    if let Some(r) = checked(br::plan_remove_missing(&fs), req.dry_run)? {
        return Ok(r);
    }
    typed(&fs, &req.confirm)?;
    let p = br::plan_remove_missing(&fs);
    let name = name_of(&fs);
    let held = s.reserve(vec![format!("volume:{name}")])?;
    let mnt = fs.mountpoint.clone();
    let ops = s.clone();
    let guard = guard_path(&s);
    let id = spawn(&s, "storage.btrfs-remove-missing", &user, held, move |m| async move {
        let _fs = heavy_on(mnt.as_deref(), "storage.btrfs-remove-missing").await;
        br::run_plan(&m, &p).await?;
        if let Ok(inv) = ops.inventory(true).await {
            forget_if_whole(guard, &uuid, &inv);
        }
        Ok(())
    })?;
    Ok(started(id))
}

async fn convert(State(st): State<StoState>, h: HeaderMap, UrlPath(uuid): UrlPath<String>, body: Result<Json<BtrfsRepairRequest>, JsonRejection>) -> Res<Response> {
    let (s, user) = (sto(&st)?, user(&h)?);
    let req = json_body(body)?;
    check_uuid(&uuid)?;
    let inv = s.inventory(true).await?;
    let fs = find(&inv, &uuid)?;
    let p = br::plan_convert(&fs);
    if let Some(r) = checked(p.clone(), req.dry_run)? {
        return Ok(r);
    }
    let name = name_of(&fs);
    let held = s.reserve(vec![format!("volume:{name}")])?;
    let mnt = fs.mountpoint.clone();
    let id = spawn(&s, "storage.btrfs-convert", &user, held, move |m| async move {
        let _fs = heavy_on(mnt.as_deref(), "storage.btrfs-convert").await;
        br::run_plan(&m, &p).await
    })?;
    Ok(started(id))
}

/// The degraded-mount candidate of an unmounted filesystem, or why it cannot be one (the
/// setting is ignored: an administrator asks).
fn candidate_of(fs: &BtrfsFilesystem, inv: &rynas_storage::Inventory, guard: &Guard) -> Res<MountCandidate> {
    let fstab = std::fs::read_to_string("/etc/fstab").unwrap_or_default();
    let volumes = fstab_volumes(inv, &fstab);
    let (ok, blocked) = br::candidates(std::slice::from_ref(fs), &[], guard, &volumes);
    if let Some(c) = ok.into_iter().next() {
        return Ok(c);
    }
    Err(match blocked.first().map(|b| b.1) {
        Some(BtrfsBlocker::SplitBrain) => conflict(
            "the devices present are the ones that were missing at the last degraded mount: they hold older data. Reconnect the devices of that mount; to give up their data, mount these by hand (see the documentation)",
        ),
        Some(BtrfsBlocker::NoDevices) => conflict("no device of the filesystem is present"),
        Some(BtrfsBlocker::Asleep) => conflict("a disk of the filesystem sleeps: its superblock was not read, try again"),
        Some(_) => conflict("the filesystem cannot be mounted degraded now"),
        None if fs.repair.as_ref().is_some_and(|r| r.mounted) => conflict("the filesystem is mounted"),
        None => conflict("the filesystem is no configured volume (no /etc/fstab line mounts it at /<label>)"),
    })
}

async fn mount_degraded(State(st): State<StoState>, h: HeaderMap, UrlPath(uuid): UrlPath<String>, body: Result<Json<BtrfsRepairRequest>, JsonRejection>) -> Res<Response> {
    let (s, user) = (sto(&st)?, user(&h)?);
    let req = json_body(body)?;
    check_uuid(&uuid)?;
    let inv = s.inventory(true).await?;
    let fs = find(&inv, &uuid)?;
    let guard_file = guard_path(&s);
    // an unreadable record refuses: it may be the one that tells a split brain
    let guard = match guard_file.as_deref() {
        Some(p) => Guard::load(p).map_err(|e| Error::new(axum::http::StatusCode::CONFLICT, error_codes::CONFLICT, e))?,
        None => Guard::default(),
    };
    let c = candidate_of(&fs, &inv, &guard)?;
    let systemd = std::path::Path::new("/run/systemd/system").exists();
    let suffix = format!("{}-{}", &uuid[..8], rynas_tasks::now_ms());
    let p = br::plan_mount(&c, systemd, &suffix);
    if let Some(r) = checked(p.clone(), req.dry_run)? {
        return Ok(r);
    }
    typed(&fs, &req.confirm)?;
    let held = s.reserve(vec![format!("volume:{}", c.label)])?;
    let id = spawn(&s, "storage.btrfs-mount-degraded", &user, held, move |m| async move {
        mount_and_record(&m, &p, &c, guard_file).await
    })?;
    Ok(started(id))
}

/// Runs the degraded mount of `c`, recorded in the guard first ([`br::begin_record`]) and the
/// record put back when the mount failed and nothing is mounted ([`br::undo_record`]).
pub async fn mount_and_record<M: Machine>(m: &M, p: &StoragePlan, c: &MountCandidate, guard: Option<PathBuf>) -> Result<(), String> {
    let prev = match &guard {
        Some(path) => Some(br::begin_record(path, c, rynas_tasks::now_ms())?),
        None => None,
    };
    if let Err(e) = br::run_plan(m, p).await {
        if let (Some(path), Some(prev)) = (&guard, prev) {
            let mounted = br::mounted_at(&rynas_storage::mounts::host_mountinfo(), &c.mountpoint);
            if let Err(u) = br::undo_record(path, c, prev, mounted) {
                m.log(&format!("the record of the degraded mount could not be put back: {u}"));
            }
        }
        return Err(e);
    }
    m.log(&format!(
        "{} is mounted degraded on {} of {} devices: replace the missing device, or remove it",
        c.mountpoint,
        c.present.len(),
        c.total
    ));
    Ok(())
}

fn settings_state(c: &rynas_config::Config, user: &str, uuid: &str) -> Result<BtrfsSettingsState, String> {
    let (seen, cur, staged) = tuning_for(c, user)?;
    let of = |t: &rynas_api_types::storage::StorageTuning| BtrfsSettings { auto_degraded: !t.btrfs_manual.iter().any(|u| u.eq_ignore_ascii_case(uuid)) };
    let applied = of(&cur);
    let pending = staged.then(|| of(&seen)).filter(|p| *p != applied);
    Ok(BtrfsSettingsState { applied, pending })
}

async fn settings_get(State(st): State<StoState>, h: HeaderMap, UrlPath(uuid): UrlPath<String>) -> Res<Json<BtrfsSettingsState>> {
    let (s, user) = (sto(&st)?, user(&h)?);
    check_uuid(&uuid)?;
    let c = s.config().clone();
    tokio::task::spawn_blocking(move || settings_state(&c, &user, &uuid).map_err(Error::internal))
        .await
        .map_err(Error::internal)?
        .map(Json)
}

async fn settings_put(State(st): State<StoState>, h: HeaderMap, UrlPath(uuid): UrlPath<String>, body: Result<Json<BtrfsSettings>, JsonRejection>) -> Res<Json<BtrfsSettingsState>> {
    let (s, user) = (sto(&st)?, user(&h)?);
    let req = json_body(body)?;
    check_uuid(&uuid)?;
    let uuid = uuid.to_ascii_lowercase();
    let c = s.config().clone();
    tokio::task::spawn_blocking(move || -> Res<BtrfsSettingsState> {
        let (mut doc, _, _) = tuning_for(&c, &user).map_err(Error::internal)?;
        doc.btrfs_manual.retain(|u| !u.eq_ignore_ascii_case(&uuid));
        if !req.auto_degraded {
            doc.btrfs_manual.push(uuid.clone());
        }
        let value = serde_json::to_value(doc).map_err(Error::internal)?;
        c.stage(&user, TUNING_KEY, Some(value))?;
        settings_state(&c, &user, &uuid).map_err(Error::internal)
    })
    .await
    .map_err(Error::internal)?
    .map(Json)
}

#[cfg(test)]
mod tests {
    use super::mounted_as;

    const U: &str = "855bfa9a-2d80-43db-80a8-767697cd9bc9";

    #[test]
    fn the_reset_runs_only_on_its_own_mount() {
        assert!(mounted_as(&format!("{U} btrfs\n"), U));
        assert!(mounted_as(&format!("{} btrfs\n", U.to_uppercase()), U));
        // unmounted meanwhile: nothing, or the filesystem below (the root's)
        assert!(!mounted_as("", U));
        assert!(!mounted_as("24a82255-ac7d-4198-b5d4-8dba11c8f5ed btrfs\n", U));
        assert!(!mounted_as(&format!("{U} ext4\n"), U));
        // another filesystem mounted over it: the last line is the one on top
        assert!(!mounted_as(&format!("{U} btrfs\n24a82255-ac7d-4198-b5d4-8dba11c8f5ed btrfs\n"), U));
        assert!(mounted_as(&format!("24a82255-ac7d-4198-b5d4-8dba11c8f5ed btrfs\n{U} btrfs\n"), U));
    }
}
