//! Snapshot replication: replica targets of a snapshot job, as btrbk `target` sections of the
//! job's own configuration.
//!
//! - one configuration per job keeps btrbk in charge of the chain: it picks the incremental
//!   parent (full send the first time), always keeps the latest snapshot/backup pair, and
//!   keeps every snapshot while a target cannot be reached;
//! - the snapshot run of a job with targets is `btrbk snapshot` (create, prune the source);
//!   a transfer is `btrbk clean <group>` (interrupted receives, btrbk-named only) then
//!   `btrbk resume --preserve-snapshots <group>` (missing replicas, target retention);
//! - btrbk deletes on a target only what matches its own names (`<job>.YYYYMMDDTHHMM`) in the
//!   target directory: nothing rynas did not create;
//! - remote targets use one dedicated key ([`KEY_PATH`]), rynas's own known_hosts filled with
//!   host keys the administrator confirmed, and on the remote side `ssh_filter_btrbk.sh`
//!   ([`crate::btrbk_ssh`]).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rynas_api_types::replication::{
    ReplicaDestination, ReplicaHint, ReplicaSnapshot, ReplicaStatus, ReplicaTarget,
    ReplicaWarning, StreamCompress,
};
use rynas_api_types::snapshots::{SnapshotJob, SnapshotsConfig};
use rynas_config::ManagedFile;
use rynas_tasks::Cmd;

use crate::btrbk::{clean_abs, conf_path, normalize_retention, parse_raw_line, preserve, valid_min};
use crate::btrbk_ssh::{self, FilterRole};
use crate::ioctl;
use crate::scan::{self, SnapError};

/// The replication key pair (private key; `.pub` beside it).
pub const KEY_PATH: &str = "/etc/rynas/ssh/replication_ed25519";
/// Host keys of the remote targets, confirmed in rynas.
pub const KNOWN_HOSTS: &str = "/etc/rynas/ssh/known_hosts";
/// ssh client settings for the remote targets (root's connections to those hosts only).
pub const SSH_CONFIG: &str = "/etc/ssh/ssh_config.d/60-rynas-replication.conf";
/// Longest transfer.
pub const TRANSFER_TIMEOUT: Duration = Duration::from_secs(24 * 3600);
/// Targets per job.
pub const MAX_TARGETS: usize = 8;

/// A target id: `[a-z0-9][a-z0-9-]{0,23}`.
pub fn valid_target_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 24
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// The btrbk group of a target (unique within its job's configuration).
pub fn group(id: &str) -> String {
    format!("replica-{id}")
}

/// The scheduler task of a target with its own schedule.
pub fn task_id(job: &str, id: &str) -> String {
    format!(
        "replica-{}-{id}",
        job.to_ascii_lowercase().replace('_', "-")
    )
}

fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && !h.starts_with('-')
        && !h.starts_with('.')
        && h
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b':')
}

fn valid_user(u: &str) -> bool {
    let b = u.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && (b[0].is_ascii_lowercase() || b[0] == b'_')
        && b
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

fn inside(path: &str, dir: &str) -> bool {
    path == dir || Path::new(path).starts_with(dir)
}

/// Validates and normalizes the targets of a (normalized) job.
pub fn normalize_targets(job: &SnapshotJob) -> Result<Vec<ReplicaTarget>, String> {
    let n = &job.name;
    if job.replicas.len() > MAX_TARGETS {
        return Err(format!("job {n}: at most {MAX_TARGETS} replica targets"));
    }
    let mut out: Vec<ReplicaTarget> = Vec::with_capacity(job.replicas.len());
    for t in &job.replicas {
        let mut t = t.clone();
        let id = t.id.clone();
        if !valid_target_id(&id) {
            return Err(format!(
                "job {n}: invalid replica target id {id:?} (1-24 lowercase letters, digits and '-')"
            ));
        }
        if out.iter().any(|x| x.id == id) {
            return Err(format!("job {n}: replica target {id} is defined twice"));
        }
        let path = t.destination.path().to_string();
        if !clean_abs(&path) || path == "/" {
            return Err(format!(
                "job {n}, replica {id}: the target must be an absolute directory below / (got {path:?})"
            ));
        }
        match &mut t.destination {
            ReplicaDestination::Local { path } => {
                if inside(path, &job.subvolume) || inside(&job.subvolume, path) {
                    return Err(format!(
                        "job {n}, replica {id}: {path} overlaps the subvolume {}",
                        job.subvolume
                    ));
                }
                if inside(path, &job.snapshot_dir) {
                    return Err(format!(
                        "job {n}, replica {id}: {path} is inside the snapshot directory"
                    ));
                }
                // the stream never leaves the machine: compression would only cost CPU
                t.compress = StreamCompress::No;
            }
            ReplicaDestination::Ssh {
                host,
                port,
                user,
                host_keys,
                ..
            } => {
                *host = host.trim().trim_start_matches('[').trim_end_matches(']').to_string();
                if !valid_host(host) {
                    return Err(format!("job {n}, replica {id}: invalid host {host:?}"));
                }
                if *port == 0 {
                    return Err(format!("job {n}, replica {id}: invalid port 0"));
                }
                if !valid_user(user) {
                    return Err(format!("job {n}, replica {id}: invalid user {user:?}"));
                }
                if host_keys.len() > 8 {
                    return Err(format!("job {n}, replica {id}: at most 8 host keys"));
                }
                for k in host_keys.iter_mut() {
                    *k = btrbk_ssh::known_hosts_line(k, host, *port).ok_or_else(|| {
                        format!("job {n}, replica {id}: invalid host key {k:?}")
                    })?;
                }
            }
        }
        if out
            .iter()
            .any(|x| x.destination.url() == t.destination.url())
        {
            return Err(format!(
                "job {n}, replica {id}: another target already uses {}",
                t.destination.url()
            ));
        }
        t.retention = normalize_retention(&t.retention);
        let r = &t.retention;
        if [r.hourly, r.daily, r.weekly, r.monthly, r.yearly]
            .iter()
            .any(|&c| c > 10_000)
        {
            return Err(format!("job {n}, replica {id}: retention counts are at most 10000"));
        }
        if !valid_min(&r.min) {
            return Err(format!(
                "job {n}, replica {id}: invalid minimum {:?} (all, latest, or a number followed by h, d, w, m or y)",
                r.min
            ));
        }
        if t.rate_limit_kib > 10_000_000 {
            return Err(format!("job {n}, replica {id}: bandwidth limit too high"));
        }
        out.push(t);
    }
    Ok(out)
}

/// The `target` sections of a job (indented under its `subvolume`); empty without enabled
/// targets. Disabled targets are left out: btrbk neither sends to nor prunes them.
pub fn render_targets(job: &SnapshotJob) -> String {
    let mut s = String::new();
    for t in job.replicas.iter().filter(|t| t.enabled) {
        let r = &t.retention;
        s.push_str(&format!(
            "\x20   target send-receive {url}\n\
             \x20     group {group}\n\
             \x20     target_preserve_min {min}\n\
             \x20     target_preserve {pre}\n",
            url = t.destination.url(),
            group = group(&t.id),
            min = r.min,
            pre = preserve(r),
        ));
        if t.rate_limit_kib > 0 {
            s.push_str(&format!("\x20     rate_limit {}k\n", t.rate_limit_kib));
        }
        if let ReplicaDestination::Ssh { user, sudo, .. } = &t.destination {
            s.push_str(&format!(
                "\x20     ssh_user {user}\n\x20     ssh_identity {KEY_PATH}\n"
            ));
            if t.compress != StreamCompress::No {
                s.push_str(&format!("\x20     stream_compress {}\n", t.compress.btrbk()));
            }
            if *sudo {
                s.push_str("\x20     backend_remote btrfs-progs-sudo\n");
            }
        }
    }
    s
}

/// The remote targets of a configuration: `(host, port, user, host keys)`.
fn remotes(c: &SnapshotsConfig) -> Vec<(&str, u16, &str, &[String])> {
    c.jobs
        .iter()
        .flat_map(|j| j.replicas.iter())
        .filter_map(|t| match &t.destination {
            ReplicaDestination::Ssh {
                host,
                port,
                user,
                host_keys,
                ..
            } => Some((host.as_str(), *port, user.as_str(), host_keys.as_slice())),
            ReplicaDestination::Local { .. } => None,
        })
        .collect()
}

/// rynas's known_hosts: the confirmed keys of every remote target.
pub fn known_hosts(c: &SnapshotsConfig) -> String {
    let mut lines: Vec<&str> = remotes(c)
        .into_iter()
        .flat_map(|(_, _, _, k)| k.iter().map(String::as_str))
        .collect();
    lines.sort_unstable();
    lines.dedup();
    let mut s = String::from("# Host keys of the rynas replica targets, confirmed in rynas.\n");
    for l in lines {
        s.push_str(l);
        s.push('\n');
    }
    s
}

/// ssh client settings applied to root's connections to the remote targets: rynas's
/// known_hosts, strict host key checking, no prompts, keepalives.
pub fn ssh_config(c: &SnapshotsConfig) -> String {
    let mut hosts: Vec<(&str, &str)> = remotes(c).into_iter().map(|(h, _, u, _)| (h, u)).collect();
    hosts.sort_unstable();
    hosts.dedup();
    let mut s = String::from(
        "# ssh settings of the rynas snapshot replication (root's connections to its targets).\n",
    );
    for (h, u) in hosts {
        s.push_str(&format!(
            "Match originalhost {h} user {u} localuser root\n\
             \x20   UserKnownHostsFile {KNOWN_HOSTS}\n\
             \x20   StrictHostKeyChecking yes\n\
             \x20   BatchMode yes\n\
             \x20   IdentitiesOnly yes\n\
             \x20   ConnectTimeout 20\n\
             \x20   ServerAliveInterval 30\n\
             \x20   ServerAliveCountMax 4\n"
        ));
    }
    s
}

/// The managed files of the remote targets (absent without any).
pub fn managed_files(c: &SnapshotsConfig) -> Vec<ManagedFile> {
    if remotes(c).is_empty() {
        return vec![ManagedFile::absent(KNOWN_HOSTS), ManagedFile::absent(SSH_CONFIG)];
    }
    vec![
        ManagedFile::whole(KNOWN_HOSTS, known_hosts(c), true).mode(0o644),
        ManagedFile::whole(SSH_CONFIG, ssh_config(c), true).mode(0o644),
    ]
}

/// The `authorized_keys` line for a remote target: the replication key forced into
/// `ssh_filter_btrbk.sh --target --delete --restrict-path <path>`.
pub fn authorized_keys_line(public_key: &str, t: &ReplicaTarget) -> String {
    match &t.destination {
        ReplicaDestination::Ssh { path, sudo, .. } if !public_key.is_empty() => {
            btrbk_ssh::authorized_keys_line(
                public_key,
                &btrbk_ssh::filter_command(FilterRole::Target, path, *sudo),
            )
        }
        _ => String::new(),
    }
}

/// The snapshot run of a job: `run` without targets (unchanged), `snapshot` with targets
/// (transfers are their own runs).
pub fn snapshot_cmd(job: &SnapshotJob, timeout: Duration) -> Cmd {
    let action = if job.replicas.iter().any(|t| t.enabled) {
        "snapshot"
    } else {
        "run"
    };
    Cmd::new("btrbk")
        .arg("-c")
        .arg(conf_path(&job.name))
        .args([action, "--format=raw"])
        .env_clear()
        .timeout(timeout)
}

/// `btrbk clean <group>`: deletes interrupted receives (btrbk-named, without
/// `received_uuid`) in the target directory, so the next send starts that replica again.
pub fn clean_cmd(job: &str, id: &str) -> Cmd {
    Cmd::new("btrbk")
        .arg("-c")
        .arg(conf_path(job))
        .args(["clean", "--format=raw"])
        .arg(group(id))
        .env_clear()
        .timeout(Duration::from_secs(3600))
}

/// `btrbk resume --preserve-snapshots <group>`: sends the missing replicas (incremental from
/// the latest common parent, full the first time) and applies the target retention; the
/// source snapshots are left to the job's own run.
pub fn resume_cmd(job: &str, id: &str) -> Cmd {
    Cmd::new("btrbk")
        .arg("-c")
        .arg(conf_path(job))
        .args(["resume", "--preserve-snapshots", "--format=raw"])
        .arg(group(id))
        .env_clear()
        .timeout(TRANSFER_TIMEOUT)
}

/// btrbk's exit code when another run holds the job's lock.
pub const EXIT_LOCKED: i32 = 3;

/// What a transfer reported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransferReport {
    /// Replicas received (names).
    pub sent: Vec<String>,
    /// Replicas deleted on the target (retention or interrupted receives).
    pub pruned: Vec<String>,
    /// Failed actions: `type target: message`.
    pub errors: Vec<String>,
    /// Sum of the send-receive durations, seconds.
    pub duration_s: u64,
}

/// Parses `--format=raw` output of `clean` / `resume`.
pub fn parse_transfer(out: &str) -> TransferReport {
    let mut r = TransferReport::default();
    for line in out.lines() {
        let f = parse_raw_line(line);
        if f.get("format").map(String::as_str) != Some("transaction") {
            continue;
        }
        let get = |k: &str| f.get(k).map_or("", String::as_str);
        let (t, status, target) = (get("type"), get("status"), get("target_url"));
        let name = target.rsplit('/').next().unwrap_or(target).to_string();
        let ok = matches!(status, "success" | "dryrun_success");
        match t {
            "startup" | "finished" => {}
            _ if !ok => r.errors.push(format!(
                "{t} {target}: {}",
                f.get("message")
                    .filter(|m| !m.is_empty())
                    .map_or(status, String::as_str)
            )),
            "send-receive" => {
                r.duration_s += get("duration").parse::<u64>().unwrap_or(0);
                r.sent.push(name);
            }
            t if t.starts_with("delete") => r.pruned.push(name),
            _ => {}
        }
    }
    r
}

/// The most likely fix for a failure, from btrbk's and ssh's words.
pub fn hint(text: &str) -> ReplicaHint {
    let t = text.to_ascii_lowercase();
    let has = |ws: &[&str]| ws.iter().any(|w| t.contains(w));
    if has(&["host key verification failed", "remote host identification has changed", "no ed25519 host key is known", "host key for"]) {
        ReplicaHint::HostKey
    } else if has(&["permission denied (publickey", "permission denied, please try again", "too many authentication failures"]) {
        ReplicaHint::SshKey
    } else if has(&["ssh_filter_btrbk", "access denied", "command not allowed"]) {
        ReplicaHint::Filter
    } else if has(&["could not resolve hostname", "connection refused", "connection timed out", "no route to host", "network is unreachable", "connection closed by", "operation timed out"]) {
        ReplicaHint::Unreachable
    } else if has(&["no space left"]) {
        ReplicaHint::NoSpace
    } else if has(&["not a btrfs", "not on btrfs", "is not a btrfs filesystem"]) {
        ReplicaHint::NotBtrfs
    } else if has(&["no such file or directory", "does not exist", "failed to fetch subvolume detail", "target directory"]) {
        ReplicaHint::TargetMissing
    } else if has(&["lockfile", "lock file"]) {
        ReplicaHint::Busy
    } else if has(&["btrbk: not found", "no such file or directory: btrbk", "unknown command"]) {
        ReplicaHint::Btrbk
    } else {
        ReplicaHint::Other
    }
}

// ---------------------------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------------------------

pub fn status_path(state_dir: &Path, job: &str, id: &str) -> PathBuf {
    state_dir.join("replication").join(job).join(format!("{id}.json"))
}

pub fn read_status(state_dir: &Path, job: &str, id: &str) -> ReplicaStatus {
    std::fs::read(status_path(state_dir, job, id))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Writes the status (temporary file then rename).
pub fn write_status(state_dir: &Path, job: &str, id: &str, s: &ReplicaStatus) -> std::io::Result<()> {
    let p = status_path(state_dir, job, id);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(s).map_err(std::io::Error::other)?)?;
    std::fs::rename(tmp, p)
}

/// The status after a run.
pub fn next_status(prev: &ReplicaStatus, now_ms: u64, r: Result<&TransferReport, &str>) -> ReplicaStatus {
    let mut s = prev.clone();
    s.last_attempt_ms = Some(now_ms);
    match r {
        Ok(rep) => {
            s.last_ok_ms = Some(now_ms);
            s.failing = false;
            s.duration_s = Some(rep.duration_s);
            s.transferred = rep.sent.clone();
            if let Some(n) = rep.sent.iter().max_by(|a, b| crate::name::compare_names(a, b)) {
                s.latest = Some(n.clone());
            }
            s.pruned = rep.pruned.len() as u32;
            s.error = None;
            s.hint = None;
        }
        Err(e) => {
            s.failing = true;
            s.error = Some(e.chars().take(2000).collect());
            s.hint = Some(hint(e));
        }
    }
    s
}

// ---------------------------------------------------------------------------------------------
// the machine, behind a trait (tests and --dry-system)
// ---------------------------------------------------------------------------------------------

/// The filesystem a path is on (the path or its nearest existing ancestor).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FsOf {
    pub btrfs: bool,
    /// btrfs filesystem UUID.
    pub uuid: Option<String>,
    /// The mount point holding it.
    pub mount: String,
}

pub trait ReplicaHost: Send + Sync {
    fn fs_of(&self, path: &str) -> Option<FsOf>;
    /// Creates a local target directory (mode 0700: replicas are not shared).
    fn ensure_dir(&self, path: &str) -> Result<(), String>;
    /// Creates the replication key pair when missing; returns the public key.
    fn ensure_key(&self) -> Result<String, String>;
    fn public_key(&self) -> Option<String>;
    /// `SHA256:` fingerprints of key lines, in order.
    fn fingerprints(&self, lines: &str) -> Vec<String>;
    /// The host keys a host presents (`ssh-keyscan`).
    fn scan_host(&self, host: &str, port: u16) -> Result<Vec<String>, String>;
}

/// The real machine.
pub struct HostReplicas;

fn nearest_existing(path: &str) -> PathBuf {
    let mut p = PathBuf::from(path);
    while !p.exists() {
        if !p.pop() {
            return PathBuf::from("/");
        }
    }
    p
}

impl ReplicaHost for HostReplicas {
    fn fs_of(&self, path: &str) -> Option<FsOf> {
        let p = nearest_existing(path);
        let f = ioctl::open_dir(&p).ok()?;
        let btrfs = ioctl::is_btrfs(&f);
        let mut m = p.clone();
        while !ioctl::is_mount_point(&m) && m.pop() {}
        Some(FsOf {
            btrfs,
            uuid: if btrfs { ioctl::fs_uuid(&f).ok() } else { None },
            mount: m.display().to_string(),
        })
    }

    fn ensure_dir(&self, path: &str) -> Result<(), String> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|e| format!("{path}: {e}"))
    }

    fn ensure_key(&self) -> Result<String, String> {
        let host = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
        btrbk_ssh::ensure_keypair(
            Path::new(KEY_PATH),
            &format!("rynas-replication@{}", host.trim()),
        )
    }

    fn public_key(&self) -> Option<String> {
        btrbk_ssh::public_key(Path::new(KEY_PATH))
    }

    fn fingerprints(&self, lines: &str) -> Vec<String> {
        btrbk_ssh::fingerprint_cmd(lines)
            .run_blocking()
            .map(|o| btrbk_ssh::parse_fingerprints(&o.stdout))
            .unwrap_or_default()
    }

    fn scan_host(&self, host: &str, port: u16) -> Result<Vec<String>, String> {
        let o = btrbk_ssh::keyscan_cmd(host, port)
            .output_blocking()
            .map_err(|e| format!("ssh-keyscan: {e}"))?;
        let keys = btrbk_ssh::parse_keyscan(&o.stdout);
        if keys.is_empty() {
            return Err(format!("{host}:{port} answered no SSH host key"));
        }
        Ok(keys)
    }
}

/// Tests and `--dry-system`: filesystems declared by path prefix, a fixed key, no network.
#[derive(Default)]
pub struct MemoryReplicas {
    pub state: Mutex<MemoryReplicasState>,
}

#[derive(Default, Debug)]
pub struct MemoryReplicasState {
    /// `(path prefix, filesystem)`; the longest matching prefix wins; none: `/`, not btrfs.
    pub filesystems: Vec<(String, FsOf)>,
    pub dirs: Vec<String>,
    pub key: Option<String>,
    /// Keys answered by `scan_host`.
    pub host_keys: Vec<String>,
}

impl MemoryReplicas {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares a btrfs filesystem mounted at `mount`.
    pub fn add_btrfs(&self, mount: &str, uuid: &str) {
        self.state.lock().unwrap().filesystems.push((
            mount.into(),
            FsOf {
                btrfs: true,
                uuid: Some(uuid.into()),
                mount: mount.into(),
            },
        ));
    }
}

impl ReplicaHost for MemoryReplicas {
    fn fs_of(&self, path: &str) -> Option<FsOf> {
        let st = self.state.lock().unwrap();
        st.filesystems
            .iter()
            .filter(|(p, _)| inside(path, p))
            .max_by_key(|(p, _)| p.len())
            .map(|(_, f)| f.clone())
            .or_else(|| {
                Some(FsOf {
                    btrfs: false,
                    uuid: None,
                    mount: "/".into(),
                })
            })
    }

    fn ensure_dir(&self, path: &str) -> Result<(), String> {
        tracing::info!("dry system: mkdir -p -m 0700 {path}");
        self.state.lock().unwrap().dirs.push(path.into());
        Ok(())
    }

    fn ensure_key(&self) -> Result<String, String> {
        let mut st = self.state.lock().unwrap();
        Ok(st
            .key
            .get_or_insert_with(|| {
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDryRynasDrySystemKeyOnly0000000000000000 rynas-replication@nas".into()
            })
            .clone())
    }

    fn public_key(&self) -> Option<String> {
        self.state.lock().unwrap().key.clone()
    }

    fn fingerprints(&self, lines: &str) -> Vec<String> {
        lines
            .lines()
            .filter(|l| !l.trim().is_empty())
            .enumerate()
            .map(|(i, _)| format!("SHA256:dry{i}"))
            .collect()
    }

    fn scan_host(&self, host: &str, port: u16) -> Result<Vec<String>, String> {
        let st = self.state.lock().unwrap();
        if st.host_keys.is_empty() {
            return Err(format!("{host}:{port} answered no SSH host key"));
        }
        Ok(st.host_keys.clone())
    }
}

/// Problems of a target found without connecting.
pub fn warnings(host: &dyn ReplicaHost, job: &SnapshotJob, t: &ReplicaTarget) -> Vec<ReplicaWarning> {
    let mut w = Vec::new();
    match &t.destination {
        ReplicaDestination::Local { path } => {
            let target = host.fs_of(path);
            let source = host.fs_of(&job.subvolume);
            match &target {
                Some(f) if !f.btrfs => w.push(ReplicaWarning::NotBtrfs),
                _ => {}
            }
            if let (Some(a), Some(b)) = (&target, &source)
                && a.uuid.is_some()
                && a.uuid == b.uuid
            {
                w.push(ReplicaWarning::SameFilesystem);
            }
            if target.as_ref().is_some_and(|f| f.mount == "/") && !Path::new(path).exists() {
                w.push(ReplicaWarning::NotMounted);
            }
        }
        ReplicaDestination::Ssh { host_keys, .. } => {
            if host_keys.is_empty() {
                w.push(ReplicaWarning::NoHostKey);
            }
        }
    }
    w
}

/// The replicas in a local target, newest first.
pub fn scan_replicas(dir: &Path, prefix: &str) -> std::io::Result<Vec<ReplicaSnapshot>> {
    Ok(scan::scan(dir, prefix)?
        .into_iter()
        .map(|s| {
            let readonly = s.info.as_ref().is_some_and(|i| i.readonly);
            let received_uuid = s.info.as_ref().and_then(|i| i.received_uuid.clone());
            ReplicaSnapshot {
                incomplete: !readonly || received_uuid.is_none(),
                name: s.name,
                path: s.path.display().to_string(),
                created_ms: s.created_ms.max(0) as u64,
                readonly,
                received_uuid,
            }
        })
        .collect())
}

/// A folder name for a promoted replica: a plain top-level name of a volume.
pub fn valid_folder(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && !n.starts_with(['.', '#', '@', '-'])
        && !n
            .chars()
            .any(|c| c.is_control() || "/\\\"'#%$*?<>|:".contains(c))
}

/// Promotes a replica: a writable snapshot of it as `<volume>/<folder>`. The replica stays
/// read-only (making a received subvolume writable would break later incremental receives).
pub fn promote(replica: &Path, volume: &Path, folder: &str) -> Result<PathBuf, SnapError> {
    if !valid_folder(folder) {
        return Err(SnapError::Invalid(format!("invalid folder name {folder:?}")));
    }
    let src = ioctl::open_dir(replica)
        .map_err(|e| SnapError::Failed(format!("{}: {e}", replica.display())))?;
    if !ioctl::is_subvolume(&src) {
        return Err(SnapError::Invalid(format!("{} is not a subvolume", replica.display())));
    }
    let info = ioctl::subvol_info(&src)
        .map_err(|e| SnapError::Failed(format!("{}: {e}", replica.display())))?;
    if !info.readonly || info.received_uuid.is_none() {
        return Err(SnapError::Invalid(format!(
            "{} is an interrupted receive, not a complete replica",
            replica.display()
        )));
    }
    let dest = ioctl::open_dir(volume)
        .map_err(|e| SnapError::Failed(format!("{}: {e}", volume.display())))?;
    let target = volume.join(folder);
    if target.symlink_metadata().is_ok() {
        return Err(SnapError::Invalid(format!("{} already exists", target.display())));
    }
    ioctl::create_snapshot(&src, &dest, folder, false)
        .map_err(|e| SnapError::Failed(format!("snapshot to {}: {e}", target.display())))?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::replication::default_target_retention;
    use rynas_api_types::snapshots::RetentionPreset;

    fn target(id: &str, d: ReplicaDestination) -> ReplicaTarget {
        ReplicaTarget {
            id: id.into(),
            enabled: true,
            destination: d,
            retention: default_target_retention(),
            compress: StreamCompress::Zstd,
            rate_limit_kib: 0,
            after_snapshots: true,
        }
    }

    fn job(replicas: Vec<ReplicaTarget>) -> SnapshotJob {
        SnapshotJob {
            name: "docs".into(),
            share: Some("docs".into()),
            subvolume: "/srv/vol1/docs".into(),
            snapshot_dir: "/srv/vol1/.snapshots/docs".into(),
            retention: RetentionPreset::Standard.retention(),
            replicas,
        }
    }

    fn remote() -> ReplicaDestination {
        ReplicaDestination::Ssh {
            host: "backup.lan".into(),
            port: 2222,
            user: "btrbk".into(),
            path: "/vol1/replica/docs".into(),
            sudo: true,
            host_keys: vec!["backup.lan ssh-ed25519 AAAAC3NzaC1lZDI1NTE5".into()],
        }
    }

    #[test]
    fn validation() {
        let local = |p: &str| ReplicaDestination::Local { path: p.into() };
        let ok = normalize_targets(&job(vec![target("hdd", local("/srv/vol2/.replicas/docs")), target("off", remote())])).unwrap();
        assert_eq!(ok[0].compress, StreamCompress::No, "no compression on a local target");
        assert_eq!(
            ok[1].destination,
            ReplicaDestination::Ssh {
                host: "backup.lan".into(),
                port: 2222,
                user: "btrbk".into(),
                path: "/vol1/replica/docs".into(),
                sudo: true,
                host_keys: vec!["[backup.lan]:2222 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5".into()],
            }
        );
        for (bad, why) in [
            (vec![target("HDD", local("/srv/vol2/r"))], "invalid replica target id"),
            (vec![target("a", local("/srv/vol1/docs/r"))], "overlaps"),
            (vec![target("a", local("/srv/vol1"))], "overlaps"),
            (vec![target("a", local("/srv/vol1/.snapshots/docs/x"))], "snapshot directory"),
            (vec![target("a", local("relative"))], "absolute"),
            (vec![target("a", local("/srv/v2")), target("a", local("/srv/v3"))], "twice"),
            (vec![target("a", local("/srv/v2")), target("b", local("/srv/v2"))], "already uses"),
        ] {
            let e = normalize_targets(&job(bad)).unwrap_err();
            assert!(e.contains(why), "{e} / {why}");
        }
        let mut t = target("r", remote());
        if let ReplicaDestination::Ssh { host, .. } = &mut t.destination {
            *host = "-oProxyCommand=x".into();
        }
        assert!(normalize_targets(&job(vec![t])).unwrap_err().contains("invalid host"));
        let mut t = target("r", remote());
        if let ReplicaDestination::Ssh { host_keys, .. } = &mut t.destination {
            host_keys.push("h evil\nkey AAAA".into());
        }
        assert!(normalize_targets(&job(vec![t])).is_err());
        let mut t = target("r", remote());
        t.retention.min = "forever".into();
        assert!(normalize_targets(&job(vec![t])).unwrap_err().contains("minimum"));
    }

    #[test]
    fn golden_configuration_with_targets() {
        let mut off = target("offsite", remote());
        off.rate_limit_kib = 10240;
        let mut disabled = target("old", ReplicaDestination::Local { path: "/srv/vol3/r".into() });
        disabled.enabled = false;
        let mut j = job(vec![target("hdd", ReplicaDestination::Local { path: "/srv/vol2/.replicas/docs".into() }), off, disabled]);
        j.replicas = normalize_targets(&j).unwrap();
        crate::tests::golden("docs-replicas.conf", &crate::btrbk::render_conf(&j).unwrap());
        assert_eq!(snapshot_cmd(&j, TRANSFER_TIMEOUT).display(), "btrbk -c /etc/rynas/btrbk/docs.conf snapshot --format=raw");
        assert_eq!(snapshot_cmd(&job(vec![]), TRANSFER_TIMEOUT).display(), "btrbk -c /etc/rynas/btrbk/docs.conf run --format=raw");
        assert_eq!(resume_cmd("docs", "hdd").display(), "btrbk -c /etc/rynas/btrbk/docs.conf resume --preserve-snapshots --format=raw replica-hdd");
        assert_eq!(clean_cmd("docs", "hdd").display(), "btrbk -c /etc/rynas/btrbk/docs.conf clean --format=raw replica-hdd");
        let c = SnapshotsConfig { jobs: vec![j.clone()], ..Default::default() };
        crate::tests::golden("replication-ssh.conf", &ssh_config(&c));
        assert_eq!(
            known_hosts(&c),
            "# Host keys of the rynas replica targets, confirmed in rynas.\n[backup.lan]:2222 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5\n"
        );
        let line = authorized_keys_line("ssh-ed25519 AAAA rynas-replication@nas", &j.replicas[1]);
        assert_eq!(line, "command=\"/usr/share/btrbk/scripts/ssh_filter_btrbk.sh --target --delete --restrict-path /vol1/replica/docs --sudo --log\",restrict ssh-ed25519 AAAA rynas-replication@nas");
        assert_eq!(authorized_keys_line("ssh-ed25519 AAAA", &j.replicas[0]), "");
        assert_eq!(task_id("My_Docs", "hdd"), "replica-my-docs-hdd");
        let none = SnapshotsConfig::default();
        assert!(managed_files(&none).iter().all(|f| f.kind == rynas_config::files::FileKind::Absent));
    }

    #[test]
    fn transfer_report_and_hints() {
        let out = "\
format=\"transaction\" time='1' localtime='x' type='startup' status='v0.32.7' duration='' target_url='' source_url='' parent_url='' message=''
format=\"transaction\" time='1' localtime='x' type='send-receive' status='success' duration='42' target_url='ssh://backup.lan:2222/vol1/replica/docs/docs.20261108T0100' source_url='/srv/vol1/.snapshots/docs/docs.20261108T0100' parent_url='/srv/vol1/.snapshots/docs/docs.20261107T0100' message=''
format=\"transaction\" time='1' localtime='x' type='delete_target' status='success' duration='' target_url='ssh://backup.lan:2222/vol1/replica/docs/docs.20250101T0100' source_url='' parent_url='' message=''
format=\"transaction\" time='1' localtime='x' type='send-receive' status='ERROR' duration='' target_url='ssh://backup.lan:2222/vol1/replica/docs/docs.20261108T0200' source_url='' parent_url='' message=''
format=\"transaction\" time='1' localtime='x' type='finished' status='success' duration='42' target_url='' source_url='' parent_url='' message=''
";
        let r = parse_transfer(out);
        assert_eq!(r.sent, ["docs.20261108T0100"]);
        assert_eq!(r.pruned, ["docs.20250101T0100"]);
        assert_eq!(r.duration_s, 42);
        assert_eq!(r.errors, ["send-receive ssh://backup.lan:2222/vol1/replica/docs/docs.20261108T0200: ERROR"]);
        assert_eq!(hint("btrbk@backup.lan: Permission denied (publickey)."), ReplicaHint::SshKey);
        assert_eq!(hint("Host key verification failed."), ReplicaHint::HostKey);
        assert_eq!(hint("ssh: Could not resolve hostname x"), ReplicaHint::Unreachable);
        assert_eq!(hint("ssh_filter_btrbk.sh: ssh command rejected"), ReplicaHint::Filter);
        assert_eq!(hint("ERROR: Failed to fetch subvolume detail for: /x"), ReplicaHint::TargetMissing);
        assert_eq!(hint("ERROR: No space left on device"), ReplicaHint::NoSpace);
        assert_eq!(hint("weird"), ReplicaHint::Other);
        let s = next_status(&ReplicaStatus::default(), 5, Ok(&r));
        assert_eq!((s.last_ok_ms, s.latest.as_deref(), s.pruned, s.failing), (Some(5), Some("docs.20261108T0100"), 1, false));
        let f = next_status(&s, 9, Err("Host key verification failed."));
        assert_eq!((f.last_ok_ms, f.last_attempt_ms, f.failing, f.hint), (Some(5), Some(9), true, Some(ReplicaHint::HostKey)));
        let dir = tempfile::tempdir().unwrap();
        write_status(dir.path(), "docs", "hdd", &f).unwrap();
        assert_eq!(read_status(dir.path(), "docs", "hdd"), f);
        assert_eq!(read_status(dir.path(), "docs", "nope"), ReplicaStatus::default());
    }

    #[test]
    fn warnings_without_connecting() {
        let h = MemoryReplicas::new();
        h.add_btrfs("/srv/vol1", "uuid-1");
        h.add_btrfs("/srv/vol2", "uuid-2");
        let local = |p: &str| target("t", ReplicaDestination::Local { path: p.into() });
        let j = job(vec![]);
        assert!(warnings(&h, &j, &local("/srv/vol2/.replicas/docs")).is_empty());
        assert_eq!(warnings(&h, &j, &local("/srv/vol1/replica")), [ReplicaWarning::SameFilesystem]);
        assert_eq!(
            warnings(&h, &j, &local("/mnt/usb/replica")),
            [ReplicaWarning::NotBtrfs, ReplicaWarning::NotMounted]
        );
        let mut r = target("r", remote());
        assert!(warnings(&h, &j, &r).is_empty());
        if let ReplicaDestination::Ssh { host_keys, .. } = &mut r.destination {
            host_keys.clear();
        }
        assert_eq!(warnings(&h, &j, &r), [ReplicaWarning::NoHostKey]);
        assert!(valid_folder("docs-restored") && !valid_folder(".x") && !valid_folder("a/b") && !valid_folder("#replica"));
    }
}
