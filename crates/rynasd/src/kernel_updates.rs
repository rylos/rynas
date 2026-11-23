//! When new kernels are installed (`updates.kernel`, public under `/api/v1/updates/kernel/…`).
//!
//! - **Holds**: in every mode but `system` the kernel metapackages are held
//!   ([`rynas_updates::kernel_policy`]), so neither "Install all", nor unattended-upgrades,
//!   nor apt by hand installs a newer kernel. The configuration domain puts the holds in place
//!   when the mode changes; rynasd does it at start too ([`ensure_holds`]: the default on a
//!   machine whose module ships its kernels is manual, without any apply), and after every
//!   kernel task.
//! - **Channel** (`GET /v1/updates/kernel/channel[?refresh=1]`): the series (a module's kernel
//!   channel names them; else the installed image metapackage is the only one), the version
//!   of the series' image metapackage installed and the newer kernels. apt is the truth for
//!   what can be installed (`apt-cache policy` of the image metapackage); the publisher's feed
//!   (format 1, read at most every 6 hours, a refresh at most once a minute) adds the dates,
//!   the change logs, the publisher's own changes and the CVEs, and the older newer kernels
//!   (how far behind the machine is). Without a feed a kernel's date is when rynas first saw
//!   it (state file `kernel-updates.json`).
//! - **Install** (`POST …/install {version}`, task `updates.kernel`): the version the page
//!   showed must still be apt's candidate; the series' installed metapackages are let go,
//!   installed (`apt-get install`, nothing may be removed) and held again; then the module's
//!   step after a kernel change runs (a boot flash written by its tool).
//! - **Series** (`POST …/series {series}`, task `updates.kernel-series`): the target series'
//!   image metapackage (and headers when some are installed) is installed; going to an older
//!   branch also removes the metapackages of the newer ones (the machine boots the newest
//!   series installed). Then the module's step, the holds.
//! - **Notify** (mode `notify`): the daily updates check emits `updates.kernel_available` for
//!   a newer kernel not notified yet, at most every `interval_days`.
//! - **Auto** (mode `auto`, scheduler task `kernel-update` in the window of the automatic
//!   updates): installs apt's candidate once it is `min_age_days` old and `interval_days`
//!   after the last kernel rynas installed, then restarts at the reboot time of the automatic
//!   updates (`shutdown -r HH:MM`) when the policy says so. A notification either way.
//!
//! `--dry-system`: apt is never run; the metapackages and the candidate are fixtures, the feed
//! is the sample one, the tasks only log.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use axum::extract::rejection::JsonRejection;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::error_codes;
use rynas_api_types::notify::{EventAction, NotifyCategory, NotifySeverity};
use rynas_api_types::updates::{
    AutoUpdates, KernelChannelState, KernelFeed, KernelInstallRequest, KernelMode, KernelOffer,
    KernelPolicy, KernelPolicyState, KernelSeriesRequest, KernelSeriesState,
};
use rynas_notify::event::{code, field, oneshot};
use rynas_tasks::{Cmd, TaskCtx};
use rynas_updates::apt::parse_simulation;
use rynas_updates::kernel_policy::{
    Installed, apt_mark, auto_due, compare_versions, depends_meta, depends_release, hold_plan,
    mode_name, notify_due, parse_feed, policy_candidate, read_metas, utc_ms,
};
use rynas_updates::{AUTO_KEY, KERNEL_KEY};
use serde::{Deserialize, Serialize};

use crate::ops::{Error, Res, user};
use crate::services::stage;
use crate::updates::{SVC, UpdState, Updates, apt};

const APT_CACHE: &str = "/usr/bin/apt-cache";
const FEED_TTL: Duration = Duration::from_secs(6 * 3600);
const FEED_MIN_REFRESH: Duration = Duration::from_secs(60);
const FEED_LIMIT: usize = 8 << 20;
const FEED_TIMEOUT: Duration = Duration::from_secs(30);
const STATE_FILE: &str = "kernel-updates.json";
const NOTIFY_KEY: &str = "updates:kernel";
const DRY_FEED: &str = include_str!("../../rynas-updates/src/fixtures/kernels.json");

/// The feed last read.
#[derive(Clone)]
pub struct FeedCache {
    at: Instant,
    read_ms: u64,
    feed: Option<KernelFeed>,
    error: Option<String>,
}

/// What rynas remembers between runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct KState {
    /// Version of an image metapackage → when rynas first saw it as apt's candidate.
    #[serde(default)]
    first_seen: BTreeMap<String, u64>,
    #[serde(default)]
    last_install_ms: Option<u64>,
    /// (when, version) of the last "newer kernel" notification.
    #[serde(default)]
    last_notified: Option<(u64, String)>,
}

/// A series with owned names.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Series {
    id: String,
    image: String,
    headers: String,
    followers: Vec<String>,
}

impl Series {
    fn names(&self) -> impl Iterator<Item = &String> {
        [&self.image, &self.headers].into_iter().chain(self.followers.iter())
    }

    fn owns(&self, name: &str) -> bool {
        self.names().any(|n| n == name)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The series of this machine: the module's, else the installed image metapackage alone.
fn series_of(u: &Updates, metas: &[Installed]) -> Vec<Series> {
    if let Some(c) = &u.channel {
        return c
            .series()
            .iter()
            .map(|s| Series {
                id: s.id.into(),
                image: s.image.into(),
                headers: s.headers.into(),
                followers: s.followers.iter().map(|f| f.to_string()).collect(),
            })
            .collect();
    }
    let Some(image) = metas.iter().find(|m| m.name.starts_with("linux-image-")) else {
        return Vec::new();
    };
    let flavour = image.name.trim_start_matches("linux-image-");
    let headers = format!("linux-headers-{flavour}");
    vec![Series {
        id: "default".into(),
        image: image.name.clone(),
        followers: metas.iter().map(|m| m.name.clone()).filter(|n| *n != image.name && *n != headers).collect(),
        headers,
    }]
}

/// The series the machine boots: the last one whose image metapackage is installed.
fn current(series: &[Series], metas: &[Installed]) -> Option<usize> {
    series.iter().rposition(|s| metas.iter().any(|m| m.name == s.image))
}

impl Updates {
    fn kstate_path(&self) -> PathBuf {
        self.state_dir.join(STATE_FILE)
    }

    fn kstate(&self) -> KState {
        std::fs::read_to_string(self.kstate_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn kstate_update(&self, f: impl FnOnce(&mut KState)) {
        let _g = self.kstate_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = self.kstate();
        let before = s.clone();
        f(&mut s);
        if s == before {
            return;
        }
        let path = self.kstate_path();
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(&s).unwrap_or_default();
        if let Err(e) = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &path)) {
            tracing::warn!("kernel state {}: {e}", path.display());
        }
    }

    /// The policy in force (the default for this machine when never applied).
    pub(crate) fn kernel_policy(&self) -> KernelPolicy {
        match self.ops.config.current(KERNEL_KEY) {
            Ok(Some(v)) => serde_json::from_value(v).unwrap_or_else(|_| self.kernel_default()),
            _ => self.kernel_default(),
        }
    }

    fn kernel_default(&self) -> KernelPolicy {
        KernelPolicy::default_for(self.channel.is_some())
    }

    /// The installed kernel metapackages.
    fn metas(&self) -> Vec<Installed> {
        if self.dry {
            let held = self.kernel_policy().holds();
            let (name, version, release) = if self.channel.is_some() {
                ("linux-image-synodeb-stable", "7.3.4-11", "7.3.4-11-synodeb")
            } else {
                ("linux-image-amd64", "6.12.48-1", "6.12.48+deb13-amd64")
            };
            return vec![Installed {
                name: name.into(),
                version: version.into(),
                held,
                release: Some(release.into()),
            }];
        }
        read_metas(std::path::Path::new("/"))
    }

    /// Metapackages held now by the policy (none in mode `system`).
    pub(crate) fn kernel_held(&self) -> Vec<String> {
        if !self.kernel_policy().holds() {
            return Vec::new();
        }
        self.metas().into_iter().filter(|m| m.held).map(|m| m.name).collect()
    }

    async fn apt_text(&self, args: &[&str]) -> Option<String> {
        let o = Cmd::new(APT_CACHE)
            .args(args)
            .env_clear()
            .env("LC_ALL", "C")
            .timeout(Duration::from_secs(60))
            .run(None, None)
            .await
            .ok()?;
        o.success().then_some(o.stdout)
    }

    /// apt's candidate of an image metapackage: (version, kernel release).
    async fn candidate(&self, meta: &str) -> Option<(String, Option<String>)> {
        if self.dry {
            return Some(if self.channel.is_some() {
                ("7.3.5-13".into(), Some("7.3.5-13-synodeb".into()))
            } else {
                ("6.12.57-1".into(), Some("6.12.57+deb13-amd64".into()))
            });
        }
        let version = policy_candidate(&self.apt_text(&["policy", "--", meta]).await?)?;
        let mut stanza = self.apt_text(&["show", "--", &format!("{meta}={version}")]).await.unwrap_or_default();
        // an alias depends on its series' metapackage: one step down
        if depends_release(&stanza).is_none()
            && let Some(next) = depends_meta(&stanza)
        {
            stanza = self.apt_text(&["show", "--", &format!("{next}={version}")]).await.unwrap_or_default();
        }
        Some((version, depends_release(&stanza)))
    }

    /// The feed: cached, read again after [`FEED_TTL`] or on a refresh (at most once a
    /// minute). A failed read keeps the last good feed with the error.
    async fn read_feed(&self, refresh: bool) -> Option<FeedCache> {
        let url = self.channel.as_ref()?.feed_url()?;
        let mut g = self.feed.lock().await;
        let max = if refresh { FEED_MIN_REFRESH } else { FEED_TTL };
        if let Some(c) = g.as_ref().filter(|c| c.at.elapsed() < max) {
            return Some(c.clone());
        }
        let got = if self.dry { parse_feed(DRY_FEED) } else { fetch_feed(url).await };
        let prev = g.as_ref().and_then(|c| c.feed.clone());
        let c = match got {
            Ok(f) => FeedCache { at: Instant::now(), read_ms: now_ms(), feed: Some(f), error: None },
            Err(e) => {
                tracing::warn!("kernel feed: {e}");
                FeedCache { at: Instant::now(), read_ms: now_ms(), feed: prev, error: Some(e) }
            }
        };
        *g = Some(c.clone());
        Some(c)
    }

    /// The kernels this machine can install.
    pub(crate) async fn channel_state(&self, refresh: bool) -> KernelChannelState {
        let policy = self.kernel_policy();
        let metas = self.metas();
        let series = series_of(self, &metas);
        let cur = current(&series, &metas);
        let mut st = KernelChannelState {
            module: self.channel.is_some(),
            policy,
            metapackages: metas.iter().map(|m| m.name.clone()).collect(),
            held: metas.iter().filter(|m| m.held).map(|m| m.name.clone()).collect(),
            ..Default::default()
        };
        let Some(ci) = cur.filter(|_| self.apt_present()) else {
            return st;
        };
        st.supported = true;
        let cs = &series[ci];
        let installed = metas.iter().find(|m| m.name == cs.image).cloned();
        let iv = installed.as_ref().map(|m| m.version.clone()).unwrap_or_default();
        st.installed_version = installed.as_ref().map(|m| m.version.clone());
        st.installed_release = installed.and_then(|m| m.release);
        let feed = self.read_feed(refresh).await;
        let branches: BTreeMap<String, Option<String>> = feed
            .as_ref()
            .and_then(|c| c.feed.as_ref())
            .map(|f| f.series.iter().map(|(k, s)| (k.clone(), s.branch.clone())).collect())
            .unwrap_or_default();
        st.series = series
            .iter()
            .enumerate()
            .map(|(i, s)| KernelSeriesState {
                id: s.id.clone(),
                branch: branches.get(&s.id).cloned().flatten(),
                image: s.image.clone(),
                installed: metas.iter().any(|m| m.name == s.image),
                current: i == ci,
            })
            .collect();
        if let Some(c) = &feed {
            st.feed_error = c.error.clone();
            st.feed_read_ms = Some(c.read_ms);
        }
        let cand = self.candidate(&cs.image).await.filter(|(v, _)| compare_versions(v, &iv).is_gt());
        let ks = self.kstate();
        // the feed's kernels of this series newer than the installed one
        let mut newer: Vec<KernelOffer> = feed
            .as_ref()
            .and_then(|c| c.feed.as_ref())
            .map(|f| {
                f.kernels
                    .iter()
                    .filter(|k| k.series == cs.id && compare_versions(&k.version, &iv).is_gt())
                    .map(|k| KernelOffer {
                        release: k.release.clone(),
                        version: k.version.clone(),
                        available: cand.as_ref().is_some_and(|(v, _)| *v == k.version),
                        published_ms: k.published.as_deref().and_then(utc_ms),
                        published_known: k.published.is_some(),
                        upstream: k.upstream.clone(),
                        changelog: k.changelog.clone(),
                        changes: k.changes.clone(),
                        cves: k.cves.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Some((v, rel)) = &cand {
            let seen = *ks.first_seen.get(v).unwrap_or(&now_ms());
            if !ks.first_seen.contains_key(v) {
                let v = v.clone();
                self.kstate_update(|s| {
                    s.first_seen.insert(v, seen);
                    // only the versions still to come are worth remembering
                    s.first_seen.retain(|k, _| compare_versions(k, &iv).is_gt());
                });
            }
            match newer.iter_mut().find(|k| k.version == *v) {
                Some(k) => {
                    if k.published_ms.is_none() {
                        k.published_ms = Some(seen);
                    }
                }
                None => newer.insert(
                    0,
                    KernelOffer {
                        release: rel.clone().unwrap_or_else(|| v.clone()),
                        version: v.clone(),
                        available: true,
                        published_ms: Some(seen),
                        ..Default::default()
                    },
                ),
            }
        }
        newer.sort_by(|a, b| compare_versions(&b.version, &a.version));
        let named: BTreeSet<&String> = newer.iter().flat_map(|k| k.cves.iter().flatten()).collect();
        if let Some(f) = feed.as_ref().and_then(|c| c.feed.as_ref()) {
            st.cve_titles = f.cve_titles.iter().filter(|(k, _)| named.contains(k)).map(|(k, v)| (k.clone(), v.clone())).collect();
        }
        st.newer = newer;
        st.last_install_ms = ks.last_install_ms;
        st.last_notified_ms = ks.last_notified.as_ref().map(|(t, _)| *t);
        if policy.mode == KernelMode::Auto
            && let Some(c) = st.candidate()
        {
            st.auto_due_ms = Some(auto_due(&policy, c.published_ms.unwrap_or_else(now_ms), ks.last_install_ms));
        }
        st
    }

    /// Runs `cmd` in the task (or logs it on a dry system).
    async fn krun(&self, ctx: &TaskCtx, cmd: Cmd) -> Result<String, String> {
        self.run(ctx, cmd).await
    }

    /// Holds or lets go the metapackages as the policy says.
    async fn sync_holds(&self, ctx: Option<&TaskCtx>) -> Result<(), String> {
        let p = self.kernel_policy();
        let (hold, unhold) = hold_plan(&self.metas(), &p);
        for (on, list) in [(true, hold), (false, unhold)] {
            if list.is_empty() {
                continue;
            }
            let what = format!("kernel policy {}: {} {}", mode_name(p.mode), if on { "holding" } else { "releasing" }, list.join(", "));
            match ctx {
                Some(ctx) => {
                    ctx.log(&what);
                    self.krun(ctx, apt_mark(on, &list)).await?;
                }
                None => {
                    tracing::info!("{what}");
                    if !self.dry {
                        let o = apt_mark(on, &list).run(None, None).await.map_err(|e| e.to_string())?;
                        if !o.success() {
                            return Err(format!("apt-mark: {}", o.stderr.trim()));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The module's step after a kernel change.
    async fn after_change(&self, ctx: &TaskCtx) -> Result<(), String> {
        let Some(c) = self.channel.clone() else { return Ok(()) };
        ctx.log("pointing the boot device at the kernel of the series");
        let out = tokio::task::spawn_blocking(move || c.after_change()).await.map_err(|e| e.to_string())??;
        for l in out.lines().filter(|l| !l.trim().is_empty()) {
            ctx.log(l);
        }
        Ok(())
    }

    fn kernel_changed(&self) {
        *self.kernel.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Installs apt's candidate of the series in use (`expected`: the version the user saw).
    async fn install_kernel(&self, ctx: &TaskCtx, expected: Option<&str>) -> Result<KernelOffer, String> {
        let st = self.channel_state(true).await;
        if !st.supported {
            return Err("no kernel metapackage is installed".into());
        }
        let cand = st.candidate().cloned().ok_or("no newer kernel to install")?;
        if let Some(v) = expected
            && v != cand.version
        {
            return Err(format!("{} would be installed now, not {v}: check again", cand.version));
        }
        let metas = self.metas();
        let series = series_of(self, &metas);
        let cs = current(&series, &metas).map(|i| series[i].clone()).ok_or("no series in use")?;
        let pkgs: Vec<String> = metas.iter().filter(|m| cs.owns(&m.name)).map(|m| m.name.clone()).collect();
        ctx.log(&format!("installing kernel {} ({})", cand.release, cand.version));
        let held: Vec<String> = metas.iter().filter(|m| m.held && cs.owns(&m.name)).map(|m| m.name.clone()).collect();
        if !held.is_empty() {
            self.krun(ctx, apt_mark(false, &held)).await?;
        }
        let r = self.apt_kernel(ctx, &pkgs, &[]).await;
        // held again whatever happened
        let h = self.sync_holds(Some(ctx)).await;
        r?;
        h?;
        self.after_change(ctx).await?;
        self.kstate_update(|s| s.last_install_ms = Some(now_ms()));
        self.kernel_changed();
        Ok(cand)
    }

    /// `apt-get install` of `install` and removal of `remove`, after a simulation that may
    /// remove nothing else.
    async fn apt_kernel(&self, ctx: &TaskCtx, install: &[String], remove: &[String]) -> Result<(), String> {
        let mut args: Vec<String> = vec!["install".into(), "--".into()];
        args.extend(install.iter().cloned());
        args.extend(remove.iter().map(|r| format!("{r}-")));
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        if !self.dry {
            let sim = self.simulate(&argv, "").await.map(|t| parse_simulation(&t))?;
            let extra: Vec<&String> = sim.removals.iter().filter(|r| !remove.contains(r)).collect();
            if !extra.is_empty() {
                return Err(format!(
                    "the kernel change would also remove {}: nothing done",
                    extra.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                ));
            }
        }
        self.krun(ctx, apt(&argv, Duration::from_secs(3600))).await.map(|_| ())
    }

    /// Switches to the series `id`.
    async fn switch_series(&self, ctx: &TaskCtx, id: &str) -> Result<(), String> {
        let metas = self.metas();
        let series = series_of(self, &metas);
        let to = series.iter().position(|s| s.id == id).ok_or_else(|| format!("unknown series {id}"))?;
        let from = current(&series, &metas).ok_or("no series in use")?;
        if to == from {
            return Err(format!("the machine already follows the {id} series"));
        }
        let headers = metas.iter().any(|m| m.name.starts_with("linux-headers-"));
        let mut install = vec![series[to].image.clone()];
        if headers {
            install.push(series[to].headers.clone());
        }
        // going back: the newer series' metapackages go (the newest series installed boots)
        let remove: Vec<String> = if to < from {
            metas.iter().filter(|m| series[to + 1..].iter().any(|s| s.owns(&m.name))).map(|m| m.name.clone()).collect()
        } else {
            Vec::new()
        };
        ctx.log(&format!("switching from the {} series to the {id} series", series[from].id));
        let held: Vec<String> = metas.iter().filter(|m| m.held).map(|m| m.name.clone()).collect();
        if !held.is_empty() {
            self.krun(ctx, apt_mark(false, &held)).await?;
        }
        let r = self.apt_kernel(ctx, &install, &remove).await;
        let h = self.sync_holds(Some(ctx)).await;
        r?;
        h?;
        self.after_change(ctx).await?;
        self.kstate_update(|s| s.last_install_ms = Some(now_ms()));
        self.kernel_changed();
        Ok(())
    }

    /// Notify mode (and a waiting auto install): the "newer kernel" notification when due.
    pub(crate) async fn kernel_notify(&self) {
        let p = self.kernel_policy();
        if !matches!(p.mode, KernelMode::Notify) {
            return;
        }
        let st = self.channel_state(false).await;
        let Some(c) = st.candidate().cloned() else { return };
        let ks = self.kstate();
        let last = ks.last_notified.as_ref().map(|(t, v)| (*t, v.as_str()));
        if !notify_due(&p, now_ms(), last, &c.version) {
            return;
        }
        let behind = st.newer.len();
        let cves = st.cves().map(|v| v.len());
        let mut ev = oneshot(
            NotifyCategory::Update,
            NotifySeverity::Notice,
            "updates",
            NOTIFY_KEY,
            format!("Kernel {} available", c.release),
        );
        ev.body = match cves {
            Some(n) => format!("Kernels newer than the installed one: {behind}. CVEs fixed: {n}."),
            None => format!("Kernels newer than the installed one: {behind}."),
        };
        ev.actions = vec![EventAction { label: "Open updates".into(), url: "/system/updates".into() }];
        let ev = field(field(code(ev, "updates.kernel_available"), "kernel", &c.release), "behind", behind);
        self.notifier.emit(match cves {
            Some(n) => field(ev, "cves", n),
            None => ev,
        });
        let v = c.version.clone();
        self.kstate_update(|s| s.last_notified = Some((now_ms(), v)));
    }

    /// Schedules the restart at the reboot time of the automatic updates.
    async fn schedule_reboot(&self, ctx: &TaskCtx) -> Result<String, String> {
        let a: AutoUpdates = match self.ops.config.current(AUTO_KEY) {
            Ok(Some(v)) => serde_json::from_value(v).unwrap_or_default(),
            _ => AutoUpdates::default(),
        };
        let at = format!("{:02}:{:02}", a.reboot_hour, a.reboot_minute);
        let cmd = Cmd::new("shutdown")
            .args(["-r", &at, "rynas: restart for the new kernel"])
            .timeout(Duration::from_secs(30));
        self.krun(ctx, cmd).await?;
        Ok(at)
    }
}

async fn fetch_feed(url: &str) -> Result<KernelFeed, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .https_only(true)
        .user_agent(concat!("rynas/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let r = client.get(url).timeout(FEED_TIMEOUT).send().await.map_err(|e| format!("{url}: {e}"))?;
    if !r.status().is_success() {
        return Err(format!("{url}: HTTP {}", r.status().as_u16()));
    }
    if r.content_length().is_some_and(|l| l > FEED_LIMIT as u64) {
        return Err(format!("{url}: too large"));
    }
    let body = r.bytes().await.map_err(|e| format!("{url}: {e}"))?;
    if body.len() > FEED_LIMIT {
        return Err(format!("{url}: too large"));
    }
    parse_feed(&String::from_utf8_lossy(&body))
}

/// At the start of rynasd: the holds the policy wants (the default needs no apply).
pub fn ensure_holds(u: Arc<Updates>) {
    if u.dry || !u.apt_present() {
        return;
    }
    tokio::spawn(async move {
        if let Err(e) = u.sync_holds(None).await {
            tracing::warn!("kernel holds: {e}");
        }
    });
}

/// What keeps the automatic installation from starting: the scheduler's lock table (package
/// work, heavy storage work, an md operation, a shutdown) and the maintenance task kinds the
/// automatic Docker updates also wait for (scrubs, backups, replications, restores…).
fn maintenance(u: &Updates, me: &str) -> Option<String> {
    let mdstat = if u.dry { String::new() } else { std::fs::read_to_string("/proc/mdstat").unwrap_or_default() };
    let gate = crate::gate::running();
    let table = crate::claims::table(gate.as_deref(), &u.ops.tasks, &mdstat, &[]);
    if let Some(why) = crate::claims::busy_except(
        &table,
        rynas_api_types::scheduler::system::KERNEL_UPDATE,
        &["updates.kernel"],
    ) {
        return Some(why);
    }
    u.ops
        .tasks
        .list(200)
        .into_iter()
        .find(|t| {
            t.id != me
                && matches!(t.status, rynas_api_types::TaskStatus::Running | rynas_api_types::TaskStatus::Queued)
                && crate::docker_auto::is_maintenance(&t.kind)
        })
        .map(|t| format!("task {} is running", t.kind))
}

/// The scheduled automatic kernel installation (task `kernel-update`).
pub fn spawn_auto(who: &str) -> Res<String> {
    let u = SVC.get().cloned().ok_or_else(|| Error::internal("updates are not enabled in this rynasd"))?;
    let me = u.clone();
    Ok(u.ops.tasks.spawn("updates.kernel", who, move |ctx| async move {
        let p = me.kernel_policy();
        if p.mode != KernelMode::Auto {
            ctx.log(&format!("kernel policy {}: nothing to do", mode_name(p.mode)));
            return Ok(());
        }
        if let Some(other) = me.running_other(ctx.id()) {
            ctx.log(&format!("another update task runs ({other}): the kernel waits for tomorrow"));
            return Ok(());
        }
        if let Some(why) = maintenance(&me, ctx.id()) {
            ctx.log(&format!("maintenance runs ({why}): the kernel waits for tomorrow"));
            return Ok(());
        }
        let st = me.channel_state(true).await;
        let Some(c) = st.candidate().cloned() else {
            ctx.log("no newer kernel to install");
            return Ok(());
        };
        if let Some(due) = st.auto_due_ms.filter(|d| *d > now_ms()) {
            ctx.log(&format!(
                "kernel {} waits: installed once it is {} days old and {} days after the last one (from {} UTC)",
                c.release,
                p.min_age_days,
                p.interval_days,
                jiff::Timestamp::from_millisecond(due as i64).map(|t| t.strftime("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
            ));
            return Ok(());
        }
        let r = me.install_kernel(&ctx, Some(&c.version)).await;
        me.task_done();
        match r {
            Ok(k) => {
                let mut title = format!("Kernel {} installed", k.release);
                let mut ev_reboot = None;
                if p.reboot {
                    match me.schedule_reboot(&ctx).await {
                        Ok(at) => {
                            title = format!("Kernel {} installed, restart at {at}", k.release);
                            ev_reboot = Some(at);
                        }
                        Err(e) => ctx.log(&format!("restart not scheduled: {e}")),
                    }
                }
                let ev = oneshot(NotifyCategory::Update, NotifySeverity::Notice, "updates", NOTIFY_KEY, title);
                let ev = field(code(ev, "updates.kernel_installed"), "kernel", &k.release);
                me.notifier.emit(match ev_reboot {
                    Some(at) => field(ev, "at", at),
                    None => ev,
                });
                Ok(())
            }
            Err(e) => {
                let mut ev = oneshot(
                    NotifyCategory::Update,
                    NotifySeverity::Warning,
                    "updates",
                    NOTIFY_KEY,
                    format!("Kernel {} not installed", c.release),
                );
                ev.body = e.clone();
                let ev = field(field(code(ev, "updates.kernel_failed"), "kernel", &c.release), "error", &e);
                me.notifier.emit(ev);
                Err(e)
            }
        }
    })?)
}

// ------------------------------------------------------------------ HTTP

fn upd(st: &UpdState) -> Res<Arc<Updates>> {
    st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "updates are not enabled in this rynasd".into(),
        )
    })
}

pub fn router(st: UpdState) -> Router {
    Router::new()
        .route("/v1/updates/kernel/channel", get(channel_get))
        .route("/v1/updates/kernel/policy", get(policy_get).put(policy_put))
        .route("/v1/updates/kernel/install", post(install))
        .route("/v1/updates/kernel/series", post(series_post))
        .with_state(st)
}

async fn channel_get(State(st): State<UpdState>, RawQuery(q): RawQuery) -> Res<Json<KernelChannelState>> {
    let u = upd(&st)?;
    let refresh = q.is_some_and(|q| q.split('&').any(|kv| kv == "refresh=1"));
    Ok(Json(u.channel_state(refresh).await))
}

fn policy_state(u: &Updates, owner: &str) -> Res<KernelPolicyState> {
    let cur: KernelPolicy = match u.ops.config.current(KERNEL_KEY).map_err(Error::internal)? {
        Some(v) => serde_json::from_value(v).map_err(Error::internal)?,
        None => u.kernel_default(),
    };
    let pending = match u.ops.config.pending_value(owner, KERNEL_KEY).map_err(Error::internal)? {
        Some(Some(v)) => Some(serde_json::from_value::<KernelPolicy>(v).map_err(Error::internal)?),
        _ => None,
    };
    Ok(KernelPolicyState { pending: pending.filter(|p| *p != cur), applied: cur })
}

async fn policy_get(State(st): State<UpdState>, hd: HeaderMap) -> Res<Json<KernelPolicyState>> {
    let (u, owner) = (upd(&st)?, user(&hd)?);
    let r = tokio::task::spawn_blocking(move || policy_state(&u, &owner)).await.map_err(Error::internal)??;
    Ok(Json(r))
}

async fn policy_put(
    State(st): State<UpdState>,
    hd: HeaderMap,
    body: Result<Json<KernelPolicy>, JsonRejection>,
) -> Res<Json<KernelPolicyState>> {
    let (u, owner) = (upd(&st)?, user(&hd)?);
    let Json(req) = body?;
    let r = tokio::task::spawn_blocking(move || {
        stage(&u.ops.config, &owner, KERNEL_KEY, &req)?;
        crate::system_tasks::stage_kernel(&u.ops.config, &owner, KERNEL_KEY)?;
        policy_state(&u, &owner)
    })
    .await
    .map_err(Error::internal)??;
    Ok(Json(r))
}

async fn install(
    State(st): State<UpdState>,
    hd: HeaderMap,
    body: Result<Json<KernelInstallRequest>, JsonRejection>,
) -> Res<Response> {
    let (u, who) = (upd(&st)?, user(&hd)?);
    let Json(req) = body?;
    let s = u.channel_state(false).await;
    match s.candidate() {
        None => return Err(Error::invalid("version", "no newer kernel to install")),
        Some(c) if c.version != req.version => {
            return Err(Error::new(
                StatusCode::CONFLICT,
                error_codes::CONFIRMATION_REQUIRED,
                format!("{} would be installed now, not {}", c.version, req.version),
            )
            .field("version"));
        }
        Some(_) => {}
    }
    u.spawn("updates.kernel", &who, move |u, ctx| async move {
        u.install_kernel(&ctx, Some(&req.version)).await.map(|_| ())
    })
}

async fn series_post(
    State(st): State<UpdState>,
    hd: HeaderMap,
    body: Result<Json<KernelSeriesRequest>, JsonRejection>,
) -> Res<Response> {
    let (u, who) = (upd(&st)?, user(&hd)?);
    let Json(req) = body?;
    let s = u.channel_state(false).await;
    match s.series.iter().find(|x| x.id == req.series) {
        None => return Err(Error::invalid("series", "unknown series")),
        Some(x) if x.current => return Err(Error::invalid("series", "the machine already follows this series")),
        Some(_) => {}
    }
    u.spawn("updates.kernel-series", &who, move |u, ctx| async move { u.switch_series(&ctx, &req.series).await })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(name: &str) -> Installed {
        Installed { name: name.into(), version: "1".into(), held: false, release: None }
    }

    #[test]
    fn the_newest_installed_series_boots() {
        let s = vec![
            Series { id: "longterm".into(), image: "a".into(), headers: "ah".into(), followers: vec!["x".into()] },
            Series { id: "stable".into(), image: "b".into(), headers: "bh".into(), followers: vec![] },
        ];
        assert_eq!(current(&s, &[meta("a")]), Some(0));
        assert_eq!(current(&s, &[meta("a"), meta("b")]), Some(1));
        assert_eq!(current(&s, &[meta("x")]), None);
        assert!(s[0].owns("x") && !s[1].owns("x"));
    }

    #[test]
    fn dry_feed_parses() {
        assert!(parse_feed(DRY_FEED).is_ok());
    }
}
