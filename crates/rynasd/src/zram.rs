//! zram, compressed swap in RAM (`/v1/system/zram`, administrators only; D-ZRAM).
//!
//! The device is made by `systemd-zram-generator` from one drop-in, the VM settings come from
//! one sysctl file, and those two files are the saved state: nothing else to keep in step.
//!
//! - Debian's package ships a `[zram0]` section, so it is installed only when zram is enabled,
//!   with the drop-in already in place; "off" is the drop-in with `zram-size = 0`.
//! - zram lives together with swap on disk: it gets a priority above all of them, so the
//!   disks only take what no longer fits.
//! - A change of size or algorithm empties the device. That needs room for its data elsewhere;
//!   without it the values are saved for the next restart ([`ZramState::pending_restart`]).
//!
//! `--dry-system`: files go below the dry root, nothing is run and the device is simulated.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rynas_api_types::zram::{DiskSwap, ZramRunning, ZramSettings, ZramState};
use rynas_api_types::{TaskStarted, error_codes};
use rynas_hw::under;
use rynas_tasks::{Cmd, TaskCtx};

use crate::ops::{Error, Ops, Res, user};

const PACKAGE: &str = "systemd-zram-generator";
const GENERATOR: &str = "/usr/lib/systemd/system-generators/zram-generator";
const DROPIN: &str = "/etc/systemd/zram-generator.conf.d/50-rynas.conf";
const SYSCTL: &str = "/etc/sysctl.d/60-rynas-zram.conf";
/// Written by the generator; a running device without it was set up by something else.
const GENERATED_UNIT: &str = "/run/systemd/generator/dev-zram0.swap";
const SETUP_UNIT: &str = "systemd-zram-setup@zram0.service";
const SWAP_UNIT: &str = "dev-zram0.swap";
const MANAGED: &str = "# Managed by rynas (System > Memory): changes here are replaced.\n";

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
const MIN_SIZE_MIB: u32 = 64;
/// Free room demanded on top of the data of a device about to be emptied.
const SWAPOFF_MARGIN: u64 = 256 * MIB;
const STOP_TIMEOUT: Duration = Duration::from_secs(300);
const APT_TIMEOUT: Duration = Duration::from_secs(1800);

/// Offered until the kernel module is loaded and can tell its own list.
const USUAL_ALGORITHMS: [&str; 2] = ["zstd", "lz4"];
/// Everything the kernel may name: an answer outside this list never reaches a config file.
const KNOWN_ALGORITHMS: [&str; 7] = ["zstd", "lz4", "lz4hc", "lzo-rle", "lzo", "deflate", "842"];

/// What the VM settings go back to when zram is turned off (kernel defaults); a sysctl file of
/// the administrator then has the last word.
const VM_DEFAULTS: [(&str, u32); 4] = [
    ("swappiness", 60),
    ("page-cluster", 3),
    ("watermark_boost_factor", 15000),
    ("watermark_scale_factor", 10),
];

// ------------------------------------------------------------------ advice and files

/// The values advised for this machine. Size: all the RAM up to 8 GiB, then half of it, at
/// most 16 GiB. zstd saves the most RAM where it is scarce; with plenty, lz4 costs less CPU.
/// Swappiness tells the kernel how cheap swapping is: 180 when all of it is fast, 100 when
/// what overflows zram may land on a spinning disk (or on something unknown).
pub fn recommended(mem_total: u64, algorithms: &[String], disks: &[DiskSwap]) -> ZramSettings {
    let size = if mem_total <= 8 * GIB {
        mem_total
    } else {
        (mem_total / 2).clamp(8 * GIB, 16 * GIB)
    };
    let has = |a: &str| algorithms.iter().any(|x| x == a);
    let wanted = if mem_total <= 12 * GIB { "zstd" } else { "lz4" };
    let algorithm = if has(wanted) {
        wanted.to_string()
    } else {
        ["zstd", "lz4"]
            .iter()
            .find(|a| has(a))
            .map(|a| a.to_string())
            .or_else(|| algorithms.first().cloned())
            .unwrap_or_else(|| wanted.to_string())
    };
    ZramSettings {
        enabled: true,
        size_mib: (size / MIB) as u32,
        algorithm,
        swappiness: if disks.iter().all(|d| d.rotational == Some(false)) { 180 } else { 100 },
        page_cluster: 0,
    }
}

/// Above every other swap device, 100 at least.
pub fn priority(disks: &[DiskSwap]) -> i32 {
    disks.iter().map(|d| d.priority.saturating_add(1)).max().unwrap_or(100).clamp(100, 32767)
}

fn render_dropin(s: &ZramSettings, prio: i32) -> String {
    if s.enabled {
        format!(
            "{MANAGED}[zram0]\nzram-size = {}\ncompression-algorithm = {}\nswap-priority = {prio}\n",
            s.size_mib, s.algorithm
        )
    } else {
        format!("{MANAGED}[zram0]\nzram-size = 0\n")
    }
}

/// `watermark_*`: reclaim starts a little earlier and is not boosted, the settings that go
/// with swap in RAM.
fn vm_values(s: &ZramSettings) -> [(&'static str, u32); 4] {
    [
        ("swappiness", s.swappiness),
        ("page-cluster", s.page_cluster),
        ("watermark_boost_factor", 0),
        ("watermark_scale_factor", 125),
    ]
}

fn render_sysctl(s: &ZramSettings) -> String {
    let mut out = MANAGED.to_string();
    for (k, v) in vm_values(s) {
        out.push_str(&format!("vm.{k} = {v}\n"));
    }
    out
}

fn value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == key).then(|| v.trim())
    })
}

/// The saved choice from the two files; `None` when rynas never saved one.
fn parse_saved(dropin: &str, sysctl: &str, off: &ZramSettings) -> Option<ZramSettings> {
    let size: u32 = value(dropin, "zram-size")?.parse().ok()?;
    if size == 0 {
        return Some(ZramSettings { enabled: false, ..off.clone() });
    }
    Some(ZramSettings {
        enabled: true,
        size_mib: size,
        algorithm: value(dropin, "compression-algorithm")?.to_string(),
        swappiness: value(sysctl, "vm.swappiness").and_then(|v| v.parse().ok()).unwrap_or(off.swappiness),
        page_cluster: value(sysctl, "vm.page-cluster").and_then(|v| v.parse().ok()).unwrap_or(off.page_cluster),
    })
}

/// (`MemTotal`, `MemAvailable`) in bytes.
pub fn parse_meminfo(text: &str) -> (u64, u64) {
    let get = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(':')?.split_ascii_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    };
    (get("MemTotal"), get("MemAvailable"))
}

/// The lines of `/proc/swaps` other than zram: (path, is a partition, size, used, priority).
fn parse_swaps(text: &str) -> Vec<(String, bool, u64, u64, i32)> {
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_ascii_whitespace().collect();
            let [path, kind, size, used, prio] = f.as_slice() else {
                return None;
            };
            let path = path.replace("\\040", " ");
            (!path.starts_with("/dev/zram")).then_some((
                path,
                *kind == "partition",
                size.parse::<u64>().ok()? * 1024,
                used.parse::<u64>().ok()? * 1024,
                prio.parse().ok()?,
            ))
        })
        .collect()
}

/// "lzo-rle lzo [lz4] zstd" → the names without brackets, and the selected one.
fn parse_algorithms(text: &str) -> (Vec<String>, Option<String>) {
    let mut selected = None;
    let all = text
        .split_ascii_whitespace()
        .map(|w| match w.strip_prefix('[').and_then(|x| x.strip_suffix(']')) {
            Some(x) => {
                selected = Some(x.to_string());
                x.to_string()
            }
            None => w.to_string(),
        })
        .collect();
    (all, selected)
}

/// `CONFIG_ZRAM=y` or `=m` in a kernel's build options.
fn kernel_has_zram(config: &str) -> bool {
    config.lines().any(|l| l == "CONFIG_ZRAM=y" || l == "CONFIG_ZRAM=m")
}

fn max_size_mib(mem_total: u64) -> u32 {
    ((mem_total / MIB) as u32).saturating_mul(2).max(MIN_SIZE_MIB)
}

fn validate(s: &ZramSettings, mem_total: u64, algorithms: &[String]) -> Res<()> {
    if !s.enabled {
        return Ok(());
    }
    let max = max_size_mib(mem_total);
    if !(MIN_SIZE_MIB..=max).contains(&s.size_mib) {
        return Err(Error::invalid("size_mib", format!("the size is {MIN_SIZE_MIB}-{max} MiB")));
    }
    if !KNOWN_ALGORITHMS.contains(&s.algorithm.as_str()) || !algorithms.contains(&s.algorithm) {
        return Err(Error::invalid("algorithm", format!("{:?} is not offered by this kernel", s.algorithm)));
    }
    if s.swappiness > 200 {
        return Err(Error::invalid("swappiness", "swappiness is 0-200"));
    }
    if s.page_cluster > 5 {
        return Err(Error::invalid("page_cluster", "page-cluster is 0-5"));
    }
    Ok(())
}

/// Emptying a device moves its data to RAM or to the other swap devices.
fn room_to_empty(data: u64, mem_available: u64, disks: &[DiskSwap]) -> bool {
    let free_disk: u64 = disks.iter().map(|d| d.size.saturating_sub(d.used)).sum();
    mem_available.saturating_add(free_disk) >= data.saturating_add(SWAPOFF_MARGIN)
}

fn differs(r: &ZramRunning, s: &ZramSettings) -> bool {
    r.size != u64::from(s.size_mib) * MIB || r.algorithm != s.algorithm
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Step {
    Install,
    Stop,
    Reload,
    Start,
}

/// What an apply does after the files are written, and whether the device change waits for a
/// restart.
fn plan(s: &ZramSettings, running: Option<&ZramRunning>, installed: bool, room: bool) -> (Vec<Step>, bool) {
    let mut steps = Vec::new();
    if s.enabled && !installed {
        steps.push(Step::Install);
    }
    let must_stop = running.is_some_and(|r| !s.enabled || differs(r, s));
    if must_stop && !room {
        return (steps, true);
    }
    if must_stop {
        steps.push(Step::Stop);
    }
    if installed || s.enabled {
        steps.push(Step::Reload);
    }
    if s.enabled && (must_stop || running.is_none()) {
        steps.push(Step::Start);
    }
    (steps, false)
}

// ------------------------------------------------------------------ the machine

pub struct Zram {
    /// Where the configuration files are ("/", or the dry root).
    pub etc_root: PathBuf,
    /// Where `/proc`, `/sys` and `/lib/modules` are ("/"; tests: a directory).
    pub sys_root: PathBuf,
    pub dry: bool,
    pub busy: AtomicBool,
}

impl Zram {
    pub fn new(etc_root: PathBuf, dry: bool) -> Self {
        Zram { etc_root, sys_root: PathBuf::from("/"), dry, busy: AtomicBool::new(false) }
    }

    fn sys(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(under(&self.sys_root, path)).ok()
    }

    fn etc(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(under(&self.etc_root, path)).ok()
    }

    fn installed(&self) -> bool {
        under(&self.etc_root, GENERATOR).exists()
    }

    fn supported(&self) -> bool {
        if self.dry || under(&self.sys_root, "/sys/module/zram").exists() {
            return true;
        }
        // not loaded yet: the kernel's build options tell (the sandbox hides /lib/modules);
        // without them it is simply tried
        let release = self.sys("/proc/sys/kernel/osrelease").unwrap_or_default();
        self.sys(&format!("/boot/config-{}", release.trim())).is_none_or(|t| kernel_has_zram(&t))
    }

    fn algorithms(&self) -> Vec<String> {
        let live = if self.dry { None } else { self.sys("/sys/block/zram0/comp_algorithm") };
        let known = live.map(|t| parse_algorithms(&t).0).filter(|v| !v.is_empty());
        known.unwrap_or_else(|| USUAL_ALGORITHMS.iter().map(|a| a.to_string()).collect())
    }

    fn meminfo(&self) -> (u64, u64) {
        parse_meminfo(&self.sys("/proc/meminfo").unwrap_or_default())
    }

    fn rotational(&self, path: &str, partition: bool) -> Option<bool> {
        if !partition {
            return None;
        }
        let real = std::fs::canonicalize(under(&self.sys_root, path)).unwrap_or_else(|_| PathBuf::from(path));
        let name = real.file_name()?.to_str()?.to_string();
        ["queue/rotational", "../queue/rotational"].iter().find_map(|q| {
            self.sys(&format!("/sys/class/block/{name}/{q}")).map(|t| t.trim() == "1")
        })
    }

    fn disk_swaps(&self) -> Vec<DiskSwap> {
        if self.dry {
            return Vec::new();
        }
        parse_swaps(&self.sys("/proc/swaps").unwrap_or_default())
            .into_iter()
            .map(|(path, partition, size, used, priority)| DiskSwap {
                rotational: self.rotational(&path, partition),
                path,
                size,
                used,
                priority,
            })
            .collect()
    }

    fn running(&self) -> Option<ZramRunning> {
        if self.dry {
            let d = self.etc(DROPIN)?;
            let size: u64 = value(&d, "zram-size")?.parse().ok()?;
            return (size > 0).then(|| ZramRunning {
                size: size * MIB,
                algorithm: value(&d, "compression-algorithm").unwrap_or("zstd").to_string(),
                data: 0,
                mem_used: 0,
            });
        }
        let size: u64 = self.sys("/sys/block/zram0/disksize")?.trim().parse().ok()?;
        if size == 0 {
            return None;
        }
        let stat = self.sys("/sys/block/zram0/mm_stat").unwrap_or_default();
        let v: Vec<u64> = stat.split_ascii_whitespace().map_while(|f| f.parse().ok()).collect();
        let algorithm = self.sys("/sys/block/zram0/comp_algorithm").and_then(|t| parse_algorithms(&t).1)?;
        Some(ZramRunning {
            size,
            algorithm,
            data: v.first().copied().unwrap_or(0),
            mem_used: v.get(2).copied().unwrap_or(0),
        })
    }

    fn external(&self, running: Option<&ZramRunning>) -> bool {
        // with rynas's drop-in the device is ours, even when a reload already dropped the
        // generated unit of a device that "off" could not empty yet
        !self.dry
            && running.is_some()
            && self.etc(DROPIN).is_none()
            && !under(&self.sys_root, GENERATED_UNIT).exists()
    }

    /// `vm.<key>` as it is now.
    fn vm_now(&self, key: &str) -> Option<u32> {
        self.sys(&format!("/proc/sys/vm/{key}"))?.trim().parse().ok()
    }

    pub fn state(&self) -> ZramState {
        let (mem_total, _) = self.meminfo();
        let algorithms = self.algorithms();
        let disk_swaps = self.disk_swaps();
        let recommended = recommended(mem_total, &algorithms, &disk_swaps);
        let running = self.running();
        let off = ZramSettings { enabled: false, ..recommended.clone() };
        let saved = self.etc(DROPIN).and_then(|d| parse_saved(&d, &self.etc(SYSCTL).unwrap_or_default(), &off));
        let pending_restart = saved.as_ref().is_some_and(|s| match &running {
            Some(r) => !s.enabled || differs(r, s),
            None => s.enabled,
        });
        let settings = saved.unwrap_or_else(|| match &running {
            Some(r) => ZramSettings {
                enabled: true,
                size_mib: (r.size / MIB) as u32,
                algorithm: r.algorithm.clone(),
                swappiness: self.vm_now("swappiness").unwrap_or(recommended.swappiness),
                page_cluster: self.vm_now("page-cluster").unwrap_or(recommended.page_cluster),
            },
            None => off,
        });
        ZramState {
            supported: self.supported(),
            installed: self.installed(),
            external: self.external(running.as_ref()),
            settings,
            recommended,
            pending_restart,
            algorithms,
            max_size_mib: max_size_mib(mem_total),
            mem_total,
            disk_swaps,
            zswap: !self.dry && self.sys("/sys/module/zswap/parameters/enabled").is_some_and(|t| t.trim() == "Y"),
            running,
        }
    }

    fn write_file(&self, path: &str, text: &str) -> Result<(), String> {
        let p = under(&self.etc_root, path);
        let fail = |e: std::io::Error| format!("{}: {e}", p.display());
        let dir = p.parent().unwrap_or(Path::new("/"));
        if !dir.exists() {
            std::fs::create_dir_all(dir).map_err(fail)?;
            std::fs::set_permissions(dir, PermissionsExt::from_mode(0o755)).map_err(fail)?;
        }
        let new = p.with_extension("rynas-new");
        std::fs::write(&new, text).map_err(fail)?;
        // system configuration, readable like the rest of /etc (rynasd's umask is tighter)
        std::fs::set_permissions(&new, PermissionsExt::from_mode(0o644)).map_err(fail)?;
        std::fs::rename(&new, &p).map_err(fail)
    }

    fn remove_file(&self, path: &str) {
        let _ = std::fs::remove_file(under(&self.etc_root, path));
    }

    fn write_vm(&self, values: &[(&str, u32)]) -> Result<(), String> {
        if self.dry {
            return Ok(());
        }
        for (k, v) in values {
            let p = under(&self.sys_root, &format!("/proc/sys/vm/{k}"));
            std::fs::write(&p, format!("{v}\n")).map_err(|e| format!("vm.{k}: {e}"))?;
        }
        Ok(())
    }

    async fn run(&self, ctx: &TaskCtx, cmd: Cmd) -> Result<(), String> {
        if self.dry {
            ctx.log(&format!("dry system, not run: {}", cmd.display()));
            return Ok(());
        }
        ctx.run(&cmd).await.map(|_| ()).map_err(|e| e.to_string())
    }

    /// The task behind `POST /v1/system/zram`.
    async fn apply(&self, ctx: &TaskCtx, s: &ZramSettings) -> Result<(), String> {
        let running = self.running();
        let installed = self.installed();
        let disks = self.disk_swaps();
        let (_, available) = self.meminfo();
        let room = running.as_ref().is_none_or(|r| room_to_empty(r.data, available, &disks));
        let (steps, deferred) = plan(s, running.as_ref(), installed, room);
        let previous = self.etc(DROPIN);
        if installed || s.enabled {
            // before the package, or its own [zram0] would be the configuration
            self.write_file(DROPIN, &render_dropin(s, priority(&disks)))?;
        }
        let sys = |args: &[&str], t: Duration| Cmd::new("systemctl").args(args.iter().copied()).timeout(t);
        for step in steps {
            match step {
                Step::Install => {
                    ctx.log(&format!("installing {PACKAGE} from the Debian archive"));
                    let cmd = crate::updates::apt(&["install", "--", PACKAGE], APT_TIMEOUT);
                    if let Err(e) = self.run(ctx, cmd).await {
                        // dpkg may still finish after a cancel: without a drop-in the
                        // package's own [zram0] would start at the next boot
                        let off = ZramSettings { enabled: false, ..s.clone() };
                        let back = previous.clone().unwrap_or_else(|| render_dropin(&off, 100));
                        self.write_file(DROPIN, &back)?;
                        return Err(e);
                    }
                    if self.dry {
                        self.write_file(GENERATOR, "")?;
                    }
                }
                Step::Stop => {
                    ctx.log("emptying the zram device");
                    // both: after a reload with size 0 the setup unit no longer pulls the swap
                    if let Err(e) = self.run(ctx, sys(&["stop", SWAP_UNIT, SETUP_UNIT], STOP_TIMEOUT)).await {
                        match &previous {
                            Some(t) => self.write_file(DROPIN, t)?,
                            None => self.remove_file(DROPIN),
                        }
                        return Err(e);
                    }
                }
                Step::Reload => self.run(ctx, sys(&["daemon-reload"], Duration::from_secs(60))).await?,
                Step::Start => {
                    ctx.log(&format!("starting zram: {} MiB, {}", s.size_mib, s.algorithm));
                    self.run(ctx, sys(&["start", SWAP_UNIT], Duration::from_secs(60))).await?;
                }
            }
        }
        if deferred {
            ctx.log("zram holds too much data to be emptied now: the new device comes with the next restart");
        }
        if s.enabled {
            self.write_file(SYSCTL, &render_sysctl(s))?;
            self.write_vm(&vm_values(s))?;
        } else if self.etc(SYSCTL).is_some() {
            self.remove_file(SYSCTL);
            self.write_vm(&VM_DEFAULTS)?;
            self.run(ctx, sys(&["restart", "systemd-sysctl.service"], Duration::from_secs(30))).await?;
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ API

pub struct ZramApi {
    pub zram: Arc<Zram>,
    pub ops: Arc<Ops>,
}

pub type ZramHandle = Option<Arc<ZramApi>>;

fn api(st: &ZramHandle) -> Res<Arc<ZramApi>> {
    st.clone().ok_or_else(|| {
        Error::new(StatusCode::SERVICE_UNAVAILABLE, error_codes::INTERNAL, "zram is not available".into())
    })
}

fn conflict(msg: &str) -> Error {
    Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, msg.into())
}

pub fn router(st: ZramHandle) -> Router {
    let admins = st.as_ref().map(|s| s.ops.admins.clone()).unwrap_or_default();
    Router::new()
        .route("/v1/system/zram", get(state_get).post(apply_post))
        .with_state(st)
        .route_layer(axum::middleware::from_fn_with_state(
            crate::roles::AdminGate { policy: admins, what: "zram" },
            crate::roles::admin_only,
        ))
}

async fn state_get(State(st): State<ZramHandle>) -> Res<Json<ZramState>> {
    let a = api(&st)?;
    let z = a.zram.clone();
    tokio::task::spawn_blocking(move || z.state()).await.map(Json).map_err(Error::internal)
}

/// Clears the busy flag when the task ends, however it ends.
struct Busy(Arc<Zram>);

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::SeqCst);
    }
}

async fn apply_post(
    State(st): State<ZramHandle>,
    hd: HeaderMap,
    body: Result<Json<ZramSettings>, JsonRejection>,
) -> Res<Response> {
    let (a, who) = (api(&st)?, user(&hd)?);
    let Json(req) = body?;
    let z = a.zram.clone();
    let probe = z.clone();
    let now = tokio::task::spawn_blocking(move || probe.state()).await.map_err(Error::internal)?;
    if !now.supported {
        return Err(conflict("this kernel has no zram"));
    }
    if now.external {
        return Err(conflict("this zram device was set up outside rynas"));
    }
    validate(&req, now.mem_total, &now.algorithms)?;
    if z.busy.swap(true, Ordering::SeqCst) {
        return Err(conflict("another zram change is running"));
    }
    let guard = Busy(z.clone());
    tracing::info!(target: "rynas::audit", "zram apply enabled={} size={} by {who}", req.enabled, req.size_mib);
    let id = a.ops.tasks.spawn("zram.apply", &who, move |ctx| async move {
        let _guard = guard;
        z.apply(&ctx, &req).await
    })?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: u64 = GIB;

    fn algos() -> Vec<String> {
        vec!["lz4".into(), "lz4hc".into(), "zstd".into(), "deflate".into()]
    }

    fn disk(rotational: Option<bool>, priority: i32) -> DiskSwap {
        DiskSwap { path: "/dev/sda2".into(), size: 4 * G, used: 0, priority, rotational }
    }

    fn on(size_mib: u32, algorithm: &str) -> ZramSettings {
        ZramSettings { enabled: true, size_mib, algorithm: algorithm.into(), swappiness: 180, page_cluster: 0 }
    }

    fn running(size_mib: u32, algorithm: &str) -> ZramRunning {
        ZramRunning { size: u64::from(size_mib) * MIB, algorithm: algorithm.into(), data: 0, mem_used: 0 }
    }

    #[test]
    fn advice_follows_the_machine() {
        let r = recommended(2 * G, &algos(), &[]);
        assert_eq!((r.size_mib, r.algorithm.as_str(), r.swappiness, r.page_cluster), (2048, "zstd", 180, 0));
        assert_eq!(recommended(8 * G, &algos(), &[]).size_mib, 8192);
        assert_eq!(recommended(12 * G, &algos(), &[]).size_mib, 8192, "half, but 8 GiB at least");
        assert_eq!(recommended(16 * G, &algos(), &[]).algorithm, "lz4");
        assert_eq!(recommended(64 * G, &algos(), &[]).size_mib, 16384);
        assert_eq!(recommended(2 * G, &["lz4".to_string()], &[]).algorithm, "lz4", "only what the kernel has");
    }

    #[test]
    fn swappiness_depends_on_where_the_overflow_goes() {
        assert_eq!(recommended(2 * G, &algos(), &[disk(Some(false), -1)]).swappiness, 180);
        assert_eq!(recommended(2 * G, &algos(), &[disk(Some(true), -1)]).swappiness, 100);
        assert_eq!(recommended(2 * G, &algos(), &[disk(None, -1)]).swappiness, 100);
    }

    #[test]
    fn zram_goes_before_every_disk() {
        assert_eq!(priority(&[]), 100);
        assert_eq!(priority(&[disk(None, -2), disk(None, 5)]), 100);
        assert_eq!(priority(&[disk(None, 200)]), 201);
        assert_eq!(priority(&[disk(None, 32767)]), 32767);
    }

    #[test]
    fn files_round_trip() {
        let s = on(3672, "zstd");
        let d = render_dropin(&s, 100);
        assert!(d.contains("[zram0]\nzram-size = 3672\ncompression-algorithm = zstd\nswap-priority = 100\n"), "{d}");
        let y = render_sysctl(&s);
        assert!(y.contains("vm.swappiness = 180\nvm.page-cluster = 0\nvm.watermark_boost_factor = 0\nvm.watermark_scale_factor = 125\n"), "{y}");
        let off = ZramSettings { enabled: false, ..on(100, "lz4") };
        assert_eq!(parse_saved(&d, &y, &off).unwrap(), s);
        let d0 = render_dropin(&off, 100);
        assert!(d0.contains("zram-size = 0\n") && !d0.contains("compression"), "{d0}");
        assert_eq!(parse_saved(&d0, "", &off).unwrap(), off);
        assert_eq!(parse_saved("garbage", "", &off), None);
    }

    #[test]
    fn proc_files() {
        let (t, a) = parse_meminfo("MemTotal:  3760000 kB\nMemFree: 1 kB\nMemAvailable:  2000 kB\n");
        assert_eq!((t, a), (3760000 * 1024, 2000 * 1024));
        let s = parse_swaps(
            "Filename Type Size Used Priority\n/dev/nvme0n1p1 partition 7812092 28772 -1\n/dev/zram0 partition 1048572 0 100\n/swap\\040file file 1024 0 -3\n",
        );
        assert_eq!(s.len(), 2);
        assert_eq!(s[0], ("/dev/nvme0n1p1".to_string(), true, 7812092 * 1024, 28772 * 1024, -1));
        assert_eq!(s[1].0, "/swap file");
        assert!(!s[1].1);
        let (all, sel) = parse_algorithms("[lz4] lz4hc zstd deflate \n");
        assert_eq!((all.len(), sel.as_deref()), (4, Some("lz4")));
    }

    #[test]
    fn validation() {
        let m = 4 * G;
        assert!(validate(&on(4096, "zstd"), m, &algos()).is_ok());
        assert!(validate(&on(10, "zstd"), m, &algos()).is_err());
        assert!(validate(&on(8193, "zstd"), m, &algos()).is_err());
        assert!(validate(&on(4096, "lzo"), m, &algos()).is_err(), "not in this kernel");
        let evil = "zstd\n[zram1]".to_string();
        assert!(validate(&on(4096, &evil), m, std::slice::from_ref(&evil)).is_err(), "never reaches the file");
        assert!(validate(&ZramSettings { swappiness: 201, ..on(4096, "zstd") }, m, &algos()).is_err());
        assert!(validate(&ZramSettings { page_cluster: 6, ..on(4096, "zstd") }, m, &algos()).is_err());
        assert!(validate(&ZramSettings { enabled: false, size_mib: 0, ..on(0, "x") }, m, &algos()).is_ok());
    }

    #[test]
    fn plans() {
        use Step::*;
        let s = on(2048, "zstd");
        let off = ZramSettings { enabled: false, ..s.clone() };
        assert_eq!(plan(&s, None, false, true), (vec![Install, Reload, Start], false));
        assert_eq!(plan(&s, None, true, true), (vec![Reload, Start], false));
        let same = running(2048, "zstd");
        assert_eq!(plan(&s, Some(&same), true, true), (vec![Reload], false), "only VM values changed");
        let other = running(1024, "zstd");
        assert_eq!(plan(&s, Some(&other), true, true), (vec![Stop, Reload, Start], false));
        assert_eq!(plan(&s, Some(&running(2048, "lz4")), true, true), (vec![Stop, Reload, Start], false));
        assert_eq!(plan(&s, Some(&other), true, false), (vec![], true), "no room: next restart");
        assert_eq!(plan(&off, Some(&other), true, true), (vec![Stop, Reload], false));
        assert_eq!(plan(&off, None, true, true), (vec![Reload], false));
        assert_eq!(plan(&off, None, false, true), (vec![], false), "nothing to install to turn it off");
    }

    #[test]
    fn room_counts_ram_and_disks() {
        let d = DiskSwap { used: G, ..disk(None, -1) };
        assert!(room_to_empty(G, 2 * G, &[]));
        assert!(!room_to_empty(2 * G, G, &[]));
        assert!(room_to_empty(2 * G, G, &[d]), "3 GiB free on the disk");
    }

    fn fake(dir: &Path) -> Zram {
        let mut z = Zram::new(dir.join("etc-root"), false);
        z.sys_root = dir.join("sys-root");
        let w = |p: &str, t: &str| {
            let f = under(&z.sys_root, p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, t).unwrap();
        };
        w("/proc/meminfo", "MemTotal: 2097152 kB\nMemAvailable: 1048576 kB\n");
        w("/proc/swaps", "Filename Type Size Used Priority\n/dev/sda2 partition 1048576 0 -2\n");
        w("/sys/class/block/sda2/queue/rotational", "1\n");
        std::fs::create_dir_all(under(&z.sys_root, "/sys/module/zram")).unwrap();
        w("/sys/block/zram0/comp_algorithm", "[lz4] zstd\n");
        w("/sys/block/zram0/disksize", "0\n");
        z
    }

    #[test]
    fn state_without_zram_yet() {
        let d = tempfile::tempdir().unwrap();
        let z = fake(d.path());
        let s = z.state();
        assert!(s.supported && !s.installed && !s.external && !s.pending_restart && !s.zswap);
        assert_eq!(s.running, None);
        assert!(!s.settings.enabled);
        assert_eq!(s.recommended.size_mib, 2048);
        assert_eq!(s.recommended.algorithm, "zstd");
        assert_eq!(s.disk_swaps[0].rotational, Some(true));
        assert_eq!(s.recommended.swappiness, 100, "the overflow lands on a spinning disk");
        assert_eq!(s.max_size_mib, 4096);
    }

    #[test]
    fn a_device_from_elsewhere_is_left_alone() {
        let d = tempfile::tempdir().unwrap();
        let z = fake(d.path());
        std::fs::write(under(&z.sys_root, "/sys/block/zram0/disksize"), "1073741824\n").unwrap();
        std::fs::write(under(&z.sys_root, "/sys/block/zram0/mm_stat"), "4096 100 8192 0 8192 0 0 0 0\n").unwrap();
        let s = z.state();
        assert!(s.external);
        assert_eq!(s.settings.size_mib, 1024, "what runs is shown");
        assert!(s.settings.enabled);
        assert_eq!(s.running.as_ref().unwrap().data, 4096);
        std::fs::create_dir_all(under(&z.sys_root, "/run/systemd/generator")).unwrap();
        std::fs::write(under(&z.sys_root, GENERATED_UNIT), "").unwrap();
        assert!(!z.state().external, "the generator's own device");
    }

    #[test]
    fn support_without_the_module_loaded() {
        assert!(kernel_has_zram("CONFIG_A=y\nCONFIG_ZRAM=m\n"));
        assert!(kernel_has_zram("CONFIG_ZRAM=y\n"));
        assert!(!kernel_has_zram("# CONFIG_ZRAM is not set\nCONFIG_ZRAM_WRITEBACK=y\n"));
        let d = tempfile::tempdir().unwrap();
        let z = fake(d.path());
        std::fs::remove_dir(under(&z.sys_root, "/sys/module/zram")).unwrap();
        assert!(z.supported(), "unknown is tried");
        let w = |p: &str, t: &str| {
            let f = under(&z.sys_root, p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, t).unwrap();
        };
        w("/proc/sys/kernel/osrelease", "6.1-test\n");
        w("/boot/config-6.1-test", "# CONFIG_ZRAM is not set\n");
        assert!(!z.supported());
        w("/boot/config-6.1-test", "CONFIG_ZRAM=m\n");
        assert!(z.supported());
    }

    #[test]
    fn a_device_with_our_dropin_stays_ours() {
        let d = tempfile::tempdir().unwrap();
        let z = fake(d.path());
        std::fs::write(under(&z.sys_root, "/sys/block/zram0/disksize"), "1073741824\n").unwrap();
        let off = ZramSettings { enabled: false, ..on(1024, "lz4") };
        z.write_file(DROPIN, &render_dropin(&off, 100)).unwrap();
        let s = z.state();
        assert!(!s.external, "no generated unit, but rynas's drop-in");
        assert!(s.pending_restart, "off is saved, the device still runs");
    }

    #[test]
    fn a_device_not_saved_by_rynas_shows_the_vm_values_in_force() {
        let d = tempfile::tempdir().unwrap();
        let z = fake(d.path());
        std::fs::write(under(&z.sys_root, "/sys/block/zram0/disksize"), "1073741824\n").unwrap();
        for (k, v) in [("swappiness", "60\n"), ("page-cluster", "3\n")] {
            let f = under(&z.sys_root, &format!("/proc/sys/vm/{k}"));
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, v).unwrap();
        }
        let s = z.state().settings;
        assert_eq!((s.swappiness, s.page_cluster), (60, 3));
    }

    #[test]
    fn saved_values_waiting_for_a_restart() {
        let d = tempfile::tempdir().unwrap();
        let z = fake(d.path());
        z.write_file(DROPIN, &render_dropin(&on(1024, "zstd"), 100)).unwrap();
        assert!(z.state().pending_restart, "saved but not running");
        let m = std::fs::metadata(under(&z.etc_root, DROPIN)).unwrap().permissions().mode();
        assert_eq!(m & 0o777, 0o644);
    }
}
