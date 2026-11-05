//! Execution of storage plans (the write side): the planner's steps run one by one through a
//! [`Machine`] (in rynasd: a task with its log, progress and cancellation), with
//!
//! - **checks before anything is destroyed**: the typed confirmation (volume label and the
//!   exact serials of the disks, [`check_confirmation`]) and the kernel's own word that no
//!   selected disk or partition is in use ([`Machine::claim_free`], `open(O_EXCL)`);
//! - **secrets on standard input only**: the passphrase is fed to the steps that name it and
//!   never logged; outputs that carry a secret (the recovery key) are not logged either;
//! - **rollback while it is safe**: creating a volume undoes every step done so far when a
//!   later one fails or the task is cancelled (unmount, configuration entries removed, LUKS
//!   closed, array stopped and its superblocks cleared, partitions removed), and reports what
//!   was undone and what could not be; growing or replacing undoes only the preparation of
//!   the new disks, before the array is touched (after that the kernel owns the operation);
//! - **waits that follow the kernel**: md resync/reshape/rebuild/check progress from sysfs,
//!   cancellable (the md operation itself continues).

use std::future::Future;
use std::time::Duration;

use rynas_api_types::storage::{
    DiskSummary, PlanAction, PlanStep, RaidLevel, StoragePlan, VolumeRecord,
};

use crate::Inventory;
use crate::plan::{COMPRESSION, mount_unit};

/// How to run one command.
#[derive(Clone)]
pub struct RunOpts {
    /// Bytes for the standard input (a secret: never logged).
    pub stdin: Option<Vec<u8>>,
    /// Do not copy the output to the log (it carries a secret).
    pub quiet: bool,
    /// Killed when the task is cancelled. Rollback and cleanup commands are not.
    pub cancellable: bool,
    pub timeout: Duration,
}

impl RunOpts {
    pub fn new(timeout: Duration) -> Self {
        Self {
            stdin: None,
            quiet: false,
            cancellable: true,
            timeout,
        }
    }
}

impl std::fmt::Debug for RunOpts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunOpts")
            .field("stdin", &self.stdin.as_ref().map(|_| "<secret>"))
            .field("quiet", &self.quiet)
            .field("cancellable", &self.cancellable)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// What a command printed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
}

/// A change of the `storage.volumes` configuration made by an operation.
#[derive(Debug, Clone, PartialEq)]
pub enum VolumeChange {
    Add(VolumeRecord),
    Remove(String),
}

/// Everything an operation does to the machine. rynasd implements it on top of a task; the
/// tests use [`FakeMachine`].
pub trait Machine: Send + Sync {
    fn log(&self, line: &str);
    fn progress(&self, pct: u8);
    fn is_cancelled(&self) -> bool;
    /// Runs `argv` (never a shell); a non-zero exit is an error whose message ends with the
    /// last line of stderr. The command line is logged, the output unless `quiet`.
    fn run(
        &self,
        argv: Vec<String>,
        opts: RunOpts,
    ) -> impl Future<Output = Result<Output, String>> + Send;
    /// Writes a sysfs attribute.
    fn write_file(&self, path: &str, value: &str) -> Result<(), String>;
    fn read_file(&self, path: &str) -> Option<String>;
    fn exists(&self, path: &str) -> bool;
    fn remove_file(&self, path: &str);
    /// See [`crate::host::claim_free`].
    fn claim_free(&self, dev: &str) -> Result<(), String>;
    /// Sleeps, returning early when the task is cancelled.
    fn sleep(&self, d: Duration) -> impl Future<Output = ()> + Send;
    /// systemd is PID 1: mounts go through mount units.
    fn systemd(&self) -> bool;
    /// Applies a change of the `storage.volumes` configuration (a new revision).
    fn commit(&self, change: VolumeChange) -> impl Future<Output = Result<(), String>> + Send;
}

/// The secrets of one operation, wiped from memory when dropped.
#[derive(Default)]
pub struct Secrets {
    passphrase: Option<Vec<u8>>,
}

impl Secrets {
    pub fn new(passphrase: Option<String>) -> Self {
        Self {
            passphrase: passphrase.map(String::into_bytes),
        }
    }

    pub fn has_passphrase(&self) -> bool {
        self.passphrase.is_some()
    }

    /// Bytes for a step whose `stdin` names `name`.
    fn get(&self, name: &str) -> Result<Vec<u8>, String> {
        match name {
            "passphrase" => self
                .passphrase
                .clone()
                .ok_or_else(|| "this step needs the passphrase".to_string()),
            other => Err(format!("unknown secret {other:?}")),
        }
    }
}

impl Drop for Secrets {
    fn drop(&mut self) {
        if let Some(p) = &mut self.passphrase {
            for b in p.iter_mut() {
                // SAFETY: a plain write to a byte we own; volatile so it is not elided.
                unsafe { std::ptr::write_volatile(b, 0) };
            }
        }
    }
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secrets(<redacted>)")
    }
}

/// Minimum passphrase length for a new LUKS volume.
pub const MIN_PASSPHRASE: usize = 8;

/// Where the LUKS header backup of a new volume is written (tmpfs, removed after the
/// download or at the next boot).
pub fn header_backup_path(label: &str) -> String {
    format!("/run/rynas/luks-header-{label}.img")
}

/// LUKS mapper name of a volume.
pub fn mapper_name(label: &str) -> String {
    format!("rynas-{label}")
}

// ------------------------------------------------------------------------------- checks

/// What a destructive request must name for a disk: its serial, or its kernel name when it
/// has none (virtual and loop disks).
pub fn identity(d: &DiskSummary) -> &str {
    if d.serial.trim().is_empty() {
        &d.name
    } else {
        d.serial.trim()
    }
}

/// The typed confirmation of a destructive request: `confirm` is the volume label and
/// `serials` names exactly the disks that will be erased.
pub fn check_confirmation(
    label: &str,
    confirm: &str,
    disks: &[&DiskSummary],
    serials: &[String],
) -> Result<(), String> {
    if confirm != label {
        return Err(format!(
            "type the volume name ({label}) in \"confirm\" to confirm"
        ));
    }
    let mut want: Vec<&str> = disks.iter().map(|d| identity(d)).collect();
    let mut got: Vec<&str> = serials.iter().map(|s| s.trim()).collect();
    want.sort_unstable();
    got.sort_unstable();
    if want != got {
        return Err(format!(
            "\"serials\" must list the serial numbers of exactly the disks that will be erased ({})",
            disks
                .iter()
                .map(|d| format!("{}: {}", d.name, identity(d)))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(())
}

/// Device nodes to claim before erasing `disk`: the disk and its partitions.
pub fn claim_targets(inv: &Inventory, disk: &str) -> Vec<String> {
    let mut v = vec![format!("/dev/{disk}")];
    if let Some(d) = inv.graph.get(disk) {
        for c in &d.children {
            if let Some(p) = inv.graph.get(c)
                && p.kind == "part"
            {
                v.push(p.path.clone());
            }
        }
    }
    v
}

/// Refuses when the kernel says one of `disks` (or a partition) is in use.
pub fn check_free<M: Machine>(m: &M, inv: &Inventory, disks: &[String]) -> Result<(), String> {
    for d in disks {
        for dev in claim_targets(inv, d) {
            m.claim_free(&dev)?;
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------- helpers

fn timeout_for(argv: &[String]) -> Duration {
    const MIN: u64 = 60;
    let s = match (
        argv.first().map(String::as_str),
        argv.get(1).map(String::as_str),
    ) {
        (Some("btrfs"), Some("scrub")) => 14 * 24 * 3600,
        (Some("mkfs.btrfs"), _) => 30 * MIN,
        (Some("cryptsetup" | "systemd-cryptenroll"), _) => 10 * MIN,
        (Some("mdadm" | "systemctl" | "wipefs" | "sgdisk"), _) => 5 * MIN,
        _ => 2 * MIN,
    };
    Duration::from_secs(s)
}

fn argv_of(step: &PlanStep) -> Option<(&[String], Option<&str>)> {
    match &step.action {
        PlanAction::Command { argv, stdin } if !argv.is_empty() => {
            Some((argv.as_slice(), stdin.as_deref()))
        }
        _ => None,
    }
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// A recovery key as systemd-cryptenroll prints it: 8 groups of 8 modhex characters.
pub fn find_recovery_key(text: &str) -> Option<String> {
    const MODHEX: &str = "cbdefghijklnrtuv";
    text.split(|c: char| !(c.is_ascii_lowercase() || c == '-'))
        .find(|w| {
            let groups: Vec<&str> = w.split('-').collect();
            groups.len() == 8
                && groups
                    .iter()
                    .all(|g| g.len() == 8 && g.chars().all(|c| MODHEX.contains(c)))
        })
        .map(str::to_string)
}

/// `MD_UUID` from `mdadm --detail --export`.
pub fn md_uuid_from_export(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix("MD_UUID="))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn first_line(s: &str) -> Option<String> {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// Translates a mount unit start/stop into a plain `mount`/`umount` when systemd is not PID 1
/// (containers without systemd, recovery shells).
fn mount_argv<M: Machine>(m: &M, argv: &[String], mountpoint: &str) -> Vec<String> {
    let is_mount_unit = argv.len() == 3
        && argv[0] == "systemctl"
        && (argv[1] == "start" || argv[1] == "stop")
        && argv[2].ends_with(".mount");
    if is_mount_unit && !m.systemd() {
        let verb = if argv[1] == "start" {
            "mount"
        } else {
            "umount"
        };
        return strs(&[verb, mountpoint]);
    }
    argv.to_vec()
}

async fn run_cmd<M: Machine>(
    m: &M,
    argv: Vec<String>,
    stdin: Option<Vec<u8>>,
    quiet: bool,
    cancellable: bool,
) -> Result<Output, String> {
    let mut o = RunOpts::new(timeout_for(&argv));
    o.stdin = stdin;
    o.quiet = quiet;
    o.cancellable = cancellable;
    m.run(argv, o).await
}

// ------------------------------------------------------------------------------- undo

/// What creating a volume has done so far, undone in reverse order on failure.
#[derive(Debug, Clone, PartialEq)]
enum Undo {
    /// Partitions written on a disk (dev path, first partition path).
    Disk(String, String),
    Array {
        md: String,
        parts: Vec<String>,
    },
    Mapper(String),
    File(String),
    Dir(String),
    Mount {
        unit: String,
        mountpoint: String,
    },
    Config(String),
}

async fn undo_one<M: Machine>(m: &M, u: &Undo) -> Result<String, String> {
    let quiet_ok = |r: Result<Output, String>| r.map(|_| ());
    match u {
        Undo::Mount { unit, mountpoint } => {
            let argv = mount_argv(m, &strs(&["systemctl", "stop", unit]), mountpoint);
            quiet_ok(run_cmd(m, argv, None, false, false).await)?;
            Ok(format!("unmounted {mountpoint}"))
        }
        Undo::Dir(d) => {
            quiet_ok(run_cmd(m, strs(&["rmdir", d]), None, false, false).await)?;
            Ok(format!("removed {d}"))
        }
        Undo::Config(label) => {
            m.commit(VolumeChange::Remove(label.clone())).await?;
            Ok(format!(
                "removed {label} from fstab, crypttab and mdadm.conf"
            ))
        }
        Undo::Mapper(name) => {
            quiet_ok(run_cmd(m, strs(&["cryptsetup", "close", name]), None, false, false).await)?;
            Ok(format!("closed LUKS mapping {name}"))
        }
        Undo::File(f) => {
            m.remove_file(f);
            Ok(format!("removed {f}"))
        }
        Undo::Array { md, parts } => {
            quiet_ok(run_cmd(m, strs(&["mdadm", "--stop", md]), None, false, false).await)?;
            for p in parts {
                quiet_ok(
                    run_cmd(
                        m,
                        strs(&["mdadm", "--zero-superblock", p]),
                        None,
                        false,
                        false,
                    )
                    .await,
                )?;
            }
            Ok(format!("stopped {md} and cleared its member superblocks"))
        }
        Undo::Disk(dev, part) => {
            if m.exists(part) {
                // signatures inside the partition (md superblock), then the table itself
                let _ = run_cmd(m, strs(&["wipefs", "--all", part]), None, false, false).await;
            }
            quiet_ok(run_cmd(m, strs(&["sgdisk", "--zap-all", dev]), None, false, false).await)?;
            quiet_ok(run_cmd(m, strs(&["wipefs", "--all", dev]), None, false, false).await)?;
            Ok(format!("removed the partitions of {dev}"))
        }
    }
}

/// Runs the undo list in reverse; returns (done, failed) descriptions.
async fn rollback<M: Machine>(m: &M, undo: &[Undo]) -> (Vec<String>, Vec<String>) {
    m.log("rolling back");
    let (mut done, mut failed) = (Vec::new(), Vec::new());
    for u in undo.iter().rev() {
        match undo_one(m, u).await {
            Ok(d) => {
                m.log(&format!("rollback: {d}"));
                done.push(d);
            }
            Err(e) => {
                let d = format!("{u:?}: {e}");
                m.log(&format!("rollback failed: {d}"));
                failed.push(d);
            }
        }
    }
    (done, failed)
}

fn report(
    step: usize,
    total: usize,
    desc: &str,
    err: &str,
    done: &[String],
    failed: &[String],
) -> String {
    let mut r = format!("step {step}/{total} ({desc}) failed: {err}");
    if !done.is_empty() {
        r.push_str(&format!("; rolled back: {}", done.join(", ")));
    }
    if !failed.is_empty() {
        r.push_str(&format!(
            "; NOT rolled back, check by hand: {}",
            failed.join(", ")
        ));
    }
    if done.is_empty() && failed.is_empty() {
        r.push_str("; nothing had been changed");
    }
    r
}

fn summary<'a>(plan: &'a StoragePlan, key: &str) -> Option<&'a str> {
    plan.summary
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

// ------------------------------------------------------------------------------- create

/// What a new volume leaves to its creator.
#[derive(Clone, PartialEq, Default)]
pub struct CreateOutcome {
    pub record: VolumeRecord,
    /// Shown once to the user (encrypted volumes).
    pub recovery_key: Option<String>,
    /// LUKS header backup waiting for its download (encrypted volumes).
    pub header_backup: Option<String>,
}

impl std::fmt::Debug for CreateOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreateOutcome")
            .field("record", &self.record)
            .field(
                "recovery_key",
                &self.recovery_key.as_ref().map(|_| "<secret>"),
            )
            .field("header_backup", &self.header_backup)
            .finish()
    }
}

/// Runs a `create_volume` plan. On failure or cancellation everything done so far is undone
/// and the error is the report.
pub async fn create_volume<M: Machine>(
    m: &M,
    inv: &Inventory,
    plan: &StoragePlan,
    level: RaidLevel,
    disks: &[String],
    secrets: &Secrets,
    now_ms: u64,
) -> Result<CreateOutcome, String> {
    if !plan.valid || plan.operation != "create_volume" {
        return Err(format!(
            "not a valid volume plan: {}",
            plan.errors.join("; ")
        ));
    }
    let (Some(md), Some(fs_dev), Some(mountpoint)) = (
        summary(plan, "md"),
        summary(plan, "filesystem_device"),
        summary(plan, "mountpoint"),
    ) else {
        return Err("the plan has no md/filesystem/mountpoint summary".into());
    };
    let label = mountpoint.trim_start_matches('/').to_string();
    let encrypted = summary(plan, "mapper").is_some();
    if encrypted && !secrets.has_passphrase() {
        return Err("an encrypted volume needs a passphrase".into());
    }
    check_free(m, inv, disks)?;

    let options = plan
        .steps
        .iter()
        .find_map(|s| match &s.action {
            PlanAction::Config { file, entry } if file == "/etc/fstab" => {
                entry.split_whitespace().nth_back(2).map(|o| {
                    o.split(',')
                        .filter(|x| *x != "nofail" && *x != "noauto")
                        .collect::<Vec<_>>()
                        .join(",")
                })
            }
            _ => None,
        })
        .unwrap_or_else(|| format!("noatime,compress={COMPRESSION}"));
    let mut out = CreateOutcome {
        record: VolumeRecord {
            label: label.clone(),
            level,
            mountpoint: mountpoint.to_string(),
            options,
            created_ms: now_ms,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut undo: Vec<Undo> = Vec::new();
    let total = plan.steps.len();
    async fn fail_at<M: Machine>(
        m: &M,
        (i, total): (usize, usize),
        desc: &str,
        e: String,
        undo: Vec<Undo>,
    ) -> Result<CreateOutcome, String> {
        m.log(&format!("FAILED: {e}"));
        let (done, failed) = rollback(m, &undo).await;
        Err(report(i + 1, total, desc, &e, &done, &failed))
    }
    macro_rules! fail {
        ($i:expr, $desc:expr, $e:expr, $undo:expr) => {
            fail_at(m, ($i, total), $desc, $e, $undo)
        };
    }

    for (i, step) in plan.steps.iter().enumerate() {
        if m.is_cancelled() {
            return fail!(i, &step.description, "cancelled".into(), undo).await;
        }
        m.progress((i * 100 / total) as u8);
        m.log(&format!("[{}/{total}] {}", i + 1, step.description));
        match &step.action {
            PlanAction::Config { file, .. } => {
                if file == "/etc/fstab" {
                    if let Err(e) = m.commit(VolumeChange::Add(out.record.clone())).await {
                        return fail!(i, &step.description, e, undo).await;
                    }
                    undo.push(Undo::Config(label.clone()));
                } else {
                    m.log(&format!("{file}: written with the volume's entries"));
                }
                continue;
            }
            PlanAction::WriteSysfs { path, value } => {
                if let Err(e) = m.write_file(path, value) {
                    return fail!(i, &step.description, e, undo).await;
                }
                continue;
            }
            PlanAction::Wait { condition } => {
                if let Err(e) = wait_idle(m, condition, false, i, total).await {
                    return fail!(i, &step.description, e, undo).await;
                }
                continue;
            }
            PlanAction::Command { .. } => {}
        }
        let (argv, stdin_name) = argv_of(step).expect("a command");
        let prog = argv[0].as_str();
        let sub = argv.get(1).map(String::as_str).unwrap_or("");
        // undo entries that must exist before the command runs
        if prog == "wipefs" && !step.destroys.is_empty() {
            let disk = &step.destroys[0];
            if !undo
                .iter()
                .any(|u| matches!(u, Undo::Disk(d, _) if d == &format!("/dev/{disk}")))
            {
                undo.push(Undo::Disk(
                    format!("/dev/{disk}"),
                    format!("/dev/{}", crate::plan::part_name(disk, 1)),
                ));
            }
        }
        if prog == "cryptsetup"
            && sub == "luksHeaderBackup"
            && let Some(f) = argv
                .iter()
                .skip_while(|a| *a != "--header-backup-file")
                .nth(1)
        {
            m.remove_file(f);
        }
        let mkdir_new = prog == "mkdir" && !m.exists(argv.last().map(String::as_str).unwrap_or(""));
        let stdin = match stdin_name.map(|n| secrets.get(n)).transpose() {
            Ok(s) => s,
            Err(e) => return fail!(i, &step.description, e, undo).await,
        };
        let quiet = prog == "systemd-cryptenroll";
        let argv_run = mount_argv(m, argv, mountpoint);
        let res = run_cmd(m, argv_run, stdin, quiet, true).await;
        let output = match res {
            Ok(o) => o,
            Err(e) if prog == "udevadm" => {
                // no udev daemon (containers): the nodes are created by the kernel anyway
                m.log(&format!("udevadm settle: {e} (ignored)"));
                Output::default()
            }
            Err(e) => return fail!(i, &step.description, e, undo).await,
        };
        // captures and undo entries after success
        let captured: Result<(), String> = async {
            match (prog, sub) {
                ("mdadm", "--create") => {
                    let parts: Vec<String> = argv
                        .iter()
                        .skip(3)
                        .filter(|a| a.starts_with("/dev/"))
                        .cloned()
                        .collect();
                    undo.push(Undo::Array {
                        md: md.to_string(),
                        parts,
                    });
                    let o = run_cmd(
                        m,
                        strs(&["mdadm", "--detail", "--export", md]),
                        None,
                        true,
                        true,
                    )
                    .await?;
                    out.record.md_uuid = md_uuid_from_export(&o.stdout)
                        .ok_or("mdadm --detail --export printed no MD_UUID")?;
                    m.log(&format!("md array UUID {}", out.record.md_uuid));
                }
                ("cryptsetup", "luksFormat") => {
                    let o =
                        run_cmd(m, strs(&["cryptsetup", "luksUUID", md]), None, true, true).await?;
                    out.record.luks_uuid =
                        Some(first_line(&o.stdout).ok_or("cryptsetup luksUUID printed nothing")?);
                }
                ("systemd-cryptenroll", _) => {
                    let text = format!("{}\n{}", output.stdout, output.stderr);
                    out.recovery_key = Some(
                        find_recovery_key(&text)
                            .ok_or("systemd-cryptenroll printed no recovery key")?,
                    );
                    m.log("recovery key created (shown once in the interface, not logged)");
                }
                ("cryptsetup", "luksHeaderBackup") => {
                    let f = argv
                        .iter()
                        .skip_while(|a| *a != "--header-backup-file")
                        .nth(1)
                        .cloned()
                        .unwrap_or_default();
                    undo.push(Undo::File(f.clone()));
                    out.header_backup = Some(f);
                }
                ("cryptsetup", "open") => {
                    if let Some(name) = argv.last() {
                        undo.push(Undo::Mapper(name.clone()));
                    }
                }
                ("mkfs.btrfs", _) => {
                    let o = run_cmd(
                        m,
                        strs(&[
                            "blkid",
                            "--probe",
                            "--match-tag",
                            "UUID",
                            "--output",
                            "value",
                            fs_dev,
                        ]),
                        None,
                        true,
                        true,
                    )
                    .await?;
                    out.record.fs_uuid = first_line(&o.stdout).ok_or("blkid printed no UUID")?;
                    m.log(&format!("btrfs UUID {}", out.record.fs_uuid));
                }
                ("mkdir", _) if mkdir_new => {
                    undo.push(Undo::Dir(argv.last().cloned().unwrap_or_default()));
                }
                ("systemctl", "start") => {
                    undo.push(Undo::Mount {
                        unit: argv[2].clone(),
                        mountpoint: mountpoint.to_string(),
                    });
                }
                _ => {}
            }
            Ok(())
        }
        .await;
        if let Err(e) = captured {
            return fail!(i, &step.description, e, undo).await;
        }
    }
    m.progress(100);
    m.log(&format!(
        "volume {label} created and mounted at {mountpoint}"
    ));
    Ok(out)
}

// ------------------------------------------------------------------------------- other plans

/// Waits for `"<sysfs path> == <value>"` (md `sync_action == idle`), reporting the md
/// progress. With `clean`, also waits for `degraded` to drop to 0 (rebuilds).
async fn wait_idle<M: Machine>(
    m: &M,
    condition: &str,
    clean: bool,
    step: usize,
    total: usize,
) -> Result<(), String> {
    let (path, want) = condition
        .split_once(" == ")
        .map(|(p, v)| (p.trim(), v.trim()))
        .ok_or_else(|| format!("cannot wait for {condition:?}"))?;
    let dir = path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    // md starts the resync/rebuild asynchronously: give it a moment
    m.sleep(Duration::from_secs(3)).await;
    let mut last_pct = None;
    loop {
        if m.is_cancelled() {
            return Err("cancelled while waiting: the md operation goes on in the kernel".into());
        }
        let now = m.read_file(path).map(|s| s.trim().to_string());
        let degraded = m
            .read_file(&format!("{dir}/degraded"))
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0);
        match now.as_deref() {
            None => return Err(format!("{path} is gone")),
            Some(v) if v == want && (!clean || degraded == 0) => return Ok(()),
            Some(v) => {
                let pct = m.read_file(&format!("{dir}/sync_completed")).and_then(|s| {
                    let (a, b) = s.trim().split_once('/')?;
                    let (a, b) = (a.trim().parse::<f64>().ok()?, b.trim().parse::<f64>().ok()?);
                    (b > 0.0).then(|| (a / b * 100.0) as u8)
                });
                if pct != last_pct {
                    if let Some(p) = pct {
                        m.log(&format!("{v}: {p}%"));
                        m.progress(
                            ((step as f64 + f64::from(p) / 100.0) * 100.0 / total as f64) as u8,
                        );
                    }
                    last_pct = pct;
                }
            }
        }
        m.sleep(Duration::from_secs(5)).await;
    }
}

/// Runs a `grow_array`, `replace_disk` or `scrub` plan. Before the first step that touches
/// the array, a failure undoes the preparation of the new disks; after it the kernel owns
/// the operation and nothing is undone (the report says so).
pub async fn run_plan<M: Machine>(
    m: &M,
    inv: &Inventory,
    plan: &StoragePlan,
    new_disks: &[String],
    secrets: &Secrets,
) -> Result<(), String> {
    if !plan.valid {
        return Err(format!("not a valid plan: {}", plan.errors.join("; ")));
    }
    check_free(m, inv, new_disks)?;
    let scrub = plan.operation == "scrub";
    let clean = matches!(plan.operation.as_str(), "grow_array" | "replace_disk");
    let mountpoint = summary(plan, "mountpoint").unwrap_or("");
    let total = plan.steps.len();
    let mut undo: Vec<Undo> = Vec::new();
    let mut touched_array = false;
    let mut sysfs_written: Vec<String> = Vec::new();

    let mut failure: Option<(usize, String)> = None;
    for (i, step) in plan.steps.iter().enumerate() {
        if m.is_cancelled() {
            failure = Some((i, "cancelled".into()));
            break;
        }
        m.progress((i * 100 / total) as u8);
        m.log(&format!("[{}/{total}] {}", i + 1, step.description));
        let r: Result<(), String> = match &step.action {
            PlanAction::Config { file, .. } => {
                m.log(&format!("{file}: no change"));
                Ok(())
            }
            PlanAction::WriteSysfs { path, value } => {
                touched_array = true;
                let r = m.write_file(path, value);
                if r.is_ok() {
                    sysfs_written.push(path.clone());
                }
                r
            }
            PlanAction::Wait { condition } => {
                let r = wait_idle(m, condition, clean, i, total).await;
                if r.is_ok() && scrub {
                    let dir = condition.split(" == ").next().unwrap_or("");
                    if let Some((d, _)) = dir.rsplit_once('/')
                        && let Some(n) = m.read_file(&format!("{d}/mismatch_cnt"))
                    {
                        let n = n.trim();
                        m.log(&format!("{d}: mismatch_cnt {n}"));
                        if n != "0" {
                            m.log("WARNING: the md check found mismatches between members");
                        }
                    }
                }
                r
            }
            PlanAction::Command { argv, stdin } => {
                let prog = argv[0].as_str();
                if prog == "mdadm"
                    || (prog == "cryptsetup" && argv.get(1).is_some_and(|s| s == "resize"))
                {
                    touched_array = true;
                }
                if prog == "wipefs" && !step.destroys.is_empty() {
                    let disk = &step.destroys[0];
                    undo.push(Undo::Disk(
                        format!("/dev/{disk}"),
                        format!("/dev/{}", crate::plan::part_name(disk, 1)),
                    ));
                }
                match stdin.as_deref().map(|n| secrets.get(n)).transpose() {
                    Err(e) => Err(e),
                    Ok(stdin) => {
                        let argv = mount_argv(m, argv, mountpoint);
                        match run_cmd(m, argv, stdin, false, true).await {
                            Err(e) if prog == "udevadm" => {
                                m.log(&format!("udevadm settle: {e} (ignored)"));
                                Ok(())
                            }
                            r => r.map(|_| ()),
                        }
                    }
                }
            }
        };
        if let Err(e) = r {
            failure = Some((i, e));
            break;
        }
    }
    let Some((i, err)) = failure else {
        m.progress(100);
        return Ok(());
    };
    m.log(&format!("FAILED: {err}"));
    let desc = plan
        .steps
        .get(i)
        .map(|s| s.description.as_str())
        .unwrap_or("");
    if scrub {
        // stop what may still run in the kernel
        if !mountpoint.is_empty() {
            let _ = run_cmd(
                m,
                strs(&["btrfs", "scrub", "cancel", mountpoint]),
                None,
                false,
                false,
            )
            .await;
        }
        for p in &sysfs_written {
            let _ = m.write_file(p, "idle");
        }
        return Err(format!(
            "step {}/{total} ({desc}) failed: {err}; scrub and md check stopped",
            i + 1
        ));
    }
    if touched_array {
        return Err(format!(
            "step {}/{total} ({desc}) failed: {err}; the array had already been changed, nothing was rolled back: check its state",
            i + 1
        ));
    }
    let (done, failed) = rollback(m, &undo).await;
    Err(report(i + 1, total, desc, &err, &done, &failed))
}

// ------------------------------------------------------------------------------- unlock/lock

/// What unlocking or locking a volume works on.
#[derive(Debug, Clone, PartialEq)]
pub struct LuksVolume {
    pub label: String,
    /// The LUKS container, e.g. `/dev/md/vol1`.
    pub device: String,
    pub mapper: String,
    pub mountpoint: String,
}

/// Units reloaded after a volume appears or before it goes, so shares follow it.
const SHARE_UNITS: [&str; 2] = ["smbd.service", "nfs-server.service"];

/// Opens the LUKS container (passphrase or recovery key on stdin), mounts the volume through
/// its mount unit (units that need it follow) and reloads the file services.
pub async fn unlock<M: Machine>(m: &M, v: &LuksVolume, secrets: &Secrets) -> Result<(), String> {
    let unit = mount_unit(&v.mountpoint);
    let mapper_dev = format!("/dev/mapper/{}", v.mapper);
    let opened = if m.exists(&mapper_dev) {
        m.log(&format!("{} is already open", v.mapper));
        false
    } else {
        let pass = secrets.get("passphrase")?;
        run_cmd(
            m,
            strs(&[
                "cryptsetup",
                "open",
                "--key-file",
                "-",
                &v.device,
                &v.mapper,
            ]),
            Some(pass),
            false,
            true,
        )
        .await
        .map_err(|e| {
            if e.contains("exit code 2") {
                "wrong passphrase or recovery key".to_string()
            } else {
                e
            }
        })?;
        true
    };
    let argv = mount_argv(m, &strs(&["systemctl", "start", &unit]), &v.mountpoint);
    if let Err(e) = run_cmd(m, argv, None, false, false).await {
        if opened {
            let _ = run_cmd(
                m,
                strs(&["cryptsetup", "close", &v.mapper]),
                None,
                false,
                false,
            )
            .await;
            return Err(format!(
                "mounting {} failed: {e}; the volume was locked again",
                v.mountpoint
            ));
        }
        return Err(format!("mounting {} failed: {e}", v.mountpoint));
    }
    if m.systemd() {
        let mut argv = strs(&["systemctl", "try-reload-or-restart"]);
        argv.extend(SHARE_UNITS.iter().map(|s| s.to_string()));
        if let Err(e) = run_cmd(m, argv, None, false, false).await {
            m.log(&format!("reloading the file services: {e}"));
        }
    }
    m.log(&format!(
        "volume {} unlocked and mounted at {}",
        v.label, v.mountpoint
    ));
    Ok(())
}

/// Unmounts the volume (its mount unit, so units that need it stop first) and closes LUKS.
pub async fn lock<M: Machine>(m: &M, v: &LuksVolume) -> Result<(), String> {
    let unit = mount_unit(&v.mountpoint);
    if m.systemd() {
        // file services let go of the volume's files
        let mut argv = strs(&["systemctl", "try-reload-or-restart"]);
        argv.extend(SHARE_UNITS.iter().map(|s| s.to_string()));
        let _ = run_cmd(m, argv, None, false, false).await;
    }
    let argv = mount_argv(m, &strs(&["systemctl", "stop", &unit]), &v.mountpoint);
    run_cmd(m, argv, None, false, false).await.map_err(|e| {
        format!(
            "unmounting {} failed (files still open?): {e}",
            v.mountpoint
        )
    })?;
    if m.exists(&format!("/dev/mapper/{}", v.mapper)) {
        run_cmd(
            m,
            strs(&["cryptsetup", "close", &v.mapper]),
            None,
            false,
            false,
        )
        .await?;
    }
    m.log(&format!("volume {} locked", v.label));
    Ok(())
}

/// Runs a list of commands in order (subvolume operations); stops at the first failure.
pub async fn run_commands<M: Machine>(m: &M, cmds: Vec<Vec<String>>) -> Result<(), String> {
    let total = cmds.len().max(1);
    for (i, argv) in cmds.into_iter().enumerate() {
        m.progress((i * 100 / total) as u8);
        run_cmd(m, argv, None, false, true).await?;
    }
    m.progress(100);
    Ok(())
}

// ------------------------------------------------------------------------------- test double

/// A machine that records what would run and answers from canned outputs: commands whose
/// line starts with a key of `fail` fail with that message.
#[derive(Default)]
pub struct FakeMachine {
    pub log: std::sync::Mutex<Vec<String>>,
    /// Command lines run, with `<stdin>` appended when a secret was fed.
    pub ran: std::sync::Mutex<Vec<String>>,
    pub outputs: std::collections::HashMap<String, Output>,
    pub fail: std::collections::HashMap<String, String>,
    pub files: std::sync::Mutex<std::collections::HashMap<String, String>>,
    pub busy: Vec<String>,
    pub commits: std::sync::Mutex<Vec<VolumeChange>>,
    pub commit_fails: bool,
    pub cancelled: std::sync::atomic::AtomicBool,
    /// Cancel when a command line starting with this runs.
    pub cancel_at: Option<String>,
    pub no_systemd: bool,
}

impl FakeMachine {
    pub fn output(&mut self, line: &str, stdout: &str) -> &mut Self {
        self.outputs.insert(
            line.into(),
            Output {
                stdout: stdout.into(),
                stderr: String::new(),
            },
        );
        self
    }

    pub fn ran(&self) -> Vec<String> {
        self.ran.lock().unwrap().clone()
    }

    pub fn logged(&self) -> String {
        self.log.lock().unwrap().join("\n")
    }
}

impl Machine for FakeMachine {
    fn log(&self, line: &str) {
        self.log.lock().unwrap().push(line.to_string());
    }

    fn progress(&self, _pct: u8) {}

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn run(
        &self,
        argv: Vec<String>,
        opts: RunOpts,
    ) -> impl Future<Output = Result<Output, String>> + Send {
        let line = argv.join(" ");
        self.ran.lock().unwrap().push(if opts.stdin.is_some() {
            format!("{line} <stdin>")
        } else {
            line.clone()
        });
        if !opts.quiet {
            self.log(&format!("$ {line}"));
        }
        if self
            .cancel_at
            .as_deref()
            .is_some_and(|c| line.starts_with(c))
        {
            self.cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let r = match self.fail.iter().find(|(k, _)| line.starts_with(k.as_str())) {
            Some((_, e)) => Err(e.clone()),
            None if opts.cancellable && self.is_cancelled() => Err("cancelled".to_string()),
            None => Ok(self.outputs.get(&line).cloned().unwrap_or_default()),
        };
        std::future::ready(r)
    }

    fn write_file(&self, path: &str, value: &str) -> Result<(), String> {
        self.ran
            .lock()
            .unwrap()
            .push(format!("echo {value} > {path}"));
        Ok(())
    }

    fn read_file(&self, path: &str) -> Option<String> {
        self.files.lock().unwrap().get(path).cloned()
    }

    fn exists(&self, path: &str) -> bool {
        self.files.lock().unwrap().contains_key(path)
    }

    fn remove_file(&self, path: &str) {
        self.ran.lock().unwrap().push(format!("rm -f {path}"));
    }

    fn claim_free(&self, dev: &str) -> Result<(), String> {
        if self.busy.iter().any(|b| b == dev) {
            Err(format!("{dev} is in use"))
        } else {
            Ok(())
        }
    }

    fn sleep(&self, _d: Duration) -> impl Future<Output = ()> + Send {
        std::future::ready(())
    }

    fn systemd(&self) -> bool {
        !self.no_systemd
    }

    fn commit(&self, change: VolumeChange) -> impl Future<Output = Result<(), String>> + Send {
        let r = if self.commit_fails {
            Err("configuration apply failed".to_string())
        } else {
            self.ran.lock().unwrap().push(format!("commit {change:?}"));
            self.commits.lock().unwrap().push(change);
            Ok(())
        };
        std::future::ready(r)
    }
}
