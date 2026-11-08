//! PC backup, receive side (D-PCBACKUP phase 2, `docs/research/pc-backup.md` §10): folders
//! where Linux PCs push btrbk snapshots over SSH. Administrators only.
//!
//! - `GET /v1/pc-backups/setup`: account, filter, host name, sshd port, btrfs volumes and
//!   what stops PCs from sending (no sshd, `AllowUsers`/`AllowGroups`, no sudo or btrfs, no
//!   filter).
//! - `GET /v1/pc-backups/targets`: every device with its freshness and snapshot counts;
//!   `POST` (202, task `pcbackup.create`): a new subvolume folder, or `adopt` an existing folder
//!   of received snapshots (nothing inside is touched; only the folder's owner and mode).
//! - `POST /v1/pc-backups/plan/adopt`: what a folder holds (read only).
//! - `GET|PUT|DELETE /v1/pc-backups/targets/{device}`: detail with every snapshot; settings
//!   (key, alert, delete rules: task `pcbackup.update`); remove (task `pcbackup.remove`: the
//!   key goes, the folder and its snapshots stay).
//! - `POST /v1/pc-backups/targets/{device}/usage` (task `pcbackup.usage`, `btrfs filesystem du`).
//! - `GET /v1/pc-backups/targets/{device}/browse?snapshot=&path=`: a folder inside a received
//!   snapshot (descriptor walk, symlinks shown never followed);
//!   `POST …/copy-out` (task `pcbackup.copy_out`): a file or folder copied into a NAS folder.
//!
//! The system side is behind [`ReceiveHost`]: the machine ([`HostReceive`]: `useradd`, the
//! account's `authorized_keys`, `/etc/sudoers.d/rynas-pcbackup` checked by `visudo -c`,
//! `btrfs subvolume create`, ioctls) or memory ([`MemoryReceive`], `--dry-system` and tests).
//! Each key line is `restrict,command="rynas-btrbk-filter --device … --root … [rules]"`: the
//! filter ([`rynas_btrfs::receive_filter`]) is the only thing a PC can run. sshd_config is
//! never edited.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::notify::{NotifyCategory, NotifySeverity};
use rynas_api_types::pcbackup::*;
use rynas_api_types::snapshots::{RestoreMode, SnapshotEntry, SnapshotListing};
use rynas_api_types::{TaskStarted, error_codes};
use rynas_btrfs::receive::{self, Entry};
use rynas_notify::Notifier;
use rynas_notify::event::{code, field, firing, resolved};
use rynas_tasks::Cmd;
use serde::{Deserialize, Serialize};

use crate::ops::{Error, Ops, Res, user};
use crate::roles::AdminPolicy;

const DAY_MS: u64 = 86_400_000;
const SUDOERS: &str = "/etc/sudoers.d/rynas-pcbackup";
/// The account's home: root-owned, so the account cannot change its own keys.
const HOME: &str = "/var/lib/rynas-pcbackup";

// ------------------------------------------------------------------ the system side

/// Facts about this machine for the setup card.
#[derive(Debug, Clone, Default)]
pub struct HostFacts {
    pub sshd: bool,
    /// `sshd -T` output (or sshd_config) when readable.
    pub sshd_config: String,
    pub sudo: bool,
    /// Path of btrfs, when installed.
    pub btrfs: Option<String>,
    pub filter: bool,
    /// btrfs mount points that can hold folders.
    pub volumes: Vec<String>,
}

/// What a folder holds: `None` when it does not exist.
pub type Scan = Option<(bool, Vec<Entry>, u32)>;

pub trait ReceiveHost: Send + Sync {
    fn facts(&self) -> HostFacts;
    /// The account, its home and the sudoers file; idempotent. Returns log lines.
    fn ensure_account(&self, btrfs: &str) -> Result<Vec<String>, String>;
    fn write_keys(&self, content: &str) -> Result<(), String>;
    /// A new subvolume folder `0750 root:<account>` (parents made as needed).
    fn create_folder(&self, path: &Path) -> Result<(), String>;
    /// An existing folder: only its own owner group and mode change.
    fn prepare_adopted(&self, path: &Path) -> Result<(), String>;
    /// (is a subvolume, snapshot entries, other entries).
    fn scan(&self, path: &Path) -> Result<Scan, String>;
    fn usage(&self, path: &Path) -> Result<(u64, u64, u64), String>;
    fn browse(&self, snapshot: &Path, rel: &str) -> Result<(Vec<SnapshotEntry>, bool), Error>;
    fn copy_out(
        &self,
        snapshot: &Path,
        rel: &str,
        dest: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<String, String>;
}

/// The machine.
pub struct HostReceive;

fn cmd_ok(c: Cmd) -> Result<String, String> {
    let shown = c.display();
    let o = c.run_blocking().map_err(|e| format!("{shown}: {e}"))?;
    if o.success() {
        Ok(o.stdout)
    } else {
        Err(format!("{shown}: {}", o.stderr.trim()))
    }
}

fn btrfs_mounts(mountinfo: &str) -> Vec<String> {
    let mut v: Vec<String> = mountinfo
        .lines()
        .filter_map(|l| {
            let (pre, post) = l.split_once(" - ")?;
            let fstype = post.split_whitespace().next()?;
            let mp = pre.split_whitespace().nth(4)?.replace("\\040", " ");
            (fstype == "btrfs"
                && mp != "/"
                && !["/boot", "/var", "/usr", "/home", "/root", "/srv/.snapshots", "/.snapshots"]
                    .iter()
                    .any(|s| mp == *s || mp.starts_with(&format!("{s}/")))
                && receive::valid_path(&format!("{mp}/x")).is_ok())
            .then_some(mp)
        })
        .collect();
    v.sort();
    v.dedup();
    v
}

impl ReceiveHost for HostReceive {
    fn facts(&self) -> HostFacts {
        let sshd_bin = rynas_btrfs::receive_filter::locate("sshd").is_some();
        let sshd_config = Cmd::new("sshd")
            .arg("-T")
            .env_clear()
            .timeout(Duration::from_secs(5))
            .run_blocking()
            .ok()
            .filter(|o| o.success())
            .map(|o| o.stdout)
            .or_else(|| std::fs::read_to_string("/etc/ssh/sshd_config").ok())
            .unwrap_or_default();
        HostFacts {
            sshd: sshd_bin,
            sshd_config,
            sudo: rynas_btrfs::receive_filter::locate("sudo").is_some(),
            btrfs: rynas_btrfs::receive_filter::locate("btrfs").map(|p| p.display().to_string()),
            filter: Path::new(FILTER).is_file(),
            volumes: btrfs_mounts(&std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default()),
        }
    }

    fn ensure_account(&self, btrfs: &str) -> Result<Vec<String>, String> {
        let mut log = Vec::new();
        let exists = Cmd::new("getent")
            .args(["passwd", ACCOUNT])
            .run_blocking()
            .is_ok_and(|o| o.success());
        if !exists {
            // a shell is needed: sshd runs the forced command through it
            cmd_ok(Cmd::new("useradd").args([
                "--system", "--user-group", "--home-dir", HOME, "--no-create-home",
                "--shell", "/bin/sh", "--comment", "rynas PC backup (btrbk receive)", ACCOUNT,
            ]))?;
            // no password login ever
            cmd_ok(Cmd::new("usermod").args(["--lock", ACCOUNT]))?;
            log.push(format!("account {ACCOUNT} created"));
        }
        let ssh = Path::new(HOME).join(".ssh");
        std::fs::create_dir_all(&ssh).map_err(|e| format!("{}: {e}", ssh.display()))?;
        for d in [Path::new(HOME), ssh.as_path()] {
            set_mode(d, 0o755)?;
            std::os::unix::fs::chown(d, Some(0), Some(0)).map_err(|e| format!("{}: {e}", d.display()))?;
        }
        let text = receive::sudoers(btrfs);
        let tmp = format!("{SUDOERS}.rynas-new");
        write_file(Path::new(&tmp), &text, 0o440)?;
        if let Err(e) = cmd_ok(Cmd::new("visudo").args(["-c", "-q", "-f", &tmp])) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("sudoers check failed: {e}"));
        }
        std::fs::rename(&tmp, SUDOERS).map_err(|e| format!("{SUDOERS}: {e}"))?;
        log.push(format!("{SUDOERS} written (btrfs at {btrfs})"));
        Ok(log)
    }

    fn write_keys(&self, content: &str) -> Result<(), String> {
        write_file(&Path::new(HOME).join(".ssh/authorized_keys"), content, 0o644)
    }

    fn create_folder(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        cmd_ok(Cmd::new("btrfs").args(["subvolume", "create"]).arg(path))?;
        self.prepare_adopted(path)
    }

    fn prepare_adopted(&self, path: &Path) -> Result<(), String> {
        let gid = Cmd::new("getent")
            .args(["group", ACCOUNT])
            .run_blocking()
            .ok()
            .and_then(|o| o.stdout.split(':').nth(2).and_then(|g| g.trim().parse::<u32>().ok()))
            .ok_or_else(|| format!("group {ACCOUNT} not found"))?;
        std::os::unix::fs::chown(path, Some(0), Some(gid)).map_err(|e| format!("{}: {e}", path.display()))?;
        set_mode(path, 0o750)
    }

    fn scan(&self, path: &Path) -> Result<Scan, String> {
        let dir = match rynas_btrfs::ioctl::open_dir(path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        if !rynas_btrfs::ioctl::is_btrfs(&dir) {
            return Err(format!("{}: not on a btrfs volume", path.display()));
        }
        let sub = rynas_btrfs::ioctl::is_subvolume(&dir);
        let (entries, other) = receive::scan_dir(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Some((sub, entries, other)))
    }

    fn usage(&self, path: &Path) -> Result<(u64, u64, u64), String> {
        let out = cmd_ok(
            Cmd::new("btrfs")
                .args(["filesystem", "du", "-s", "--raw"])
                .arg(path)
                .timeout(Duration::from_secs(3600)),
        )?;
        receive::parse_du(&out).ok_or_else(|| "btrfs filesystem du: unexpected output".into())
    }

    fn browse(&self, snapshot: &Path, rel: &str) -> Result<(Vec<SnapshotEntry>, bool), Error> {
        let item = rynas_btrfs::browse::resolve(snapshot, rel)?;
        Ok(rynas_btrfs::browse::list(&item)?)
    }

    fn copy_out(
        &self,
        snapshot: &Path,
        rel: &str,
        dest: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<String, String> {
        let now = (rynas_tasks::now_ms() / 1000) as i64;
        let r = rynas_btrfs::restore::restore(snapshot, dest, rel, RestoreMode::Copy, now, cancelled)
            .map_err(|e| e.to_string())?;
        Ok(format!(
            "copied to {}/{}: {} files, {} folders, {} bytes",
            dest.display(),
            r.restored_to,
            r.files,
            r.dirs,
            r.bytes
        ))
    }
}

fn set_mode(p: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("{}: {e}", p.display()))
}

/// Written next to the file, then renamed over it.
fn write_file(p: &Path, text: &str, mode: u32) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = p.with_extension("rynas-tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    f.write_all(text.as_bytes())
        .and_then(|()| f.sync_all())
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    set_mode(&tmp, mode)?;
    std::fs::rename(&tmp, p).map_err(|e| format!("{}: {e}", p.display()))
}

/// Memory (`--dry-system`, tests): folders and their snapshots, the keys file, the account.
#[derive(Default)]
pub struct MemoryReceive {
    pub folders: Mutex<BTreeMap<PathBuf, (bool, Vec<Entry>)>>,
    pub keys: Mutex<String>,
    pub account: Mutex<bool>,
    pub facts: Mutex<HostFacts>,
    pub copies: Mutex<Vec<String>>,
}

impl MemoryReceive {
    pub fn new() -> Self {
        let m = Self::default();
        *m.facts.lock().unwrap() = HostFacts {
            sshd: true,
            sshd_config: String::new(),
            sudo: true,
            btrfs: Some("/usr/bin/btrfs".into()),
            filter: true,
            volumes: vec!["/volume1".into()],
        };
        m
    }

    /// A folder with received snapshots, as another NAS would have left it (dry system).
    pub fn demo() -> Self {
        let m = Self::new();
        let now_s = (rynas_tasks::now_ms() / 1000) as i64;
        let mut entries = Vec::new();
        for (g, days) in [("home", [1, 2, 3, 9, 16]), ("rootfs", [1, 2, 3, 9, 16])] {
            for d in days {
                let t = now_s - d * 86_400;
                entries.push(Entry {
                    name: format!("{g}.{}", rynas_btrfs::name::long_timestamp(t)),
                    info: Some(rynas_btrfs::ioctl::SubvolInfo {
                        readonly: true,
                        received_uuid: Some(format!("6f1c2a8e-0d3b-4c5e-9a7f-{:012x}", t)),
                        otime: t + 60,
                        ..Default::default()
                    }),
                });
            }
        }
        entries.sort_by(|a, b| rynas_btrfs::name::compare_names(&b.name, &a.name));
        m.folders
            .lock()
            .unwrap()
            .insert("/volume1/backup/old-laptop".into(), (true, entries));
        m
    }
}

impl ReceiveHost for MemoryReceive {
    fn facts(&self) -> HostFacts {
        self.facts.lock().unwrap().clone()
    }
    fn ensure_account(&self, _: &str) -> Result<Vec<String>, String> {
        let mut a = self.account.lock().unwrap();
        let first = !*a;
        *a = true;
        Ok(if first { vec![format!("account {ACCOUNT} created (memory)")] } else { Vec::new() })
    }
    fn write_keys(&self, content: &str) -> Result<(), String> {
        *self.keys.lock().unwrap() = content.to_string();
        Ok(())
    }
    fn create_folder(&self, path: &Path) -> Result<(), String> {
        self.folders
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), (true, Vec::new()));
        Ok(())
    }
    fn prepare_adopted(&self, _: &Path) -> Result<(), String> {
        Ok(())
    }
    fn scan(&self, path: &Path) -> Result<Scan, String> {
        Ok(self
            .folders
            .lock()
            .unwrap()
            .get(path)
            .map(|(s, e)| (*s, e.clone(), 0)))
    }
    fn usage(&self, path: &Path) -> Result<(u64, u64, u64), String> {
        let n = self.folders.lock().unwrap().get(path).map(|f| f.1.len()).unwrap_or(0) as u64;
        let (total, excl) = ((n * 12) << 30, (n * 300) << 20);
        Ok((total, excl, total - excl))
    }
    fn browse(&self, _: &Path, rel: &str) -> Result<(Vec<SnapshotEntry>, bool), Error> {
        rynas_btrfs::browse::components(rel)?;
        let e = |name: &str, kind, size| SnapshotEntry {
            name: name.into(),
            kind,
            size_bytes: size,
            mtime_ms: 1_762_500_000_000,
            mode: 0o755,
            uid: 1000,
            gid: 1000,
            link: None,
        };
        use rynas_api_types::snapshots::EntryKind::*;
        Ok((
            if rel.is_empty() {
                vec![e("Documents", Dir, 0), e("Pictures", Dir, 0), e("notes.txt", File, 2048)]
            } else {
                vec![e("report.odt", File, 48_213), e("budget.ods", File, 19_002)]
            },
            false,
        ))
    }
    fn copy_out(&self, s: &Path, rel: &str, dest: &Path, _: &dyn Fn() -> bool) -> Result<String, String> {
        let line = format!("{}:{rel} -> {}", s.display(), dest.display());
        self.copies.lock().unwrap().push(line.clone());
        Ok(format!("copied (memory): {line}"))
    }
}

// ------------------------------------------------------------------ records

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Stored {
    description: String,
    path: String,
    public_key: String,
    alert_days: u32,
    delete: DeletePolicy,
    adopted: bool,
    created_at_ms: u64,
    usage: Option<ReceiveUsage>,
    /// The late notification is open.
    #[serde(default)]
    alerted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Registry {
    targets: BTreeMap<String, Stored>,
}

pub struct PcBackup {
    pub ops: Arc<Ops>,
    pub host: Arc<dyn ReceiveHost>,
    pub notifier: Arc<dyn Notifier>,
    pub state_dir: PathBuf,
    pub admins: AdminPolicy,
    pub hostname: String,
    reg: Mutex<Registry>,
    /// One change at a time (create, update, remove rewrite the keys file).
    change: tokio::sync::Mutex<()>,
}

pub type PbState = Option<Arc<PcBackup>>;

impl PcBackup {
    pub fn new(
        ops: Arc<Ops>,
        host: Arc<dyn ReceiveHost>,
        notifier: Arc<dyn Notifier>,
        state_dir: PathBuf,
        admins: AdminPolicy,
        hostname: String,
    ) -> Arc<Self> {
        let reg = std::fs::read(Self::reg_path(&state_dir))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Arc::new(Self {
            ops,
            host,
            notifier,
            state_dir,
            admins,
            hostname,
            reg: Mutex::new(reg),
            change: tokio::sync::Mutex::new(()),
        })
    }

    fn reg_path(state_dir: &Path) -> PathBuf {
        state_dir.join("pcbackup/targets.json")
    }

    /// Changes the records and saves them; undone when saving fails.
    fn edit(&self, f: impl FnOnce(&mut Registry)) -> Result<(), String> {
        let mut g = self.reg.lock().unwrap();
        let before = g.clone();
        f(&mut g);
        let p = Self::reg_path(&self.state_dir);
        let r = p
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .map_err(|e| e.to_string())
            .and_then(|()| write_file(&p, &serde_json::to_string_pretty(&*g).unwrap_or_default(), 0o600));
        if r.is_err() {
            *g = before;
        }
        r
    }

    fn stored(&self, device: &str) -> Res<Stored> {
        self.reg
            .lock()
            .unwrap()
            .targets
            .get(device)
            .cloned()
            .ok_or_else(|| Error::not_found(format!("no PC backup target {device}")))
    }

    /// The keys file from the records.
    fn keys_text(&self) -> String {
        let reg = self.reg.lock().unwrap();
        let lines: Vec<String> = reg
            .targets
            .iter()
            .filter_map(|(d, t)| {
                let (k, b) = receive::parse_public_key(&t.public_key).ok()?;
                Some(receive::key_line(d, &t.path, &t.delete, &k, &b))
            })
            .collect();
        receive::authorized_keys(&lines)
    }

    fn view(&self, device: &str, t: &Stored, now_ms: u64) -> (PcTarget, Vec<ReceivedSnapshot>) {
        let scan = self.host.scan(Path::new(&t.path));
        let (state, s) = match &scan {
            Ok(Some((_, entries, _))) => {
                let s = receive::summarize(entries, &t.delete, now_ms);
                let state = match s.last_received_ms {
                    None => ReceiveState::Waiting,
                    Some(ms) if t.alert_days > 0 && now_ms > ms + u64::from(t.alert_days) * DAY_MS => {
                        ReceiveState::Late
                    }
                    Some(_) => ReceiveState::Ok,
                };
                (state, s)
            }
            _ => (ReceiveState::Missing, receive::Summary::default()),
        };
        let fp = receive::parse_public_key(&t.public_key)
            .ok()
            .and_then(|(k, b)| receive::fingerprint(&b).map(|f| format!("{k} {f}")));
        let target = PcTarget {
            device: device.to_string(),
            description: t.description.clone(),
            path: t.path.clone(),
            public_key: t.public_key.clone(),
            key_fingerprint: fp,
            alert_days: t.alert_days,
            delete: t.delete,
            adopted: t.adopted,
            created_at_ms: t.created_at_ms,
            state,
            snapshots: s.list.iter().filter(|x| !x.incomplete && !x.not_subvolume).count() as u32,
            incomplete: s.incomplete,
            last_received_ms: s.last_received_ms,
            groups: s.groups,
            usage: t.usage,
        };
        (target, s.list)
    }

    fn list(&self) -> Vec<PcTarget> {
        let all: Vec<(String, Stored)> = self
            .reg
            .lock()
            .unwrap()
            .targets
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let now = rynas_tasks::now_ms();
        all.iter().map(|(d, t)| self.view(d, t, now).0).collect()
    }

    fn setup(&self) -> ReceiveSetup {
        let f = self.host.facts();
        let mut problems = Vec::new();
        let p = |code, subject: Option<String>| SetupProblem { code, subject };
        if !f.sshd {
            problems.push(p(SetupProblemCode::SshdMissing, None));
        } else if !receive::sshd_allows(&f.sshd_config, ACCOUNT, &[ACCOUNT]) {
            problems.push(p(SetupProblemCode::SshdAllowList, Some(ACCOUNT.into())));
        }
        if !f.sudo {
            problems.push(p(SetupProblemCode::SudoMissing, None));
        }
        if f.btrfs.is_none() {
            problems.push(p(SetupProblemCode::BtrfsMissing, None));
        }
        if !f.filter {
            problems.push(p(SetupProblemCode::FilterMissing, Some(FILTER.into())));
        }
        ReceiveSetup {
            account: ACCOUNT.into(),
            filter: FILTER.into(),
            host: self.hostname.clone(),
            ssh_port: receive::sshd_port(&f.sshd_config),
            volumes: f.volumes,
            problems,
        }
    }

    /// Opens or closes the late notification of each device (hourly).
    pub fn check_freshness(&self) {
        let now = rynas_tasks::now_ms();
        let all: Vec<(String, Stored)> = self
            .reg
            .lock()
            .unwrap()
            .targets
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (d, t) in all {
            let (v, _) = self.view(&d, &t, now);
            let late = t.alert_days > 0 && v.state == ReceiveState::Late;
            if late == t.alerted {
                continue;
            }
            let key = format!("pcbackup:late:{d}");
            let ev = if late {
                let days = v.last_received_ms.map(|ms| (now - ms) / DAY_MS).unwrap_or(0);
                let mut ev = firing(
                    NotifyCategory::Backup,
                    NotifySeverity::Warning,
                    "pcbackup",
                    key,
                    format!("No backup from {d} for {days} days"),
                );
                ev.body = format!(
                    "Nothing received in {} for {days} days (alert after {}).",
                    t.path, t.alert_days
                );
                field(field(field(code(ev, "pcbackup.late"), "device", &d), "days", days), "alert_days", t.alert_days)
            } else {
                let mut ev = resolved(NotifyCategory::Backup, "pcbackup", key, format!("Backups from {d} arrive again"));
                ev.body = format!("{d} sent a snapshot to {}.", t.path);
                field(code(ev, "pcbackup.resumed"), "device", &d)
            };
            if self.edit(|r| {
                if let Some(x) = r.targets.get_mut(&d) {
                    x.alerted = late;
                }
            })
            .is_ok()
            {
                self.notifier.emit(ev);
            }
        }
    }
}

/// The freshness check every hour (first after two minutes).
pub fn spawn_watch(svc: Arc<PcBackup>) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(120)).await;
        loop {
            let s = svc.clone();
            let _ = tokio::task::spawn_blocking(move || s.check_freshness()).await;
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
}

// ------------------------------------------------------------------ routes

fn pb(st: &PbState) -> Res<Arc<PcBackup>> {
    st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "PC backup is not enabled in this rynasd".into(),
        )
    })
}

fn audit(who: &str, action: &str, target: &str) {
    tracing::info!(target: "rynas::audit", "pc backup {action} {target} by {who}");
}

fn started(id: String) -> Response {
    (StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response()
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Res<T> {
    tokio::task::spawn_blocking(f).await.map_err(Error::internal)
}

pub fn router(st: PbState) -> Router {
    let admins = st.as_ref().map(|s| s.admins.clone()).unwrap_or_default();
    Router::new()
        .route("/v1/pc-backups/setup", get(setup_get))
        .route("/v1/pc-backups/plan/adopt", post(adopt_plan))
        .route("/v1/pc-backups/targets", get(list_get).post(create_post))
        .route(
            "/v1/pc-backups/targets/{device}",
            get(detail_get).put(update_put).delete(remove_delete),
        )
        .route("/v1/pc-backups/targets/{device}/usage", post(usage_post))
        .route("/v1/pc-backups/targets/{device}/browse", get(browse_get))
        .route("/v1/pc-backups/targets/{device}/copy-out", post(copy_out_post))
        .with_state(st)
        .route_layer(axum::middleware::from_fn_with_state(
            crate::roles::AdminGate {
                policy: admins,
                what: "PC backup",
            },
            crate::roles::admin_only,
        ))
}

async fn setup_get(State(st): State<PbState>) -> Res<Json<ReceiveSetup>> {
    let s = pb(&st)?;
    Ok(Json(blocking(move || s.setup()).await?))
}

async fn list_get(State(st): State<PbState>) -> Res<Json<Vec<PcTarget>>> {
    let s = pb(&st)?;
    Ok(Json(blocking(move || s.list()).await?))
}

#[derive(Deserialize)]
struct DetailQuery {
    /// The name or address the PC reaches the NAS by (the browser's, by default).
    #[serde(default)]
    host: String,
}

/// A host name or address for the snippets: `[A-Za-z0-9.:-]`, or the NAS name.
fn snippet_host(asked: &str, fallback: &str) -> String {
    let ok = !asked.is_empty()
        && asked.len() <= 253
        && asked.bytes().all(|b| b.is_ascii_alphanumeric() || b".:-[]".contains(&b));
    if ok { asked.to_string() } else { fallback.to_string() }
}

async fn detail_get(
    State(st): State<PbState>,
    UrlPath(device): UrlPath<String>,
    q: Result<Query<DetailQuery>, axum::extract::rejection::QueryRejection>,
) -> Res<Json<PcTargetDetail>> {
    let s = pb(&st)?;
    let Query(q) = q.map_err(|e| Error::bad_request(e.body_text()))?;
    let t = s.stored(&device)?;
    Ok(Json(
        blocking(move || {
            let (target, list) = s.view(&device, &t, rynas_tasks::now_ms());
            let host = snippet_host(&q.host, &s.hostname);
            let port = receive::sshd_port(&s.host.facts().sshd_config);
            let key_line = receive::parse_public_key(&t.public_key)
                .ok()
                .map(|(k, b)| receive::key_line(&device, &t.path, &t.delete, &k, &b));
            PcTargetDetail {
                key_line,
                key_commands: receive::client_key_commands(&device),
                client_config: receive::client_config(&device, &host, port, &t.path),
                test_commands: receive::client_test_commands(&host, port),
                target,
                list,
            }
        })
        .await?,
    ))
}

fn check_key(k: &str) -> Res<()> {
    if k.trim().is_empty() {
        return Ok(());
    }
    receive::parse_public_key(k)
        .map(|_| ())
        .map_err(|m| Error::invalid("public_key", m))
}

fn check_policy(alert_days: u32, d: &DeletePolicy) -> Res<()> {
    if alert_days > 365 {
        return Err(Error::invalid("alert_days", "at most 365 days"));
    }
    if d.keep_last > 1000 {
        return Err(Error::invalid("delete.keep_last", "at most 1000"));
    }
    if d.min_age_days > 3650 {
        return Err(Error::invalid("delete.min_age_days", "at most 3650 days"));
    }
    Ok(())
}

async fn adopt_plan(
    State(st): State<PbState>,
    body: Result<Json<AdoptRequest>, JsonRejection>,
) -> Res<Json<AdoptPreview>> {
    let s = pb(&st)?;
    let Json(req) = body?;
    receive::valid_path(&req.path).map_err(|m| Error::invalid("path", m))?;
    let path = req.path.clone();
    let scan = blocking(move || s.host.scan(Path::new(&path))).await?;
    let Some((sub, entries, other)) = scan.map_err(|m| Error::invalid("path", m))? else {
        return Err(Error::invalid("path", "no such folder").field("path"));
    };
    let sum = receive::summarize(&entries, &DeletePolicy::default(), rynas_tasks::now_ms());
    Ok(Json(AdoptPreview {
        suggested_device: receive::device_from_path(&req.path),
        path: req.path,
        subvolume: sub,
        list: sum.list,
        groups: sum.groups,
        other_entries: other,
    }))
}

async fn create_post(
    State(st): State<PbState>,
    hd: HeaderMap,
    body: Result<Json<PcTargetCreate>, JsonRejection>,
) -> Res<Response> {
    let (s, who) = (pb(&st)?, user(&hd)?);
    let Json(req) = body?;
    if !valid_device(&req.device) {
        return Err(Error::invalid("device", "lowercase letters, digits and -, at most 32"));
    }
    if req.description.chars().count() > 120 {
        return Err(Error::invalid("description", "at most 120 characters"));
    }
    receive::valid_path(&req.path).map_err(|m| Error::invalid("path", m))?;
    check_key(&req.public_key)?;
    check_policy(req.alert_days, &req.delete)?;
    {
        let reg = s.reg.lock().unwrap();
        if reg.targets.contains_key(&req.device) {
            return Err(Error::new(
                StatusCode::CONFLICT,
                error_codes::ALREADY_EXISTS,
                format!("{} already exists", req.device),
            )
            .field("device"));
        }
        let clash = reg.targets.iter().find(|(_, t)| {
            t.path == req.path
                || req.path.starts_with(&format!("{}/", t.path))
                || t.path.starts_with(&format!("{}/", req.path))
        });
        if let Some((d, _)) = clash {
            return Err(Error::invalid("path", format!("overlaps the folder of {d}")));
        }
    }
    let s2 = s.clone();
    let who2 = who.clone();
    let id = s.ops.tasks.spawn("pcbackup.create", &who, move |ctx| async move {
        let (s, who) = (s2, who2);
        let _g = s.change.lock().await;
        let s3 = s.clone();
        let req2 = req.clone();
        let log = tokio::task::spawn_blocking(move || -> Result<Vec<String>, String> {
            let (s, req) = (s3, req2);
            let f = s.host.facts();
            let btrfs = f.btrfs.ok_or("btrfs-progs is not installed")?;
            let mut log = s.host.ensure_account(&btrfs)?;
            let path = Path::new(&req.path);
            match (s.host.scan(path)?, req.adopt) {
                (Some((_, entries, _)), true) => {
                    s.host.prepare_adopted(path)?;
                    log.push(format!("{} adopted as it is: {} snapshots", req.path, entries.len()));
                }
                (None, true) => return Err(format!("{}: no such folder", req.path)),
                (Some(_), false) => {
                    return Err(format!("{} already exists: adopt it instead", req.path));
                }
                (None, false) => {
                    s.host.create_folder(path)?;
                    log.push(format!("subvolume {} created", req.path));
                }
            }
            s.edit(|r| {
                r.targets.insert(
                    req.device.clone(),
                    Stored {
                        description: req.description.trim().to_string(),
                        path: req.path.clone(),
                        public_key: req.public_key.trim().to_string(),
                        alert_days: req.alert_days,
                        delete: req.delete,
                        adopted: req.adopt,
                        created_at_ms: rynas_tasks::now_ms(),
                        usage: None,
                        alerted: false,
                    },
                );
            })?;
            s.host.write_keys(&s.keys_text())?;
            log.push("keys of the account written".into());
            Ok(log)
        })
        .await
        .map_err(|e| e.to_string())??;
        for l in log {
            ctx.log(&l);
        }
        audit(&who, if req.adopt { "adopt" } else { "create" }, &req.device);
        Ok(())
    })?;
    Ok(started(id))
}

async fn update_put(
    State(st): State<PbState>,
    hd: HeaderMap,
    UrlPath(device): UrlPath<String>,
    body: Result<Json<PcTargetUpdate>, JsonRejection>,
) -> Res<Response> {
    let (s, who) = (pb(&st)?, user(&hd)?);
    let Json(req) = body?;
    s.stored(&device)?;
    if req.description.chars().count() > 120 {
        return Err(Error::invalid("description", "at most 120 characters"));
    }
    check_key(&req.public_key)?;
    check_policy(req.alert_days, &req.delete)?;
    let s2 = s.clone();
    let who2 = who.clone();
    let id = s.ops.tasks.spawn("pcbackup.update", &who, move |ctx| async move {
        let (s, who) = (s2, who2);
        let _g = s.change.lock().await;
        let s3 = s.clone();
        let dev = device.clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            s3.edit(|r| {
                if let Some(t) = r.targets.get_mut(&dev) {
                    t.description = req.description.trim().to_string();
                    t.public_key = req.public_key.trim().to_string();
                    t.alert_days = req.alert_days;
                    t.delete = req.delete;
                }
            })?;
            s3.host.write_keys(&s3.keys_text())
        })
        .await
        .map_err(|e| e.to_string())??;
        ctx.log("settings saved, keys of the account written");
        audit(&who, "update", &device);
        Ok(())
    })?;
    Ok(started(id))
}

async fn remove_delete(
    State(st): State<PbState>,
    hd: HeaderMap,
    UrlPath(device): UrlPath<String>,
) -> Res<Response> {
    let (s, who) = (pb(&st)?, user(&hd)?);
    let t = s.stored(&device)?;
    let s2 = s.clone();
    let who2 = who.clone();
    let id = s.ops.tasks.spawn("pcbackup.remove", &who, move |ctx| async move {
        let (s, who) = (s2, who2);
        let _g = s.change.lock().await;
        let s3 = s.clone();
        let dev = device.clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            s3.edit(|r| {
                r.targets.remove(&dev);
            })?;
            s3.host.write_keys(&s3.keys_text())
        })
        .await
        .map_err(|e| e.to_string())??;
        ctx.log(&format!(
            "{device}: key removed; the folder {} and its snapshots stay",
            t.path
        ));
        if t.alerted {
            s.notifier.emit(resolved(
                NotifyCategory::Backup,
                "pcbackup",
                format!("pcbackup:late:{device}"),
                format!("{device} removed"),
            ));
        }
        audit(&who, "remove", &device);
        Ok(())
    })?;
    Ok(started(id))
}

async fn usage_post(
    State(st): State<PbState>,
    hd: HeaderMap,
    UrlPath(device): UrlPath<String>,
) -> Res<Response> {
    let (s, who) = (pb(&st)?, user(&hd)?);
    let t = s.stored(&device)?;
    let s2 = s.clone();
    let id = s.ops.tasks.spawn("pcbackup.usage", &who, move |ctx| async move {
        let s = s2;
        let s3 = s.clone();
        let path = t.path.clone();
        let (total, excl, shared) = tokio::task::spawn_blocking(move || s3.host.usage(Path::new(&path)))
            .await
            .map_err(|e| e.to_string())??;
        let u = ReceiveUsage {
            total_bytes: total,
            exclusive_bytes: excl,
            shared_bytes: shared,
            read_at_ms: rynas_tasks::now_ms(),
        };
        s.edit(|r| {
            if let Some(x) = r.targets.get_mut(&device) {
                x.usage = Some(u);
            }
        })?;
        ctx.log(&format!("{device}: {total} bytes in all, {excl} only in these snapshots"));
        Ok(())
    })?;
    Ok(started(id))
}

#[derive(Deserialize)]
struct BrowseQuery {
    snapshot: String,
    #[serde(default)]
    path: String,
}

/// The path of snapshot `name` of a device (a direct child with a btrbk name).
fn snapshot_path(t: &Stored, name: &str) -> Res<PathBuf> {
    if name.contains('/') || rynas_btrfs::name::split_snapshot_name(name).is_none() {
        return Err(Error::invalid("snapshot", "not a snapshot of this device"));
    }
    Ok(Path::new(&t.path).join(name))
}

async fn browse_get(
    State(st): State<PbState>,
    UrlPath(device): UrlPath<String>,
    q: Result<Query<BrowseQuery>, axum::extract::rejection::QueryRejection>,
) -> Res<Json<SnapshotListing>> {
    let s = pb(&st)?;
    let Query(q) = q.map_err(|e| Error::bad_request(e.body_text()))?;
    let t = s.stored(&device)?;
    let snap = snapshot_path(&t, &q.snapshot)?;
    let path = q.path.clone();
    let (entries, truncated) = blocking(move || s.host.browse(&snap, &path)).await??;
    let path = rynas_btrfs::browse::components(&q.path)?.join("/");
    Ok(Json(SnapshotListing {
        id: q.snapshot,
        path,
        entries,
        truncated,
    }))
}

/// A folder to copy into: absolute, normalized, not a system folder (spaces are fine: no
/// shell ever sees it).
fn valid_dest(p: &str) -> Result<(), &'static str> {
    let parts: Vec<&str> = p.strip_prefix('/').map(|r| r.split('/').collect()).unwrap_or_default();
    if parts.len() < 2 || p.len() > 1024 || p.contains(['\0', '\n']) {
        return Err("an absolute folder inside a volume, such as /volume1/restore");
    }
    if parts.iter().any(|s| s.is_empty() || *s == "." || *s == "..") {
        return Err("no empty, . or .. parts and no trailing /");
    }
    let sys = ["etc", "usr", "bin", "sbin", "lib", "boot", "proc", "sys", "dev", "run", "root"];
    if sys.contains(&parts[0]) || p.starts_with("/var/lib/rynas") {
        return Err("not inside a system folder");
    }
    Ok(())
}

async fn copy_out_post(
    State(st): State<PbState>,
    hd: HeaderMap,
    UrlPath(device): UrlPath<String>,
    body: Result<Json<CopyOutRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, who) = (pb(&st)?, user(&hd)?);
    let Json(req) = body?;
    let t = s.stored(&device)?;
    let snap = snapshot_path(&t, &req.snapshot)?;
    if rynas_btrfs::browse::components(&req.path)?.is_empty() {
        return Err(Error::invalid("path", "choose a file or folder inside the snapshot"));
    }
    let dest = req.destination.trim().to_string();
    valid_dest(&dest).map_err(|m| Error::invalid("destination", m))?;
    if dest == t.path || dest.starts_with(&format!("{}/", t.path)) {
        return Err(Error::invalid("destination", "a folder outside the device's backups"));
    }
    let s2 = s.clone();
    let who2 = who.clone();
    let id = s.ops.tasks.spawn("pcbackup.copy_out", &who, move |ctx| async move {
        let (s, who) = (s2, who2);
        let ctx2 = ctx.clone();
        let rel = req.path.clone();
        let d = dest.clone();
        let line = tokio::task::spawn_blocking(move || {
            s.host.copy_out(&snap, &rel, Path::new(&d), &|| ctx2.is_cancelled())
        })
        .await
        .map_err(|e| e.to_string())??;
        ctx.log(&line);
        audit(&who, "copy_out", &format!("{device}/{}:{} -> {dest}", req.snapshot, req.path));
        Ok(())
    })?;
    Ok(started(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mounts() {
        let mi = "\
29 1 0:26 /@ / rw - btrfs /dev/sda2 rw\n\
30 29 0:27 / /volume1 rw,relatime - btrfs /dev/md2 rw\n\
31 29 0:28 / /boot rw - vfat /dev/sda1 rw\n\
32 29 0:26 /@home /home rw - btrfs /dev/sda2 rw\n\
33 29 0:29 / /srv/data rw - btrfs /dev/md3 rw\n\
34 29 0:30 / /mnt/usb\\040disk rw - btrfs /dev/sdc1 rw\n";
        assert_eq!(btrfs_mounts(mi), ["/srv/data", "/volume1"]);
    }

    #[test]
    fn destinations() {
        assert!(valid_dest("/volume1/Marco Docs/restore").is_ok());
        for bad in ["volume1/x", "/volume1", "/etc/x", "/volume1/../etc", "/volume1/x/", "/var/lib/rynas/x"] {
            assert!(valid_dest(bad).is_err(), "{bad}");
        }
    }
}
