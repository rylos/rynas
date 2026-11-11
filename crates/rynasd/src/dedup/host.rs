//! What deduplication needs from the machine ([`DedupHost`]): filesystems, the engine, the
//! runs. [`HostDedup`] is the machine; [`MemoryDedup`] plays scripted runs (tests and
//! `--dry-system`) and never touches a disk.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use rynas_api_types::dedup::{ENGINE_PACKAGE, ENGINE_PATH};
use rynas_tasks::{CancellationToken, Cmd, Outside};
use tokio::sync::mpsc::UnboundedSender;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
/// `(exit code, stdout, stderr)` of a short engine command.
pub type ExecOut = (Option<i32>, String, String);

/// What a path's filesystem is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FsFacts {
    /// `major:minor` of the superblock ([`crate::fslock::key_of`]).
    pub key: String,
    /// The filesystem UUID (`None` when unknown).
    pub uuid: Option<String>,
    pub fs_type: String,
    pub mountpoint: String,
    /// XFS: `reflink=1` (`None` for other filesystems or when unknown).
    pub reflink: Option<bool>,
}

/// A run to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSpec {
    /// The job key (unit name and description).
    pub key: String,
    pub argv: Vec<String>,
    /// Properties of the transient unit.
    pub properties: Vec<String>,
    /// The run is stopped (SIGINT) after this long.
    pub max: Duration,
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunEnd {
    /// `None`: no exit status (could not start, or killed).
    pub code: Option<i32>,
    /// Could not start at all.
    pub start_error: Option<String>,
}

pub trait DedupHost: Send + Sync {
    /// In memory: nothing runs, nothing is installed.
    fn simulated(&self) -> bool {
        false
    }
    /// `(label, folder)` of the volumes (the system volume included).
    fn places(&self) -> Vec<(String, String)>;
    fn fs_of(&self, path: &str) -> Option<FsFacts>;
    /// The folder has the no copy-on-write attribute.
    fn nocow(&self, path: &str) -> bool;
    fn is_dir(&self, path: &str) -> bool;
    /// A disk under the filesystem holding `path` is in standby.
    fn asleep(&self, path: &str) -> bool;
    /// Classic btrfs quotas are on for the filesystem mounted at `mountpoint`.
    fn qgroups(&self, mountpoint: &str) -> bool;
    /// The engine's `--version` output (`None`: not installed).
    fn engine_version(&self, argv: &[String]) -> Option<String>;
    /// The package can be installed: `Some(source)` (`apt` or a local `.deb` path).
    fn package_source(&self) -> Option<String>;
    /// Marks a simulated install done.
    fn mark_installed(&self) {}
    /// Runs a short engine command (report, `-R`); `(exit code, stdout, stderr)`.
    fn exec(&self, argv: Vec<String>) -> BoxFuture<'_, Result<ExecOut, String>>;
    /// Makes the hashfile's folder (no copy-on-write where it can).
    fn prepare_dir(&self, dir: &str) -> Result<(), String>;
    /// Deletes a hashfile and its SQLite sidecars.
    fn remove_hashfile(&self, file: &str) -> Result<(), String>;
    /// Runs the engine: every output line goes to `lines` (stderr lines prefixed with
    /// `stderr: `); `stop` asks it to save its work and end (SIGINT, then 5 minutes).
    fn run(&self, spec: RunSpec, lines: UnboundedSender<String>, stop: CancellationToken) -> BoxFuture<'_, RunEnd>;
}

// ------------------------------------------------------------------------------------------
// the machine
// ------------------------------------------------------------------------------------------

/// Where `.deb` files of rynas packages are put until there is an apt repository.
pub const LOCAL_PACKAGES: &str = "/var/lib/rynas/packages";

/// The `rynas-oans` packages rynas accepts from [`LOCAL_PACKAGES`]: `(file name, SHA-256)`,
/// the ones attached to rynas releases (`packaging/oans/build.sh`, reproducible: the same
/// commit of `packaging/oans` gives the same file).
pub const PINNED_DEBS: &[(&str, &str)] = &[(
    "rynas-oans_1.14.1-1_amd64.deb",
    "30502609454d06ff917e70b93c99bf2d4c62d1a5df6e9cb571234fb5159d9085",
)];

const UNIT_PREFIX: &str = "rynas-dedup";

pub struct HostDedup {
    pub shares: std::sync::Arc<rynas_services::SharesDomain>,
}

fn file_sha256(p: &Path) -> Option<String> {
    use sha2::Digest;
    use std::io::Read;
    let mut f = std::fs::File::open(p).ok()?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Some(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// The newest pinned package found in `dir` whose checksum matches.
pub fn local_package(dir: &Path) -> Option<PathBuf> {
    PINNED_DEBS.iter().rev().find_map(|(name, sum)| {
        let p = dir.join(name);
        (p.is_file() && file_sha256(&p).as_deref() == Some(*sum)).then_some(p)
    })
}

fn apt_candidate() -> bool {
    Cmd::new("apt-cache")
        .args(["policy", "--", ENGINE_PACKAGE])
        .env_clear()
        .timeout(Duration::from_secs(20))
        .output_blocking()
        .ok()
        .and_then(|o| o.stdout.lines().find_map(|l| l.trim().strip_prefix("Candidate:").map(|c| c.trim().to_string())))
        .is_some_and(|c| !c.is_empty() && c != "(none)")
}

/// `reflink=1` in `xfs_info`'s output.
pub fn xfs_reflink(info: &str) -> Option<bool> {
    info.split(|c: char| c.is_whitespace() || c == ',')
        .find_map(|w| w.strip_prefix("reflink="))
        .map(|v| v.trim() == "1")
}

/// The UUID of a block device, from `/dev/disk/by-uuid`.
fn uuid_of(source: &str) -> Option<String> {
    let dev = std::fs::canonicalize(source).ok()?;
    std::fs::read_dir("/dev/disk/by-uuid").ok()?.flatten().find_map(|e| {
        let t = std::fs::canonicalize(e.path()).ok()?;
        (t == dev).then(|| e.file_name().to_string_lossy().to_ascii_lowercase())
    })
}

impl DedupHost for HostDedup {
    fn places(&self) -> Vec<(String, String)> {
        crate::places::host_places()
    }

    fn fs_of(&self, path: &str) -> Option<FsFacts> {
        let real = std::fs::canonicalize(path).ok()?;
        let real = real.to_string_lossy();
        let mounts = rynas_storage::mounts::parse_mountinfo(&crate::places::mountinfo());
        let m = crate::fslock::mount_of(&mounts, &real)?;
        let reflink = (m.fstype == "xfs").then(|| {
            Cmd::new("xfs_info")
                .arg(&m.mountpoint)
                .env_clear()
                .timeout(Duration::from_secs(10))
                .output_blocking()
                .ok()
                .and_then(|o| xfs_reflink(&o.stdout))
        });
        Some(FsFacts {
            key: m.majmin.clone(),
            uuid: uuid_of(&m.source),
            fs_type: m.fstype.clone(),
            mountpoint: m.mountpoint.clone(),
            reflink: reflink.flatten(),
        })
    }

    fn nocow(&self, path: &str) -> bool {
        rynas_services::nocow::get(path).unwrap_or(false)
    }

    fn is_dir(&self, path: &str) -> bool {
        Path::new(path).is_dir()
    }

    fn asleep(&self, path: &str) -> bool {
        let mounts = rynas_storage::mounts::parse_mountinfo(&crate::places::mountinfo());
        crate::fslock::mount_of(&mounts, path).is_some_and(|m| self.shares.asleep(&m.mountpoint))
    }

    fn qgroups(&self, mountpoint: &str) -> bool {
        // the quota tree of a btrfs filesystem shows up in sysfs while quotas are on
        let mounts = rynas_storage::mounts::parse_mountinfo(&crate::places::mountinfo());
        let Some(m) = mounts.iter().rfind(|m| m.mountpoint == mountpoint && m.fstype == "btrfs") else {
            return false;
        };
        let Some(uuid) = uuid_of(&m.source) else { return false };
        let q = Path::new("/sys/fs/btrfs").join(&uuid).join("qgroups");
        // simple quotas (squota) count extents to their first owner: no slow commits
        let simple = std::fs::read_to_string(q.join("mode")).is_ok_and(|s| s.trim() == "squota");
        q.is_dir() && !simple
    }

    fn engine_version(&self, argv: &[String]) -> Option<String> {
        if !Path::new(ENGINE_PATH).is_file() {
            return None;
        }
        let (prog, args) = argv.split_first()?;
        Cmd::new(prog)
            .args(args)
            .env_clear()
            .timeout(Duration::from_secs(10))
            .output_blocking()
            .ok()
            .map(|o| o.stdout)
    }

    fn package_source(&self) -> Option<String> {
        if apt_candidate() {
            return Some("apt".into());
        }
        local_package(Path::new(LOCAL_PACKAGES)).map(|p| p.to_string_lossy().into_owned())
    }

    fn exec(&self, argv: Vec<String>) -> BoxFuture<'_, Result<ExecOut, String>> {
        Box::pin(async move {
            let (prog, args) = argv.split_first().ok_or("empty command")?;
            // read-only commands and `-R`: the hashfile only, from rynasd itself
            let o = Cmd::new(prog)
                .args(args)
                .env_clear()
                .timeout(Duration::from_secs(600))
                .run(None, None)
                .await
                .map_err(|e| e.to_string())?;
            Ok((o.code, o.stdout, o.stderr))
        })
    }

    fn prepare_dir(&self, dir: &str) -> Result<(), String> {
        let fresh = !Path::new(dir).exists();
        std::fs::create_dir_all(dir).map_err(|e| format!("{dir}: {e}"))?;
        if fresh {
            // SQLite on copy-on-write fragments; an empty folder can still take the flag
            if let Ok(f) = std::fs::File::open(dir) {
                let _ = rynas_services::nocow::set_fd(&f);
            }
        }
        Ok(())
    }

    fn remove_hashfile(&self, file: &str) -> Result<(), String> {
        for suffix in ["", "-wal", "-shm"] {
            let p = format!("{file}{suffix}");
            match std::fs::remove_file(&p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{p}: {e}")),
            }
        }
        Ok(())
    }

    fn run(&self, spec: RunSpec, lines: UnboundedSender<String>, stop: CancellationToken) -> BoxFuture<'_, RunEnd> {
        Box::pin(async move {
            let unit = format!("{UNIT_PREFIX}-{}", spec.key.replace('.', "-"));
            let desc = format!("rynas: deduplication of {}", spec.key);
            let props: Vec<&str> = spec.properties.iter().map(String::as_str).collect();
            let cmd = Outside::new(&unit, &desc, spec.max)
                .properties(&props)
                .cmd(&spec.argv)
                // the unit gets 5 minutes to save its work after its end or a stop
                .timeout(spec.max + Duration::from_secs(6 * 60 + 30));
            let Some(name) = rynas_tasks::transient_unit(&cmd, &unit) else {
                return RunEnd { code: None, start_error: Some("no transient unit".into()) };
            };
            let finished = CancellationToken::new();
            let stopper = {
                let finished = finished.clone();
                let name = name.clone();
                async move {
                    tokio::select! {
                        _ = stop.cancelled() => {
                            // KillSignal=SIGINT, TimeoutStopSec=5min: the engine saves and ends
                            let r = Cmd::new("systemctl")
                                .args(["stop", "--no-block", "--"])
                                .arg(&name)
                                .env_clear()
                                .timeout(Duration::from_secs(30))
                                .run(None, None)
                                .await;
                            if let Err(e) = r {
                                tracing::warn!("dedup: stopping {name}: {e}");
                            }
                        }
                        _ = finished.cancelled() => {}
                    }
                }
            };
            let sink = move |l: &str| {
                let _ = lines.send(l.to_string());
            };
            let runner = async {
                let r = cmd.run(None, Some(&sink)).await;
                finished.cancel();
                r
            };
            let (r, ()) = tokio::join!(runner, stopper);
            match r {
                Ok(o) => RunEnd { code: o.code, start_error: None },
                Err(rynas_tasks::CmdError::Spawn(e)) => RunEnd { code: None, start_error: Some(e.to_string()) },
                Err(e) => RunEnd { code: None, start_error: Some(e.to_string()) },
            }
        })
    }
}

// ------------------------------------------------------------------------------------------
// in memory
// ------------------------------------------------------------------------------------------

/// A scripted run: lines with a pause before each, then an exit code. A stop makes it end
/// with 130 after `stop_after`.
#[derive(Debug, Clone)]
pub struct Script {
    pub lines: Vec<(Duration, String)>,
    pub code: i32,
    pub stop_after: Duration,
}

impl Script {
    /// A short run that frees `reclaimed` bytes.
    pub fn ok(reclaimed: u64) -> Self {
        let p = Duration::from_millis(300);
        Script {
            lines: vec![
                (p, r#"{"phase":"scanning","elapsed_sec":0.5,"files_examined":1200,"files_to_hash":800}"#.into()),
                (p, r#"{"phase":"hashing","files":400,"files_total":800,"bytes":1073741824,"bytes_total":2147483648,"bytes_per_sec":104857600,"eta_sec":10}"#.into()),
                (p, r#"{"phase":"hashing","files":800,"files_total":800,"bytes":2147483648,"bytes_total":2147483648}"#.into()),
                (p, format!(r#"{{"phase":"dedupe","groups":5,"groups_total":10,"work_done_bytes":10,"work_total_bytes":20,"reclaimed_bytes":{}}}"#, reclaimed / 2)),
                (p, format!(r#"{{"event":"done","elapsed_sec":2.0,"files_scanned":1200,"groups_deduped":10,"reclaimed_bytes":{reclaimed}}}"#)),
            ],
            code: 0,
            stop_after: Duration::from_millis(200),
        }
    }
}

#[derive(Default)]
struct MemState {
    installed: bool,
    scripts: Vec<Script>,
    started: Vec<RunSpec>,
    execs: Vec<Vec<String>>,
    removed: Vec<String>,
    asleep: bool,
    nocow: Vec<String>,
}

/// Scripted runs and a made-up machine: places `(label, folder, fs type)`, every path below
/// a place is on its filesystem.
pub struct MemoryDedup {
    places: Vec<(String, String, String)>,
    state: Mutex<MemState>,
    report: Mutex<String>,
    pub installable: bool,
}

impl MemoryDedup {
    pub fn new(installed: bool, places: Vec<(String, String, String)>) -> Self {
        Self {
            places,
            state: Mutex::new(MemState { installed, ..Default::default() }),
            report: Mutex::new(
                r#"{"files_tracked":1200,"logical_bytes":2147483648,"dup_groups":10,"dup_files":40,"reclaimable_logical_bytes":0,"runs":1,"reclaimed_total_bytes":536870912,"scan_configured_dedupe":true,"readonly_subvols_skipped_last_run":0,"scan_skipped_last_run":{"permission":0}}"#.into(),
            ),
            installable: true,
        }
    }

    /// Scripts for the next runs (in order; the default script after them).
    pub fn push_script(&self, s: Script) {
        self.state.lock().unwrap().scripts.push(s);
    }

    pub fn set_report(&self, json: &str) {
        *self.report.lock().unwrap() = json.into();
    }

    pub fn set_asleep(&self, on: bool) {
        self.state.lock().unwrap().asleep = on;
    }

    pub fn set_nocow(&self, path: &str) {
        self.state.lock().unwrap().nocow.push(path.into());
    }

    pub fn started(&self) -> Vec<RunSpec> {
        self.state.lock().unwrap().started.clone()
    }

    pub fn execs(&self) -> Vec<Vec<String>> {
        self.state.lock().unwrap().execs.clone()
    }

    pub fn removed(&self) -> Vec<String> {
        self.state.lock().unwrap().removed.clone()
    }

    fn place_of(&self, path: &str) -> Option<&(String, String, String)> {
        self.places
            .iter()
            .filter(|(_, d, _)| path == d || path.strip_prefix(d.as_str()).is_some_and(|r| r.starts_with('/')))
            .max_by_key(|(_, d, _)| d.len())
    }
}

impl DedupHost for MemoryDedup {
    fn simulated(&self) -> bool {
        true
    }

    fn places(&self) -> Vec<(String, String)> {
        self.places.iter().map(|(l, d, _)| (l.clone(), d.clone())).collect()
    }

    fn fs_of(&self, path: &str) -> Option<FsFacts> {
        let (label, dir, t) = self.place_of(path)?;
        let n = self.places.iter().position(|p| &p.0 == label).unwrap_or(0);
        Some(FsFacts {
            key: format!("0:{}", 40 + n),
            uuid: Some(format!("00000000-0000-4000-8000-{:012x}", n)),
            fs_type: t.clone(),
            mountpoint: dir.clone(),
            reflink: (t == "xfs").then_some(true),
        })
    }

    fn nocow(&self, path: &str) -> bool {
        self.state.lock().unwrap().nocow.iter().any(|p| p == path)
    }

    fn is_dir(&self, path: &str) -> bool {
        self.place_of(path).is_some()
    }

    fn asleep(&self, _path: &str) -> bool {
        self.state.lock().unwrap().asleep
    }

    fn qgroups(&self, _mountpoint: &str) -> bool {
        false
    }

    fn engine_version(&self, _argv: &[String]) -> Option<String> {
        self.state.lock().unwrap().installed.then(|| "oans v1.14.1\n".into())
    }

    fn package_source(&self) -> Option<String> {
        self.installable.then(|| "apt".into())
    }

    fn mark_installed(&self) {
        self.state.lock().unwrap().installed = true;
    }

    fn exec(&self, argv: Vec<String>) -> BoxFuture<'_, Result<ExecOut, String>> {
        Box::pin(async move {
            let report = argv.iter().any(|a| a == "--json");
            self.state.lock().unwrap().execs.push(argv);
            if report {
                Ok((Some(0), self.report.lock().unwrap().clone(), String::new()))
            } else {
                Ok((Some(0), String::new(), String::new()))
            }
        })
    }

    fn prepare_dir(&self, _dir: &str) -> Result<(), String> {
        Ok(())
    }

    fn remove_hashfile(&self, file: &str) -> Result<(), String> {
        self.state.lock().unwrap().removed.push(file.into());
        Ok(())
    }

    fn run(&self, spec: RunSpec, lines: UnboundedSender<String>, stop: CancellationToken) -> BoxFuture<'_, RunEnd> {
        Box::pin(async move {
            let script = {
                let mut s = self.state.lock().unwrap();
                s.started.push(spec.clone());
                if s.scripts.is_empty() { Script::ok(512 << 20) } else { s.scripts.remove(0) }
            };
            let deadline = tokio::time::Instant::now() + spec.max;
            for (pause, line) in script.lines {
                tokio::select! {
                    _ = tokio::time::sleep(pause) => { let _ = lines.send(line); }
                    _ = stop.cancelled() => {
                        tokio::time::sleep(script.stop_after).await;
                        return RunEnd { code: Some(130), start_error: None };
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        return RunEnd { code: Some(130), start_error: None };
                    }
                }
            }
            RunEnd { code: Some(script.code), start_error: None }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reflink_from_xfs_info() {
        let out = "meta-data=/dev/sdb1 isize=512 agcount=4, agsize=65536 blks\n         =   crc=1 finobt=1, sparse=1, rmapbt=1\n         =   reflink=1    bigtime=1 inobtcount=1 nrext64=1\n";
        assert_eq!(xfs_reflink(out), Some(true));
        assert_eq!(xfs_reflink("reflink=0 bigtime=1"), Some(false));
        assert_eq!(xfs_reflink("nothing"), None);
    }

    #[test]
    fn a_local_package_needs_its_pinned_checksum() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(local_package(d.path()), None);
        let (name, _) = PINNED_DEBS[0];
        std::fs::write(d.path().join(name), b"not the package").unwrap();
        assert_eq!(local_package(d.path()), None);
    }
}
