//! When new kernels are installed (`updates.kernel`): the pure half.
//!
//! - The kernel **metapackages** are the installed `linux-image-*` / `linux-headers-*`
//!   packages whose suffix does not start with a digit (`linux-image-amd64`,
//!   `linux-image-synodeb-stable`; the versioned kernels are `linux-image-6.12.57+deb13-amd64`).
//!   Holding them keeps apt, unattended-upgrades and "Install all" from installing a newer
//!   kernel: each depends on the exact version of the next. Packages left with their
//!   configuration only (`rc`) are not metapackages here.
//! - The **series**: a hardware module's kernel channel names them (oldest branch first, the
//!   last installed one boots); without one the installed image metapackage is the only
//!   series (`default`).
//! - [`KernelDomain`] (key `updates.kernel`, no files): its reconcile holds the metapackages
//!   (`apt-mark hold`) in every mode but `system`, and lets them go in `system`. The key's
//!   value before the user chose one is [`KernelPolicy::default_for`] (its import): rynasd
//!   puts the holds in place at start too ([`hold_plan`]).
//! - Versions compare as Debian's (`dpkg --compare-versions`, [`compare_versions`]); feed
//!   times are RFC 3339 UTC ([`utc_ms`]).

use std::cmp::Ordering;
use std::path::Path;
use std::time::Duration;

use rynas_api_types::updates::{KernelFeed, KernelMode, KernelPolicy};
use rynas_config::{Applier, ApplyCtx, Invalid, ManagedFile};
use rynas_tasks::Cmd;
use serde_json::Value;

pub const KERNEL_KEY: &str = "updates.kernel";
pub const APT_MARK: &str = "/usr/bin/apt-mark";
pub const DPKG_STATUS: &str = "/var/lib/dpkg/status";
const DAY_MS: u64 = 86_400_000;

/// An installed package as dpkg's status file has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub name: String,
    pub version: String,
    /// Selection `hold`.
    pub held: bool,
    /// The kernel release it depends on (`Depends: linux-image-<release>`), when direct.
    pub release: Option<String>,
}

/// A kernel metapackage name.
pub fn is_meta(name: &str) -> bool {
    let rest = name
        .strip_prefix("linux-image-")
        .or_else(|| name.strip_prefix("linux-headers-"));
    match rest {
        Some(r) => {
            !r.is_empty()
                && !r.starts_with(|c: char| c.is_ascii_digit())
                && !r.ends_with("-dbg")
                && !r.ends_with("-dbgsym")
        }
        None => false,
    }
}

/// The installed kernel metapackages, by name.
pub fn installed_metas(status: &str) -> Vec<Installed> {
    let mut out = Vec::new();
    for stanza in status.split("\n\n") {
        let (mut name, mut version, mut st) = (None, None, None);
        for l in stanza.lines() {
            if let Some(v) = l.strip_prefix("Package: ") {
                name = Some(v.trim());
            } else if let Some(v) = l.strip_prefix("Version: ") {
                version = Some(v.trim());
            } else if let Some(v) = l.strip_prefix("Status: ") {
                st = Some(v.trim());
            }
        }
        let (Some(name), Some(version), Some(st)) = (name, version, st) else { continue };
        let mut f = st.split_whitespace();
        let (want, _, state) = (f.next().unwrap_or(""), f.next(), f.next().unwrap_or(""));
        if is_meta(name) && state == "installed" {
            out.push(Installed {
                name: name.into(),
                version: version.into(),
                held: want == "hold",
                release: depends_release(stanza),
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// `(to hold, to let go)` for `policy`: every installed metapackage held, or none.
pub fn hold_plan(metas: &[Installed], policy: &KernelPolicy) -> (Vec<String>, Vec<String>) {
    let hold = policy.holds();
    let names = |want: bool| metas.iter().filter(|m| m.held != want).map(|m| m.name.clone()).collect();
    if hold { (names(true), Vec::new()) } else { (Vec::new(), names(false)) }
}

/// `apt-mark hold|unhold -- <packages>` outside rynasd's sandbox (dpkg's database and lock).
pub fn apt_mark(hold: bool, packages: &[String]) -> Cmd {
    rynas_tasks::Outside::new("rynas-apt-mark", "rynas: apt-mark (System > Updates)", Duration::from_secs(120))
        .setenv(&["LC_ALL=C"])
        .cmd(
            [APT_MARK, if hold { "hold" } else { "unhold" }, "--"]
                .into_iter()
                .chain(packages.iter().map(String::as_str)),
        )
}

// ------------------------------------------------------------------ Debian versions

fn order(c: Option<char>) -> i32 {
    match c {
        None => 0,
        Some('~') => -1,
        Some(c) if c.is_ascii_alphabetic() => c as i32,
        Some(c) => c as i32 + 256,
    }
}

fn cmp_part(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a, b);
    loop {
        if a.is_empty() && b.is_empty() {
            return Ordering::Equal;
        }
        // non-digit prefix
        let na = a.find(|c: char| c.is_ascii_digit()).unwrap_or(a.len());
        let nb = b.find(|c: char| c.is_ascii_digit()).unwrap_or(b.len());
        let (mut pa, mut pb) = (a[..na].chars(), b[..nb].chars());
        loop {
            let (x, y) = (pa.next(), pb.next());
            if x.is_none() && y.is_none() {
                break;
            }
            match order(x).cmp(&order(y)) {
                Ordering::Equal => {}
                o => return o,
            }
        }
        a = &a[na..];
        b = &b[nb..];
        let da = a.find(|c: char| !c.is_ascii_digit()).unwrap_or(a.len());
        let db = b.find(|c: char| !c.is_ascii_digit()).unwrap_or(b.len());
        let va = a[..da].trim_start_matches('0');
        let vb = b[..db].trim_start_matches('0');
        match va.len().cmp(&vb.len()).then_with(|| va.cmp(vb)) {
            Ordering::Equal => {}
            o => return o,
        }
        a = &a[da..];
        b = &b[db..];
    }
}

/// Debian version order (`epoch:upstream-revision`).
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let split = |v: &str| -> (u64, String, String) {
        let (epoch, rest) = match v.split_once(':') {
            Some((e, r)) if e.chars().all(|c| c.is_ascii_digit()) => (e.parse().unwrap_or(0), r),
            _ => (0, v),
        };
        let (up, rev) = match rest.rfind('-') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        (epoch, up.to_string(), rev.to_string())
    };
    let (ea, ua, ra) = split(a);
    let (eb, ub, rb) = split(b);
    ea.cmp(&eb).then_with(|| cmp_part(&ua, &ub)).then_with(|| cmp_part(&ra, &rb))
}

// ------------------------------------------------------------------ apt-cache

/// `Candidate:` of `LC_ALL=C apt-cache policy <pkg>` (`None` for `(none)`).
pub fn policy_candidate(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.trim().strip_prefix("Candidate: "))
        .map(str::trim)
        .filter(|v| *v != "(none)" && !v.is_empty())
        .map(str::to_string)
}

/// The kernel release a metapackage stanza (`apt-cache show pkg=version`) depends on:
/// the first `linux-image-<digit>…` / `linux-headers-<digit>…` of its `Depends:`.
pub fn depends_release(stanza: &str) -> Option<String> {
    let deps = stanza.lines().find_map(|l| l.strip_prefix("Depends: "))?;
    deps.split(',').find_map(|d| {
        let name = d.trim().split([' ', '|']).next()?;
        let rest = name
            .strip_prefix("linux-image-")
            .or_else(|| name.strip_prefix("linux-headers-"))?;
        rest.starts_with(|c: char| c.is_ascii_digit()).then(|| rest.to_string())
    })
}

/// The metapackage a stanza depends on with an exact version (`linux-image-synodeb` →
/// `linux-image-synodeb-longterm`), to follow an alias down to its series.
pub fn depends_meta(stanza: &str) -> Option<String> {
    let deps = stanza.lines().find_map(|l| l.strip_prefix("Depends: "))?;
    deps.split(',').find_map(|d| {
        let name = d.trim().split([' ', '|']).next()?;
        is_meta(name).then(|| name.to_string())
    })
}

// ------------------------------------------------------------------ feed

/// A kernel feed; formats other than 1 are refused.
pub fn parse_feed(text: &str) -> Result<KernelFeed, String> {
    let f: KernelFeed = serde_json::from_str(text).map_err(|e| format!("kernel feed: {e}"))?;
    if f.format != 1 {
        return Err(format!("kernel feed: format {} is not known", f.format));
    }
    Ok(f)
}

/// `YYYY-MM-DDTHH:MM:SS[.frac](Z|+00:00)` → ms since the epoch.
pub fn utc_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    let s = s.strip_suffix('Z').or_else(|| s.strip_suffix("+00:00"))?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>().ok());
    let (y, m, dd) = (d.next()??, d.next()??, d.next()??);
    let time = time.split('.').next()?;
    let mut t = time.split(':').map(|x| x.parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next()??);
    if !(1..=12).contains(&m) || !(1..=31).contains(&dd) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // days from civil (Howard Hinnant)
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + dd - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hh * 3600 + mm * 60 + ss;
    u64::try_from(secs).ok().map(|s| s * 1000)
}

// ------------------------------------------------------------------ timing

/// Auto: the earliest time a kernel published at `published_ms` may be installed.
pub fn auto_due(p: &KernelPolicy, published_ms: u64, last_install_ms: Option<u64>) -> u64 {
    let age = published_ms.saturating_add(u64::from(p.min_age_days) * DAY_MS);
    let gap = last_install_ms.map_or(0, |l| l.saturating_add(u64::from(p.interval_days) * DAY_MS));
    age.max(gap)
}

/// Notify (and auto, for a kernel it cannot install yet): a notification for `version` is
/// due when that version was not notified and the last notification is old enough.
pub fn notify_due(p: &KernelPolicy, now_ms: u64, last: Option<(u64, &str)>, version: &str) -> bool {
    match last {
        None => true,
        Some((_, v)) if v == version => false,
        Some((t, _)) => now_ms >= t.saturating_add(u64::from(p.interval_days) * DAY_MS),
    }
}

// ------------------------------------------------------------------ domain

pub fn policy_of(v: &Value) -> Result<KernelPolicy, String> {
    serde_json::from_value(v.clone()).map_err(|e| format!("invalid updates.kernel document: {e}"))
}

pub fn check(p: &KernelPolicy) -> Result<(), Invalid> {
    if !(1..=365).contains(&p.interval_days) {
        return Err(Invalid::at("interval_days", "the interval must be 1-365 days"));
    }
    if p.min_age_days > 365 {
        return Err(Invalid::at("min_age_days", "the age must be 0-365 days"));
    }
    Ok(())
}

/// The installed metapackages below `root`.
pub fn read_metas(root: &Path) -> Vec<Installed> {
    std::fs::read_to_string(root.join(DPKG_STATUS.trim_start_matches('/')))
        .map(|t| installed_metas(&t))
        .unwrap_or_default()
}

/// The branch (`7.3`) and flavour (`synodeb`, `rt-amd64`) of a kernel release
/// (`7.3.5-13-synodeb`, `6.12.48+deb13-rt-amd64`).
fn kernel_line(release: &str) -> Option<(&str, &str)> {
    let mut dots = release.match_indices('.');
    dots.next()?;
    let branch = match dots.next() {
        Some((i, _)) => &release[..i],
        None => release.split(['-', '+']).next()?,
    };
    let mut at = 0;
    let flavour = release.split('-').enumerate().find_map(|(i, s)| {
        let start = at;
        at += s.len() + 1;
        (i > 0 && !s.starts_with(|c: char| c.is_ascii_digit())).then(|| &release[start..])
    })?;
    Some((branch, flavour))
}

/// The newest kernel installed for a metapackage, as (package version, release): its own kernel,
/// or a newer image of the same branch and flavour installed by name (`apt install linux-image-<release>`).
pub fn newest_kernel(status: &str, meta: &Installed) -> (String, Option<String>) {
    let mut best = (meta.version.clone(), meta.release.clone());
    let Some(line) = meta.release.as_deref().and_then(kernel_line) else {
        return best;
    };
    for stanza in status.split("\n\n") {
        let (mut name, mut version, mut st) = (None, None, None);
        for l in stanza.lines() {
            if let Some(v) = l.strip_prefix("Package: ") {
                name = Some(v.trim());
            } else if let Some(v) = l.strip_prefix("Version: ") {
                version = Some(v.trim());
            } else if let Some(v) = l.strip_prefix("Status: ") {
                st = Some(v.trim());
            }
        }
        let (Some(name), Some(version), Some(st)) = (name, version, st) else { continue };
        let Some(rel) = name.strip_prefix("linux-image-").filter(|r| r.starts_with(|c: char| c.is_ascii_digit())) else {
            continue;
        };
        if st.split_whitespace().nth(2) == Some("installed")
            && kernel_line(rel) == Some(line)
            && compare_versions(version, &best.0).is_gt()
        {
            best = (version.to_string(), Some(rel.to_string()));
        }
    }
    best
}

/// [`newest_kernel`] from the dpkg status below `root`.
pub fn read_newest_kernel(root: &Path, meta: &Installed) -> (String, Option<String>) {
    std::fs::read_to_string(root.join(DPKG_STATUS.trim_start_matches('/')))
        .map(|t| newest_kernel(&t, meta))
        .unwrap_or_else(|_| (meta.version.clone(), meta.release.clone()))
}

/// The `updates.kernel` domain. `module_kernels`: a hardware module ships the kernels (the
/// default is then manual).
pub struct KernelDomain {
    pub module_kernels: bool,
}

impl Applier for KernelDomain {
    fn key(&self) -> &str {
        KERNEL_KEY
    }

    fn order(&self) -> i32 {
        71
    }

    fn validate(&self, value: Value) -> Result<Value, Invalid> {
        let p = policy_of(&value)?;
        check(&p)?;
        serde_json::to_value(p).map_err(|e| e.to_string().into())
    }

    fn render(&self, _value: &Value, _ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        Ok(Vec::new())
    }

    fn reconcile(&self, _from: Option<&Value>, to: Option<&Value>, ctx: &ApplyCtx) -> Result<(), String> {
        let p = match to {
            Some(v) => policy_of(v)?,
            None => KernelPolicy::default_for(self.module_kernels),
        };
        let (hold, unhold) = hold_plan(&read_metas(ctx.root), &p);
        for (on, list) in [(true, hold), (false, unhold)] {
            if !list.is_empty() {
                (ctx.log)(&format!("{} {}", if on { "holding" } else { "releasing" }, list.join(", ")));
                ctx.system.run(&apt_mark(on, &list))?;
            }
        }
        Ok(())
    }

    fn import(&self, _ctx: &ApplyCtx) -> Result<Option<Value>, String> {
        Ok(Some(
            serde_json::to_value(KernelPolicy::default_for(self.module_kernels)).expect("serializable"),
        ))
    }
}

/// The mode's name for logs.
pub fn mode_name(m: KernelMode) -> &'static str {
    match m {
        KernelMode::System => "system",
        KernelMode::Manual => "manual",
        KernelMode::Notify => "notify",
        KernelMode::Auto => "auto",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rynas_config::{Config, ConfigOptions, MemorySystem};

    use super::*;

    const STATUS: &str = "Package: linux-image-synodeb-stable\nStatus: install ok installed\nVersion: 7.3.5-13\nDepends: linux-image-7.3.5-13-synodeb (= 7.3.5-13)\n\n\
Package: linux-image-synodeb-longterm\nStatus: deinstall ok config-files\nVersion: 6.18.60-9\n\n\
Package: linux-image-7.3.5-13-synodeb\nStatus: install ok installed\nVersion: 7.3.5-13\n\n\
Package: linux-headers-synodeb-stable\nStatus: hold ok installed\nVersion: 7.3.5-13\n\n\
Package: bash\nStatus: install ok installed\nVersion: 5.2\n";

    #[test]
    fn metapackages() {
        assert!(is_meta("linux-image-amd64"));
        assert!(is_meta("linux-headers-synodeb"));
        assert!(!is_meta("linux-image-6.12.57+deb13-amd64"));
        assert!(!is_meta("linux-image-amd64-dbg"));
        assert!(!is_meta("linux-libc-dev"));
        let m = installed_metas(STATUS);
        assert_eq!(
            m,
            vec![
                Installed { name: "linux-headers-synodeb-stable".into(), version: "7.3.5-13".into(), held: true, release: None },
                Installed {
                    name: "linux-image-synodeb-stable".into(),
                    version: "7.3.5-13".into(),
                    held: false,
                    release: Some("7.3.5-13-synodeb".into()),
                },
            ],
            "rc packages are left out"
        );
        let manual = KernelPolicy { mode: KernelMode::Manual, ..Default::default() };
        assert_eq!(hold_plan(&m, &manual), (vec!["linux-image-synodeb-stable".into()], vec![]));
        assert_eq!(hold_plan(&m, &KernelPolicy::default()), (vec![], vec!["linux-headers-synodeb-stable".into()]));
    }

    #[test]
    fn kernels_installed_by_name() {
        assert_eq!(kernel_line("7.3.5-13-synodeb"), Some(("7.3", "synodeb")));
        assert_eq!(kernel_line("6.12.48+deb13-rt-amd64"), Some(("6.12", "rt-amd64")));
        assert_eq!(kernel_line("6.12.48+deb13-amd64"), Some(("6.12", "amd64")));
        let meta = installed_metas(STATUS).into_iter().find(|m| m.name == "linux-image-synodeb-stable").unwrap();
        assert_eq!(newest_kernel(STATUS, &meta), ("7.3.5-13".into(), Some("7.3.5-13-synodeb".into())));
        let more = format!(
            "{STATUS}\nPackage: linux-image-7.3.6-14-synodeb\nStatus: install ok installed\nVersion: 7.3.6-14\n\n\
Package: linux-image-7.3.7-15-synodeb\nStatus: deinstall ok config-files\nVersion: 7.3.7-15\n\n\
Package: linux-image-6.18.65-13-synodeb\nStatus: install ok installed\nVersion: 6.18.65-13\n\n\
Package: linux-image-7.4.1-16-synodeb\nStatus: install ok installed\nVersion: 7.4.1-16\n"
        );
        assert_eq!(
            newest_kernel(&more, &meta),
            ("7.3.6-14".into(), Some("7.3.6-14-synodeb".into())),
            "same branch and flavour only, installed only"
        );
    }

    #[test]
    fn versions() {
        use Ordering::*;
        for (a, b, o) in [
            ("6.18.64-12", "6.18.64-12", Equal),
            ("6.18.64-12", "6.18.63-11", Greater),
            ("7.3.5-13", "7.3.10-14", Less),
            ("1:6.18.64-12", "7.3.5-13", Greater),
            ("6.12.57-1", "6.12.57-1~bpo", Greater),
            ("6.12.57+deb13-1", "6.12.57-1", Greater),
            ("1.0", "1.0-0", Equal),
        ] {
            assert_eq!(compare_versions(a, b), o, "{a} vs {b}");
        }
    }

    #[test]
    fn apt_cache() {
        let pol = "linux-image-synodeb-longterm:\n  Installed: (none)\n  Candidate: 6.18.64-12\n  Version table:\n";
        assert_eq!(policy_candidate(pol).as_deref(), Some("6.18.64-12"));
        assert_eq!(policy_candidate("x:\n  Installed: (none)\n  Candidate: (none)\n"), None);
        let st = "Package: linux-image-synodeb-longterm\nVersion: 6.18.64-12\nDepends: linux-image-6.18.64-12-synodeb (= 6.18.64-12)\n";
        assert_eq!(depends_release(st).as_deref(), Some("6.18.64-12-synodeb"));
        let deb = "Package: linux-image-amd64\nDepends: linux-image-6.12.57+deb13-amd64 (= 6.12.57-1)\n";
        assert_eq!(depends_release(deb).as_deref(), Some("6.12.57+deb13-amd64"));
        let alias = "Package: linux-image-synodeb\nDepends: linux-image-synodeb-longterm (= 6.18.64-12)\n";
        assert_eq!(depends_release(alias), None);
        assert_eq!(depends_meta(alias).as_deref(), Some("linux-image-synodeb-longterm"));
    }

    #[test]
    fn feed_and_times() {
        let f = parse_feed(include_str!("fixtures/kernels.json")).unwrap();
        assert_eq!(f.kernels[0].series, "stable");
        assert!(f.kernels.iter().any(|k| !k.cves.as_ref().is_none_or(|c| c.is_empty())));
        assert!(parse_feed(r#"{"format":2}"#).is_err());
        assert_eq!(utc_ms("1970-01-02T00:00:00Z"), Some(86_400_000));
        assert_eq!(utc_ms("2026-11-23T00:22:42Z"), Some(1_795_393_362_000));
        assert_eq!(utc_ms("2026-13-01T00:00:00Z"), None);
        let p = KernelPolicy { min_age_days: 14, interval_days: 30, ..Default::default() };
        assert_eq!(auto_due(&p, 0, None), 14 * DAY_MS);
        assert_eq!(auto_due(&p, 0, Some(DAY_MS)), 31 * DAY_MS);
        assert!(notify_due(&p, 0, None, "1"));
        assert!(!notify_due(&p, 99 * DAY_MS, Some((0, "1")), "1"));
        assert!(!notify_due(&p, 29 * DAY_MS, Some((0, "1")), "2"));
        assert!(notify_due(&p, 30 * DAY_MS, Some((0, "1")), "2"));
    }

    #[test]
    fn domain_holds() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("root");
        std::fs::create_dir_all(root.join("var/lib/dpkg")).unwrap();
        std::fs::write(root.join("var/lib/dpkg/status"), STATUS).unwrap();
        let sys = Arc::new(MemorySystem::new("nas"));
        let cfg = Config::open(
            ConfigOptions::new(d.path().join("state"), &root),
            sys.clone(),
            vec![Arc::new(KernelDomain { module_kernels: true })],
        )
        .unwrap();
        let cur = policy_of(&cfg.current(KERNEL_KEY).unwrap().unwrap()).unwrap();
        assert_eq!(cur.mode, KernelMode::Manual, "a module's kernels are manual by default");
        let p = KernelPolicy { mode: KernelMode::Auto, ..Default::default() };
        cfg.stage("admin", KERNEL_KEY, Some(serde_json::to_value(p).unwrap())).unwrap();
        cfg.apply("admin", "admin", "", &|_| {}, &|| false).unwrap();
        let calls = sys.state().calls.clone();
        assert!(calls.iter().any(|c| c.contains("apt-mark hold -- linux-image-synodeb-stable")), "{calls:?}");
        let bad = KernelPolicy { interval_days: 0, ..Default::default() };
        assert!(cfg.stage("admin", KERNEL_KEY, Some(serde_json::to_value(bad).unwrap())).is_err());
    }
}
