//! A newer NUT than the system's, when a UPS needs it (`POST /v1/ups/update-nut`, task
//! `ups.update-nut`, D-UPS-NUT).
//!
//! rynas stays on the distribution's NUT ("Install NUT" installs Debian's package): only when
//! the setup wizard's last detection found a setting that needs a newer NUT
//! ([`rynas_api_types::ups::UpsProbeState::NeedsNewerNut`]) does `GET /v1/ups/device` offer
//! an update ([`offer`]), and only on plain NUT (a module's UPS layer brings its own NUT and
//! is never offered one). The update comes from, in this order:
//!
//! 1. the apt candidate of `nut-server` when it is new enough (a vendor repository, a
//!    pinned source);
//! 2. a `<codename>-backports` source already configured on the system (Debian's backports
//!    are `NotAutomatic`, pin 100: never the candidate, so the version table of
//!    `apt-cache policy` is read, [`parse_version_table`]) with a new enough version,
//!    installed with `-t <codename>-backports` (apt then takes the libraries from there
//!    too). rynasd never adds apt sources here (System › Updates has the admin's switch);
//! 3. rynas's own build of Debian's NUT for Debian 13 x86-64 (`packaging/nut/`, built once,
//!    attached to the fixed release [`RELEASE_TAG`]): the files of [`PINNED_DEBS`] in
//!    [`crate::dedup::host::LOCAL_PACKAGES`] when the admin copied them there, otherwise
//!    downloaded from that release ([`fetch_release`]); a file is kept only with its pinned
//!    SHA-256.
//!
//! The install is `apt-get install --no-remove` in the rynas-apt transient unit (dpkg keeps
//! changed configuration files: `--force-confold`), checked first with a simulation so a
//! system where it would remove packages (NUT's SNMP, XML… drivers of the old version) is
//! refused before anything runs. Afterwards: udev rules applied to the UPS already plugged
//! in ([`after_packages`]), failed driver units reset (the package may start NUT with the
//! old setting), the version read again and the detection report dropped, so the admin
//! detects again. `--dry-system` runs and downloads nothing and writes the dry NUT version
//! file.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use rynas_api_types::TaskStarted;
use rynas_api_types::error_codes;
use rynas_api_types::ups::{
    UpsBackend, UpsDetectReport, UpsNutSource, UpsNutUpdate, UpsProbeState,
};
use rynas_config::files;
use rynas_nut::setup as nut;
use rynas_tasks::{Cmd, TaskCtx};

use crate::dedup::host::LOCAL_PACKAGES;
use crate::ops::{Error, Res, user};
use crate::ups::{Ups, UpsState};
use crate::ups_setup::{DRY_NUT_VERSION_FILE, Runner};

/// The NUT version of rynas's build (`packaging/nut/versions.env`).
pub const BACKPORT_NUT: (u32, u32, u32) = (2, 8, 5);
/// The Debian release (`VERSION_ID` of os-release) and architecture it is built for.
pub const BACKPORT_DEBIAN: &str = "13";
pub const BACKPORT_ARCH: &str = "x86_64";

/// The packages of rynas's NUT build rynasd installs: `(file name, SHA-256)`, as
/// `packaging/nut/build.sh` writes them into `target/nut/SHA256SUMS` (reproducible: the same
/// commit of `packaging/nut` gives the same files). Built once: they change only with a new
/// NUT version or a packaging fix, together with [`RELEASE_TAG`].
pub const PINNED_DEBS: &[(&str, &str)] = &[
    (
        "nut-server_2.8.5-0~rynas13+1_amd64.deb",
        "4f0b908d3e738b7104bf154b081050da661a5eba9d2f23d3875e031a6a714c23",
    ),
    (
        "nut-client_2.8.5-0~rynas13+1_amd64.deb",
        "467f8d044ffe3d731e007d2ba0564fc7e4a0404720fbb4115c1ef71d9aadb90b",
    ),
    (
        "libupsclient7_2.8.5-0~rynas13+1_amd64.deb",
        "a634b1b261516212b2ca6c7e5bf43cdb98648cdba91747f563fda3c72e02376f",
    ),
    (
        "libnutscan4_2.8.5-0~rynas13+1_amd64.deb",
        "b713a29e95a450370e1fe57b10c11a0bfd3b548b1532b5bbac7ed580604b0a7b",
    ),
];

/// The fixed GitHub release the packages are attached to (`packaging/nut/versions.env`).
pub const RELEASE_TAG: &str = "nut-2.8.5-0rynas1";
/// Where they are downloaded from: `<RELEASE_BASE>/<asset_name(file)>`.
pub const RELEASE_BASE: &str = "https://github.com/rylos/rynas/releases/download/nut-2.8.5-0rynas1";
/// A package larger than this is not downloaded (nut-server is ~2 MB).
const MAX_DEB: u64 = 16 << 20;

/// The packages an apt source upgrades (the metapackage `nut` depends on them unversioned).
pub const APT_PACKAGES: &[&str] = &["nut-server", "nut-client"];

const APT_TIMEOUT: Duration = Duration::from_secs(1800);
const SHORT: Duration = Duration::from_secs(20);

/// Where an update would come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// apt's candidate of `nut-server` (its Debian version).
    Apt(String),
    /// A configured `<codename>-backports` source (`suite`) has `version`.
    Backports { version: String, suite: String },
    /// rynas's packages in the local folder.
    Local(Vec<PathBuf>),
    /// rynas's packages, downloaded from [`RELEASE_BASE`] first.
    Release,
}

impl Source {
    /// The apt-get arguments of the install (`[-t <suite>] install --no-remove -- …`);
    /// empty for [`Source::Release`] (download first).
    pub fn apt_args(&self) -> Vec<String> {
        let mut a: Vec<String> = Vec::new();
        let targets: Vec<String> = match self {
            Source::Apt(_) => APT_PACKAGES.iter().map(|p| p.to_string()).collect(),
            Source::Backports { suite, .. } => {
                a.extend(["-t".into(), suite.clone()]);
                APT_PACKAGES.iter().map(|p| p.to_string()).collect()
            }
            Source::Local(files) => files
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            Source::Release => return Vec::new(),
        };
        a.extend(["install".into(), "--no-remove".into(), "--".into()]);
        a.extend(targets);
        a
    }

    fn api(&self) -> UpsNutSource {
        match self {
            Source::Apt(_) => UpsNutSource::Apt,
            Source::Backports { .. } => UpsNutSource::Backports,
            Source::Local(_) | Source::Release => UpsNutSource::Rynas,
        }
    }
}

/// The highest NUT version the last detection's skipped candidates need (`None`: none was
/// skipped for its NUT version).
pub fn needed(report: Option<&UpsDetectReport>) -> Option<(u32, u32, u32)> {
    report?
        .candidates
        .iter()
        .filter(|c| c.probe.state == UpsProbeState::NeedsNewerNut)
        .filter_map(|c| c.min_nut.as_deref().and_then(nut::parse_version))
        .max()
}

/// The upstream part of a Debian version (`1:2.8.5-1~bpo13+1` → `2.8.5`).
pub fn upstream(debian: &str) -> Option<(u32, u32, u32)> {
    let v = debian.split_once(':').map_or(debian, |(_, v)| v);
    nut::parse_version(v)
}

/// `Installed:` and `Candidate:` of `apt-cache policy` (`None` for `(none)`).
pub fn parse_policy(text: &str) -> (Option<String>, Option<String>) {
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(name).map(|v| v.trim().to_string()))
            .filter(|v| !v.is_empty() && v != "(none)")
    };
    (field("Installed:"), field("Candidate:"))
}

/// One version of `apt-cache policy`'s version table and the suites it comes from
/// (`trixie-backports`, from `100 http://deb.debian.org/debian trixie-backports/main amd64
/// Packages`; `/var/lib/dpkg/status` gives none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyVersion {
    pub version: String,
    pub suites: Vec<String>,
}

/// The version table of `apt-cache policy <one package>`.
pub fn parse_version_table(text: &str) -> Vec<PolicyVersion> {
    let mut out: Vec<PolicyVersion> = Vec::new();
    let mut in_table = false;
    for line in text.lines() {
        if line.trim() == "Version table:" {
            in_table = true;
            continue;
        }
        if !in_table {
            continue;
        }
        let words: Vec<&str> = line
            .trim()
            .trim_start_matches("***")
            .split_whitespace()
            .collect();
        let numeric = |w: &str| !w.is_empty() && w.bytes().all(|b| b.is_ascii_digit() || b == b'-');
        match words.as_slice() {
            // "<version> <priority>"
            [v, p] if !numeric(v) && numeric(p) => out.push(PolicyVersion {
                version: v.to_string(),
                suites: Vec::new(),
            }),
            // "<priority> <uri> <suite>/<component> <arch> Packages"
            [p, _uri, dist, ..] if numeric(p) && dist.contains('/') => {
                if let (Some(last), Some(suite)) = (out.last_mut(), dist.split('/').next()) {
                    last.suites.push(suite.to_string());
                }
            }
            _ => {}
        }
    }
    out
}

fn valid_suite(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// From `apt-cache policy nut-server`: the candidate when it is new enough and not
/// installed, else a version from a configured `*-backports` suite that is.
pub fn source_of_policy(text: &str, need: (u32, u32, u32)) -> Option<Source> {
    let (installed, candidate) = parse_policy(text);
    let enough =
        |v: &str| installed.as_deref() != Some(v) && upstream(v).is_some_and(|u| u >= need);
    if let Some(c) = candidate.filter(|c| enough(c)) {
        return Some(Source::Apt(c));
    }
    parse_version_table(text).into_iter().find_map(|pv| {
        let suite = pv
            .suites
            .iter()
            .find(|s| s.ends_with("-backports") && valid_suite(s))?;
        enough(&pv.version).then(|| Source::Backports {
            version: pv.version.clone(),
            suite: suite.clone(),
        })
    })
}

/// `ID=debian` and `VERSION_ID="13"` in an os-release file.
pub fn debian_release(os_release: &str) -> Option<String> {
    let get = |k: &str| {
        os_release.lines().find_map(|l| {
            l.strip_prefix(k)
                .and_then(|v| v.strip_prefix('='))
                .map(|v| v.trim().trim_matches('"').to_string())
        })
    };
    (get("ID").as_deref() == Some("debian"))
        .then(|| get("VERSION_ID"))
        .flatten()
}

/// All of [`PINNED_DEBS`] in `dir` with their checksums, or `None`.
pub fn local_packages(dir: &Path, pinned: &[(&str, &str)]) -> Option<Vec<PathBuf>> {
    if pinned.is_empty() {
        return None;
    }
    pinned
        .iter()
        .map(|(name, sum)| {
            let p = dir.join(name);
            (p.is_file() && crate::dedup::host::file_sha256(&p).as_deref() == Some(*sum))
                .then_some(p)
        })
        .collect()
}

/// The release asset of a package file: GitHub turns `~` and `+` in asset names into dots
/// (`packaging/nut/build.sh` names them so in `target/nut/release/`).
pub fn asset_name(file: &str) -> String {
    file.replace(['~', '+'], ".")
}

/// Keeps a downloaded `part` as `dest` only when its SHA-256 is `sum` (rename in the same
/// folder); removes it otherwise.
pub fn keep_if_pinned(part: &Path, dest: &Path, sum: &str) -> Result<(), String> {
    let got = crate::dedup::host::file_sha256(part);
    if got.as_deref() != Some(sum) {
        let _ = std::fs::remove_file(part);
        return Err(format!(
            "{}: SHA-256 {} is not the pinned {sum}",
            dest.file_name().unwrap_or_default().to_string_lossy(),
            got.unwrap_or_else(|| "?".into())
        ));
    }
    std::fs::rename(part, dest).map_err(|e| format!("{}: {e}", dest.display()))
}

/// Downloads the files of `pinned` missing (or wrong) in `dir` from `base`, each checked
/// against its SHA-256 before it is kept; returns all of them.
pub async fn fetch_release(
    dir: &Path,
    pinned: &[(&str, &str)],
    base: &str,
    log: &(dyn Fn(&str) + Send + Sync),
) -> Result<Vec<PathBuf>, String> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    use tokio::io::AsyncWriteExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .https_only(true)
        .user_agent(concat!("rynas/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for (name, sum) in pinned {
        let dest = dir.join(name);
        if dest.is_file() && crate::dedup::host::file_sha256(&dest).as_deref() == Some(*sum) {
            out.push(dest);
            continue;
        }
        let url = format!("{base}/{}", asset_name(name));
        log(&format!("downloading {url}"));
        let fail = |m: String| {
            format!(
                "cannot download {url}: {m} (no network? copy the packages of the release {RELEASE_TAG} to {})",
                dir.display()
            )
        };
        let mut resp = client
            .get(&url)
            .timeout(Duration::from_secs(300))
            .send()
            .await
            .map_err(|e| fail(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(fail(format!("HTTP {}", resp.status().as_u16())));
        }
        if resp.content_length().is_some_and(|n| n > MAX_DEB) {
            return Err(fail(format!("larger than {MAX_DEB} bytes")));
        }
        let part = dir.join(format!(".{name}.part"));
        let std_file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&part)
            .map_err(|e| format!("{}: {e}", part.display()))?;
        let mut f = tokio::fs::File::from_std(std_file);
        let mut n: u64 = 0;
        let body: Result<(), String> = async {
            while let Some(chunk) = resp.chunk().await.map_err(|e| fail(e.to_string()))? {
                n += chunk.len() as u64;
                if n > MAX_DEB {
                    return Err(fail(format!("larger than {MAX_DEB} bytes")));
                }
                f.write_all(&chunk).await.map_err(|e| e.to_string())?;
            }
            f.sync_all().await.map_err(|e| e.to_string())
        }
        .await;
        if let Err(e) = body {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
        keep_if_pinned(&part, &dest, sum)?;
        out.push(dest);
    }
    Ok(out)
}

/// The apt source of a new enough NUT (candidate, then a configured backports suite).
fn apt_source(runner: &dyn Runner, need: (u32, u32, u32)) -> Option<Source> {
    let o = runner
        .run(
            &Cmd::new("apt-cache")
                .args(["policy", "--", "nut-server"])
                .env_clear()
                .env("LC_ALL", "C")
                .timeout(SHORT),
        )
        .ok()
        .filter(|o| o.success())?;
    source_of_policy(&o.stdout, need)
}

/// Whether rynas's build fits this system (Debian 13 on x86-64).
fn backport_fits(root: &Path) -> bool {
    let os = std::fs::read_to_string(files::under_root(root, Path::new("/etc/os-release")))
        .unwrap_or_default();
    debian_release(&os).as_deref() == Some(BACKPORT_DEBIAN)
        && std::env::consts::ARCH == BACKPORT_ARCH
}

/// What `GET /v1/ups/device` offers: `None` with a module layer, without NUT, when no
/// candidate needed a newer NUT, or when neither apt nor rynas's build has one.
/// Called only when the last report asks for a newer NUT (apt-cache and checksums then).
pub fn offer(u: &Ups) -> Option<(UpsNutUpdate, Option<Source>)> {
    let report = u.setup.report.lock().unwrap().clone();
    offer_for(
        u.layer.is_some() || u.backend() != UpsBackend::Nut,
        report.as_ref(),
        u.dry,
        u.setup.runner.as_ref(),
        &u.root,
        PINNED_DEBS,
    )
}

/// [`offer`] on its inputs: `excluded` (a module layer, or no NUT), the last report,
/// `--dry-system`, the runner of `apt-cache`, the root of `/etc/os-release` and of the
/// package folder, the pinned files.
pub fn offer_for(
    excluded: bool,
    report: Option<&UpsDetectReport>,
    dry: bool,
    runner: &dyn Runner,
    root: &Path,
    pinned: &[(&str, &str)],
) -> Option<(UpsNutUpdate, Option<Source>)> {
    if excluded {
        return None;
    }
    let need = needed(report)?;
    let dir = LOCAL_PACKAGES.to_string();
    if dry {
        // nothing is read from the host: rynas's build from the release, simulated
        let s = Source::Release;
        return Some((
            UpsNutUpdate {
                version: nut::fmt_version(BACKPORT_NUT),
                source: Some(s.api()),
                dir,
            },
            Some(s),
        ));
    }
    if let Some(s) = apt_source(runner, need) {
        let v = match &s {
            Source::Apt(v) | Source::Backports { version: v, .. } => v.clone(),
            _ => unreachable!(),
        };
        let version = upstream(&v).map(nut::fmt_version).unwrap_or(v);
        return Some((
            UpsNutUpdate {
                version,
                source: Some(s.api()),
                dir,
            },
            Some(s),
        ));
    }
    if need > BACKPORT_NUT || !backport_fits(root) {
        return None;
    }
    // the copy the admin put in the folder, else the release download
    let s = local_packages(&files::under_root(root, Path::new(LOCAL_PACKAGES)), pinned)
        .map_or(Source::Release, Source::Local);
    Some((
        UpsNutUpdate {
            version: nut::fmt_version(BACKPORT_NUT),
            source: Some(s.api()),
            dir,
        },
        Some(s),
    ))
}

/// After NUT's packages changed: udev rules (`62-nut-usbups.rules`, which give the USB UPS to
/// the `nut` group) applied to a UPS already plugged in, otherwise its device stays
/// root-only and the drivers fail with "insufficient permissions"; then failed driver units
/// reset (the package may have started NUT with the old setting). Failures are logged: a
/// replug of the UPS does the same as the trigger.
pub async fn after_packages(ctx: &TaskCtx) {
    for c in [
        Cmd::new("udevadm").args(["control", "--reload-rules"]),
        Cmd::new("udevadm").args(["trigger", "--subsystem-match=usb", "--action=change"]),
        Cmd::new("udevadm").args(["settle", "--timeout=30"]),
        Cmd::new("systemctl").args(["reset-failed", "nut-driver@*.service", "nut-driver.service"]),
    ] {
        if let Err(e) = ctx
            .run(&c.env_clear().timeout(Duration::from_secs(40)))
            .await
        {
            ctx.log(&format!("warning: {e}"));
        }
    }
}

pub fn router(st: UpsState) -> Router {
    Router::new()
        .route("/v1/ups/update-nut", post(update_nut))
        .with_state(st)
}

fn conflict(code: &'static str, msg: String) -> Error {
    Error::new(StatusCode::CONFLICT, code, msg)
}

/// Why the update cannot start now (`Ok`: it can, from that source).
pub fn refusal(u: &Ups, who: &str) -> Res<(String, Source)> {
    u.ops.admins.require_admin(who, "NUT update")?;
    if u.layer.is_some() {
        return Err(conflict(
            error_codes::CONFLICT,
            "the UPS of this machine is run by its hardware module".into(),
        ));
    }
    if u.backend() != UpsBackend::Nut {
        return Err(conflict(
            error_codes::CONFLICT,
            "NUT is not installed: install it first".into(),
        ));
    }
    let Some((info, source)) = offer(u) else {
        return Err(conflict(
            error_codes::CONFLICT,
            "no UPS setting of the last detection needs a NUT that rynas can install".into(),
        ));
    };
    let Some(source) = source else {
        return Err(conflict(
            error_codes::CONFLICT,
            format!(
                "rynas's NUT {} packages are not in {LOCAL_PACKAGES}",
                info.version
            ),
        ));
    };
    Ok((info.version, source))
}

/// apt's simulation of the install: `Err` with apt's reason when it would fail or remove
/// packages.
fn simulate(source: &Source) -> Result<(), String> {
    let o = Cmd::new("apt-get")
        .args(["-s", "-q", "-o", "Debug::NoLocking=true"])
        .args(source.apt_args())
        .env_clear()
        .env("LC_ALL", "C")
        .timeout(Duration::from_secs(120))
        .output_blocking()
        .map_err(|e| e.to_string())?;
    if o.success() {
        return Ok(());
    }
    let why: Vec<&str> = o
        .stderr
        .lines()
        .chain(o.stdout.lines())
        .map(str::trim)
        .filter(|l| l.starts_with("E:") || l.contains("Depends:") || l.contains("Breaks:"))
        .take(6)
        .collect();
    Err(if why.is_empty() {
        "apt cannot install the packages".into()
    } else {
        why.join("; ")
    })
}

/// `POST /v1/ups/update-nut` → 202 (task `ups.update-nut`).
async fn update_nut(State(st): State<UpsState>, hd: HeaderMap) -> Res<Response> {
    let u = st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "configuration and tasks are not enabled in this rynasd".into(),
        )
    })?;
    let who = user(&hd)?;
    let u2 = u.clone();
    let who2 = who.clone();
    let (version, source) = tokio::task::spawn_blocking(move || {
        let (v, s) = refusal(&u2, &who2)?;
        // a download is simulated in the task, once the files are there
        if !u2.dry && s != Source::Release {
            simulate(&s).map_err(|e| {
                conflict(
                    error_codes::CONFLICT,
                    format!("apt would not install NUT {v}: {e}"),
                )
            })?;
        }
        Ok::<_, Error>((v, s))
    })
    .await
    .map_err(Error::internal)??;
    tracing::info!(target: "rynas::audit", "ups update of NUT to {version} ({source:?}) by {who}");
    let u2 = u.clone();
    let id = u.ops.tasks.spawn("ups.update-nut", &who, move |ctx| {
        run(u2, ctx, version, source)
    })?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

async fn run(u: Arc<Ups>, ctx: TaskCtx, version: String, source: Source) -> Result<(), String> {
    if u.dry {
        if source == Source::Release {
            ctx.log(&format!("dry system, not downloaded: {RELEASE_BASE}/…"));
        }
        let shown = match &source {
            Source::Release => Source::Local(
                PINNED_DEBS
                    .iter()
                    .map(|(n, _)| Path::new(LOCAL_PACKAGES).join(n))
                    .collect(),
            ),
            s => s.clone(),
        };
        let args = shown.apt_args();
        ctx.log(&format!("dry system, not run: apt-get {}", args.join(" ")));
        tokio::time::sleep(Duration::from_millis(600)).await;
        let f = files::under_root(&u.root, Path::new(DRY_NUT_VERSION_FILE));
        std::fs::write(&f, format!("{version}\n")).map_err(|e| format!("dry root: {e}"))?;
    } else {
        let source = if source == Source::Release {
            let dir = files::under_root(&u.root, Path::new(LOCAL_PACKAGES));
            let log = |l: &str| ctx.log(l);
            let files = fetch_release(&dir, PINNED_DEBS, RELEASE_BASE, &log).await?;
            let s = Source::Local(files);
            let s2 = s.clone();
            tokio::task::spawn_blocking(move || simulate(&s2))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| format!("apt would not install NUT {version}: {e}"))?;
            s
        } else {
            source
        };
        ctx.log(&format!(
            "updating NUT to {version} ({})",
            match &source {
                Source::Apt(v) => format!("apt, {v}"),
                Source::Backports { version, suite } => format!("{suite}, {version}"),
                _ => format!("rynas's packages in {LOCAL_PACKAGES}"),
            }
        ));
        let args = source.apt_args();
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        ctx.run(&crate::updates::apt(&refs, APT_TIMEOUT))
            .await
            .map_err(|e| e.to_string())?;
        after_packages(&ctx).await;
    }
    let u2 = u.clone();
    let now = tokio::task::spawn_blocking(move || {
        crate::ups_setup::read_version(u2.setup.runner.as_ref())
    })
    .await
    .map_err(|e| e.to_string())?;
    *u.setup.version.lock().unwrap() = now.map(nut::fmt_version);
    let want = nut::parse_version(&version).unwrap_or(BACKPORT_NUT);
    match now {
        Some(v) if v >= want => {
            // the report said "needs a newer NUT": detect again
            *u.setup.report.lock().unwrap() = None;
            ctx.log(&format!(
                "NUT {} installed: detect the UPS again",
                nut::fmt_version(v)
            ));
            Ok(())
        }
        Some(v) => Err(format!(
            "NUT is still {} after the update",
            nut::fmt_version(v)
        )),
        None => Err("the NUT version cannot be read after the update".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::ups::{UpsCandidate, UpsCandidateSource, UpsDevice, UpsProbe};

    #[test]
    fn versions_and_policy() {
        assert_eq!(upstream("2.8.5-1~rynas13+1"), Some((2, 8, 5)));
        assert_eq!(upstream("1:2.8.6-2"), Some((2, 8, 6)));
        assert_eq!(upstream("2.8.1-5+deb13u1"), Some((2, 8, 1)));
        let p = "nut-server:\n  Installed: 2.8.1-5+deb13u1\n  Candidate: 2.8.5-1~bpo13+1\n  Version table:\n";
        assert_eq!(
            parse_policy(p),
            (
                Some("2.8.1-5+deb13u1".into()),
                Some("2.8.5-1~bpo13+1".into())
            )
        );
        assert_eq!(
            parse_policy("nut-server:\n  Installed: (none)\n  Candidate: (none)\n"),
            (None, None)
        );
        assert_eq!(
            debian_release(
                "PRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\nVERSION_ID=\"13\"\nID=debian\n"
            )
            .as_deref(),
            Some("13")
        );
        assert_eq!(debian_release("ID=ubuntu\nVERSION_ID=\"24.04\"\n"), None);
        assert!(BACKPORT_NUT > (2, 8, 1));
    }

    #[test]
    fn needed_is_the_highest_skipped_version() {
        let cand = |state, min: Option<&str>| UpsCandidate {
            device: UpsDevice {
                driver: "nutdrv_qx".into(),
                port: "auto".into(),
                options: vec![],
            },
            source: UpsCandidateSource::Known,
            min_nut: min.map(str::to_string),
            probe: UpsProbe {
                state,
                ..Default::default()
            },
        };
        let mut r = UpsDetectReport {
            candidates: vec![
                cand(UpsProbeState::Failed, Some("2.8.6")),
                cand(UpsProbeState::NeedsNewerNut, Some("2.8.5")),
            ],
            ..Default::default()
        };
        assert_eq!(needed(Some(&r)), Some((2, 8, 5)));
        r.candidates[1].probe.state = UpsProbeState::Failed;
        assert_eq!(needed(Some(&r)), None);
        assert_eq!(needed(None), None);
    }

    /// `apt-cache policy` on Debian 13 with trixie-backports configured: the layout of a real
    /// output (`cockpit-ws`, 2026-11-11), with NUT's versions.
    const BACKPORTS_POLICY: &str = "nut-server:
  Installed: 2.8.1-5
  Candidate: 2.8.1-5+deb13u1
  Version table:
     2.8.5-1~bpo13+1 100
        100 http://deb.debian.org/debian trixie-backports/main amd64 Packages
     2.8.1-5+deb13u1 500
        500 http://deb.debian.org/debian trixie/main amd64 Packages
        500 http://deb.debian.org/debian-security trixie-security/main amd64 Packages
 *** 2.8.1-5 100
        100 /var/lib/dpkg/status
";

    #[test]
    fn version_table_and_backports() {
        let t = parse_version_table(BACKPORTS_POLICY);
        assert_eq!(
            t,
            [
                PolicyVersion {
                    version: "2.8.5-1~bpo13+1".into(),
                    suites: vec!["trixie-backports".into()]
                },
                PolicyVersion {
                    version: "2.8.1-5+deb13u1".into(),
                    suites: vec!["trixie".into(), "trixie-security".into()]
                },
                PolicyVersion {
                    version: "2.8.1-5".into(),
                    suites: vec![]
                },
            ]
        );
        assert_eq!(
            source_of_policy(BACKPORTS_POLICY, (2, 8, 5)),
            Some(Source::Backports {
                version: "2.8.5-1~bpo13+1".into(),
                suite: "trixie-backports".into()
            })
        );
        // a candidate new enough wins over backports
        let cand = BACKPORTS_POLICY.replace("Candidate: 2.8.1-5+deb13u1", "Candidate: 2.8.6-1");
        assert_eq!(
            source_of_policy(&cand, (2, 8, 5)),
            Some(Source::Apt("2.8.6-1".into()))
        );
        // already installed from backports: nothing
        let inst = BACKPORTS_POLICY.replace("Installed: 2.8.1-5", "Installed: 2.8.5-1~bpo13+1");
        assert_eq!(source_of_policy(&inst, (2, 8, 5)), None);
        // backports too old: nothing
        assert_eq!(source_of_policy(BACKPORTS_POLICY, (2, 8, 6)), None);
        // epoch versions in the table
        let golang = "golang-go:\n  Installed: (none)\n  Candidate: 2:1.24~2\n  Version table:\n     2:1.27~1~bpo13+1 100\n        100 http://deb.debian.org/debian trixie-backports/main amd64 Packages\n     2:1.24~2 500\n        500 http://deb.debian.org/debian trixie/main amd64 Packages\n";
        assert_eq!(parse_version_table(golang)[0].version, "2:1.27~1~bpo13+1");
    }

    #[test]
    fn release_assets_and_checksums() {
        assert_eq!(
            asset_name("nut-server_2.8.5-0~rynas13+1_amd64.deb"),
            "nut-server_2.8.5-0.rynas13.1_amd64.deb"
        );
        assert!(
            RELEASE_BASE.ends_with(RELEASE_TAG)
                && RELEASE_BASE.starts_with("https://github.com/rylos/rynas/releases/download/")
        );
        let env = include_str!("../../../packaging/nut/versions.env");
        assert!(
            env.lines()
                .any(|l| l == format!("RELEASE_TAG={RELEASE_TAG}"))
        );
        let d = tempfile::tempdir().unwrap();
        let part = d.path().join(".a.deb.part");
        std::fs::write(&part, b"a").unwrap();
        let a = "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb";
        let b = "3e23e8160039594a33894f6564e1b1348bbd7a0088d42c4acb73eeaed59c009d";
        assert!(
            keep_if_pinned(&part, &d.path().join("a.deb"), b)
                .unwrap_err()
                .contains("not the pinned")
        );
        assert!(!part.exists(), "a wrong download is removed");
        std::fs::write(&part, b"a").unwrap();
        keep_if_pinned(&part, &d.path().join("a.deb"), a).unwrap();
        assert!(d.path().join("a.deb").is_file() && !part.exists());
    }

    /// `apt-cache policy` answering with this text.
    struct Policy(&'static str);

    impl Runner for Policy {
        fn run(&self, cmd: &Cmd) -> Result<rynas_tasks::CmdOutput, String> {
            assert!(
                cmd.display().contains("apt-cache policy -- nut-server"),
                "{}",
                cmd.display()
            );
            Ok(rynas_tasks::CmdOutput {
                code: Some(0),
                stdout: self.0.into(),
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn offer_prefers_apt_then_rynas_packages_and_never_with_a_layer() {
        let report = UpsDetectReport {
            nut_version: Some("2.8.1".into()),
            candidates: vec![UpsCandidate {
                device: UpsDevice {
                    driver: "nutdrv_qx".into(),
                    port: "auto".into(),
                    options: vec![],
                },
                source: UpsCandidateSource::Known,
                min_nut: Some("2.8.5".into()),
                probe: UpsProbe {
                    state: UpsProbeState::NeedsNewerNut,
                    ..Default::default()
                },
            }],
            ..Default::default()
        };
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("etc")).unwrap();
        std::fs::write(
            root.path().join("etc/os-release"),
            "ID=debian\nVERSION_ID=\"13\"\n",
        )
        .unwrap();
        let pkgs = root.path().join("var/lib/rynas/packages");
        std::fs::create_dir_all(&pkgs).unwrap();
        let none = Policy("nut-server:\n  Installed: 2.8.1-5\n  Candidate: 2.8.1-5\n");
        let pinned = [(
            "nut-server_x.deb",
            "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb",
        )];
        // a module layer or no NUT: nothing, whatever the sources
        assert_eq!(
            offer_for(true, Some(&report), false, &none, root.path(), &pinned),
            None
        );
        // nothing needs a newer NUT: nothing
        assert_eq!(
            offer_for(false, None, false, &none, root.path(), &pinned),
            None
        );
        // an apt source with 2.8.5: apt first
        let apt = Policy("nut-server:\n  Installed: 2.8.1-5\n  Candidate: 2.8.5-1~bpo13+1\n");
        let (info, src) =
            offer_for(false, Some(&report), false, &apt, root.path(), &pinned).unwrap();
        assert_eq!(
            (info.version.as_str(), info.source),
            ("2.8.5", Some(UpsNutSource::Apt))
        );
        assert_eq!(
            src.unwrap().apt_args(),
            ["install", "--no-remove", "--", "nut-server", "nut-client"]
        );
        // only a configured backports suite has it: apt -t trixie-backports
        let bp = Policy(BACKPORTS_POLICY);
        let (info, src) =
            offer_for(false, Some(&report), false, &bp, root.path(), &pinned).unwrap();
        assert_eq!(
            (info.version.as_str(), info.source),
            ("2.8.5", Some(UpsNutSource::Backports))
        );
        assert_eq!(
            src.unwrap().apt_args(),
            [
                "-t",
                "trixie-backports",
                "install",
                "--no-remove",
                "--",
                "nut-server",
                "nut-client"
            ]
        );
        // apt only has 2.8.1: rynas's build, downloaded from the release
        let (info, src) =
            offer_for(false, Some(&report), false, &none, root.path(), &pinned).unwrap();
        if cfg!(target_arch = "x86_64") {
            assert_eq!(
                (info.source, src),
                (Some(UpsNutSource::Rynas), Some(Source::Release))
            );
            assert_eq!(info.dir, LOCAL_PACKAGES);
            // the pinned file copied by the admin with its checksum: that one
            std::fs::write(pkgs.join("nut-server_x.deb"), b"a").unwrap();
            let (info, src) =
                offer_for(false, Some(&report), false, &none, root.path(), &pinned).unwrap();
            assert_eq!(info.source, Some(UpsNutSource::Rynas));
            let p = pkgs.join("nut-server_x.deb").to_string_lossy().into_owned();
            assert_eq!(
                src.unwrap().apt_args(),
                ["install", "--no-remove", "--", p.as_str()]
            );
        }
        // another release: nothing rynas can install
        std::fs::write(
            root.path().join("etc/os-release"),
            "ID=debian\nVERSION_ID=\"12\"\n",
        )
        .unwrap();
        assert_eq!(
            offer_for(false, Some(&report), false, &none, root.path(), &pinned),
            None
        );
        // a setting needing more than rynas's build: nothing
        let mut newer = report.clone();
        newer.candidates[0].min_nut = Some("2.9.0".into());
        assert_eq!(
            offer_for(false, Some(&newer), false, &none, root.path(), &pinned),
            None
        );
        // --dry-system: simulated rynas's build (from the release)
        assert_eq!(
            offer_for(false, Some(&report), true, &none, root.path(), &pinned)
                .unwrap()
                .0
                .source,
            Some(UpsNutSource::Rynas)
        );
    }

    #[test]
    fn local_packages_need_every_file_with_its_checksum() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.deb"), b"a").unwrap();
        std::fs::write(d.path().join("b.deb"), b"b").unwrap();
        // sha256("a"), sha256("b")
        let a = "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb";
        let b = "3e23e8160039594a33894f6564e1b1348bbd7a0088d42c4acb73eeaed59c009d";
        assert_eq!(
            local_packages(d.path(), &[("a.deb", a), ("b.deb", b)]).map(|v| v.len()),
            Some(2)
        );
        assert_eq!(
            local_packages(d.path(), &[("a.deb", a), ("b.deb", a)]),
            None,
            "wrong checksum"
        );
        assert_eq!(
            local_packages(d.path(), &[("a.deb", a), ("c.deb", a)]),
            None,
            "missing file"
        );
        assert_eq!(local_packages(d.path(), &[]), None);
        // the pinned list is complete: one file per package of packaging/nut/versions.env
        let env = include_str!("../../../packaging/nut/versions.env");
        let pkgs = env
            .lines()
            .find_map(|l| l.strip_prefix("PACKAGES="))
            .unwrap()
            .trim_matches('"');
        let rev = env
            .lines()
            .find_map(|l| l.strip_prefix("PKG_REVISION="))
            .unwrap();
        let ver = env
            .lines()
            .find_map(|l| l.strip_prefix("NUT_DEBIAN_VERSION="))
            .unwrap();
        let up = ver.split('-').next().unwrap();
        for p in pkgs.split_whitespace() {
            let prefix = format!("{p}_{up}-0~rynas13+{rev}_");
            assert!(
                PINNED_DEBS
                    .iter()
                    .any(|(n, s)| n.starts_with(&prefix) && s.len() == 64),
                "{p} not pinned"
            );
        }
        assert_eq!(PINNED_DEBS.len(), pkgs.split_whitespace().count());
        assert_eq!(nut::parse_version(ver), Some(BACKPORT_NUT));
    }
}
