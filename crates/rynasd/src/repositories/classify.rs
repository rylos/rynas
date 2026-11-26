//! Who each entry belongs to (Debian, rynas, the hardware support's system layer, a third
//! party), where it comes from (the cached Release file), and the checks of the whole set.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use rynas_api_types::repositories::{
    FetchError, FetchErrorKind, ReadOnly, RepoCheck, RepoKind, RepoLevel, RepoStatus, RepoInfo,
};
use rynas_config::files;

use super::sources::{Entry, Scan, SourceFile};

pub const LISTS_DIR: &str = "/var/lib/apt/lists";
/// rynas's backports file (the Debian section manages it).
pub const BACKPORTS_FILE: &str = "rynas-backports.sources";
/// Debian's releases in order: older ones are "old", the one after this is the next.
pub const CODENAMES: &[&str] = &["jessie", "stretch", "buster", "bullseye", "bookworm", "trixie", "forky", "duke"];

/// The files rynas writes for other pages: (file name, page that manages it).
pub const RYNAS_FILES: &[(&str, &str)] = &[
    (BACKPORTS_FILE, "/system/repositories#debian"),
    ("docker.sources", "/docker"),
    ("tailscale.sources", "/network/remote-access"),
];

/// What the cached `InRelease`/`Release` of a suite says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Release {
    pub origin: Option<String>,
    pub label: Option<String>,
    pub suite: Option<String>,
    pub codename: Option<String>,
}

pub fn parse_release(text: &str) -> Release {
    let mut r = Release::default();
    let body = match text.strip_prefix("-----BEGIN PGP SIGNED MESSAGE-----") {
        // clearsigned: headers (`Hash:`) up to the first empty line
        Some(rest) => rest.split_once("\n\n").map_or("", |(_, b)| b),
        None => text,
    };
    for l in body.lines() {
        if l.is_empty() || l.starts_with("-----BEGIN PGP SIGNATURE") {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            let v = Some(v.trim().to_string());
            match k {
                "Origin" => r.origin = v,
                "Label" => r.label = v,
                "Suite" => r.suite = v,
                "Codename" => r.codename = v,
                _ => {}
            }
        }
    }
    r
}

/// apt's `URItoFileName`: no scheme or credentials, some characters `%xx`, `/` → `_`.
pub fn uri_to_file_name(uri: &str) -> String {
    let rest = uri.split_once("://").map_or(uri, |(_, r)| r);
    let rest = match rest.split_once('/') {
        Some((auth, path)) => format!("{}/{path}", auth.rsplit_once('@').map_or(auth, |(_, h)| h)),
        None => rest.rsplit_once('@').map_or(rest, |(_, h)| h).to_string(),
    };
    let mut out = String::new();
    for b in rest.bytes() {
        if b <= 0x20 || b >= 0x7f || b"\\|{}[]<>\"^~_=!@#$%&*".contains(&b) {
            out.push_str(&format!("%{b:02x}"));
        } else if b == b'/' {
            out.push('_');
        } else {
            out.push(b as char);
        }
    }
    out
}

/// The list file prefix of `uri` + `suite` (`…_dists_trixie_`, or the exact path's).
pub fn list_prefix(uri: &str, suite: &str) -> String {
    let base = if uri.ends_with('/') { uri.to_string() } else { format!("{uri}/") };
    let url = if suite.ends_with('/') { format!("{base}{suite}") } else { format!("{base}dists/{suite}/") };
    uri_to_file_name(&url)
}

/// The host of a URI (`deb.debian.org`), lower-case.
pub fn host(uri: &str) -> String {
    let rest = uri.split_once("://").map_or(uri, |(_, r)| r);
    let auth = rest.split('/').next().unwrap_or("");
    let h = auth.rsplit_once('@').map_or(auth, |(_, h)| h);
    let h = h.split(':').next().unwrap_or(h);
    h.to_ascii_lowercase()
}

/// Debian's own hosts (the fallback when no Release file is cached).
pub fn debian_host(h: &str) -> bool {
    matches!(h, "deb.debian.org" | "security.debian.org" | "ftp.debian.org" | "httpredir.debian.org" | "snapshot.debian.org" | "cdn-fastly.deb.debian.org" | "debian.map.fastlydns.net")
        || h.strip_prefix("ftp.").and_then(|r| r.strip_suffix(".debian.org")).is_some_and(|cc| cc.len() == 2 && cc.bytes().all(|b| b.is_ascii_lowercase()))
}

/// A URI on the machine itself: the installation disc or a local folder.
pub fn local_uri(uri: &str) -> bool {
    let u = uri.trim_start().to_ascii_lowercase();
    u.starts_with("cdrom:") || u.starts_with("file:") || u.starts_with("copy:")
}

/// Every URI of `e` is local ([`local_uri`]).
pub fn local_media(e: &Entry) -> bool {
    !e.uris.is_empty() && e.uris.iter().all(|u| local_uri(u))
}

/// Another distribution's archive (incompatible with Debian).
pub fn other_distro(uri: &str, origin: Option<&str>) -> bool {
    let h = host(uri);
    let by_host = ["ubuntu.com", "launchpad.net", "launchpadcontent.net", "linuxmint.com", "pop-os.org"]
        .iter()
        .any(|d| h == *d || h.ends_with(&format!(".{d}")));
    let by_origin = origin.is_some_and(|o| o == "Ubuntu" || o.starts_with("LP-PPA") || o.eq_ignore_ascii_case("linuxmint"));
    by_host || by_origin
}

/// Release files and their age, cached.
pub struct Lists {
    dir: std::path::PathBuf,
    cache: HashMap<String, Option<(Release, Option<u64>)>>,
}

impl Lists {
    pub fn new(root: &Path) -> Self {
        Lists { dir: files::under_root(root, Path::new(LISTS_DIR)), cache: HashMap::new() }
    }

    /// The Release of `uri` `suite` and when it was downloaded (ms).
    pub fn release(&mut self, uri: &str, suite: &str) -> Option<(Release, Option<u64>)> {
        let p = list_prefix(uri, suite);
        let dir = self.dir.clone();
        self.cache
            .entry(p.clone())
            .or_insert_with(|| {
                ["InRelease", "Release"].iter().find_map(|n| {
                    let f = dir.join(format!("{p}{n}"));
                    let text = std::fs::read_to_string(&f).ok()?;
                    let ms = std::fs::metadata(&f).ok().and_then(|m| m.modified().ok()).map(super::ms);
                    Some((parse_release(&text), ms))
                })
            })
            .clone()
    }
}

/// What the classification decided about one entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Class {
    pub kind: RepoKind,
    pub origin: Option<String>,
    pub read_only: Option<ReadOnly>,
    pub manage_url: Option<String>,
    pub last_fetch_ms: Option<u64>,
}

/// The facts the classification needs besides the files.
pub struct Context<'a> {
    pub codename: Option<&'a str>,
    pub system_layer: bool,
    /// sources files shipped by packages: path → package.
    pub packages: &'a BTreeMap<String, String>,
}

/// `/var/lib/dpkg/info/*.conffiles`: the sources files a package ships.
pub fn package_files(root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let dir = files::under_root(root, Path::new("/var/lib/dpkg/info"));
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(pkg) = name.strip_suffix(".conffiles") else { continue };
        let pkg = pkg.split(':').next().unwrap_or(pkg).to_string();
        let Ok(t) = std::fs::read_to_string(e.path()) else { continue };
        for l in t.lines() {
            let p = l.split_whitespace().next().unwrap_or("");
            if p == "/etc/apt/sources.list" || p.starts_with("/etc/apt/sources.list.d/") {
                out.insert(p.to_string(), pkg.clone());
            }
        }
    }
    out
}

/// A file of the hardware support's system layer.
pub fn system_layer_file(f: &SourceFile, cx: &Context) -> bool {
    cx.system_layer && f.path != super::sources::MAIN_LIST && (cx.packages.contains_key(&f.path) || f.name().starts_with("synodeb"))
}

/// The page that manages a rynas file (`None`: not one of rynas's).
pub fn rynas_file(f: &SourceFile) -> Option<&'static str> {
    let name = f.name();
    RYNAS_FILES
        .iter()
        .find(|(n, _)| *n == name && (name == BACKPORTS_FILE || f.text.contains("by rynas")))
        .map(|(_, url)| *url)
}

pub fn classify(f: &SourceFile, e: &Entry, cx: &Context, lists: &mut Lists) -> Class {
    let mut c = Class::default();
    let mut oldest: Option<Option<u64>> = None;
    for u in &e.uris {
        for s in &e.suites {
            let r = lists.release(u, s);
            if c.origin.is_none() {
                c.origin = r.as_ref().and_then(|(r, _)| r.origin.clone());
            }
            let ms = r.and_then(|(_, ms)| ms);
            oldest = Some(match oldest {
                None => ms,
                Some(o) => o.zip(ms).map(|(a, b)| a.min(b)),
            });
        }
    }
    c.last_fetch_ms = oldest.flatten();
    let debian = match &c.origin {
        Some(o) => o.starts_with("Debian"),
        None => e.uris.iter().all(|u| debian_host(&host(u))),
    };
    if c.origin.is_none() && debian {
        c.origin = Some("Debian".into());
    }
    if let Some(url) = rynas_file(f) {
        c.kind = RepoKind::Rynas;
        c.read_only = Some(ReadOnly::Rynas);
        c.manage_url = Some(url.into());
    } else if system_layer_file(f, cx) {
        c.kind = RepoKind::SystemLayer;
        c.read_only = Some(ReadOnly::SystemLayer);
    } else if local_media(e) {
        // before Debian: a disc's cached Release file says `Origin: Debian`
        c.kind = RepoKind::LocalMedia;
    } else if debian {
        c.kind = RepoKind::Debian;
        if cx.system_layer {
            c.read_only = Some(ReadOnly::DebianOnSystemLayer);
        }
    } else {
        c.kind = RepoKind::ThirdParty;
    }
    c
}

/// The fetch errors of `check` that concern `e` (by URI and suite).
pub fn errors_for(e: &Entry, check: Option<&RepoCheck>) -> Vec<FetchError> {
    let Some(check) = check else { return Vec::new() };
    check
        .errors
        .iter()
        .filter(|x| {
            e.uris.iter().any(|u| {
                let base = u.trim_end_matches('/');
                let xu = x.uri.trim_end_matches('/');
                if !(xu == base || xu.starts_with(&format!("{base}/"))) {
                    return false;
                }
                let rest = xu[base.len()..].trim_start_matches('/');
                e.suites.iter().any(|s| match &x.suite {
                    Some(xs) => xs == s || xs.trim_end_matches('/') == s.trim_end_matches('/'),
                    None => {
                        let flat = s.trim_end_matches('/').trim_start_matches("./");
                        rest.is_empty() && x.kind == FetchErrorKind::Duplicate
                            || rest.starts_with(&format!("dists/{s}/"))
                            || rest == format!("dists/{s}")
                            || (s.ends_with('/') && (flat.is_empty() || rest.starts_with(flat)))
                    }
                })
            })
        })
        .cloned()
        .collect()
}

// ------------------------------------------------------------------ checks

/// The findings of a set of classified entries.
pub struct Checked {
    pub infos: Vec<RepoInfo>,
    pub status: RepoStatus,
    pub unsupported: Vec<String>,
}

fn info(level: RepoLevel, code: &str, args: &[(&str, &str)], message: String, at: Option<(&str, usize)>) -> RepoInfo {
    RepoInfo {
        level,
        code: code.into(),
        args: args.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        message,
        path: at.map(|a| a.0.to_string()),
        index: at.map(|a| a.1),
    }
}

/// The base and suffix of a suite (`trixie-security` → `trixie`, `-security`).
pub fn split_suite(s: &str) -> (&str, &str) {
    match s.find(['-', '/']) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    }
}

fn label_of(e: &Entry, c: &Class) -> String {
    c.origin.clone().unwrap_or_else(|| e.uris.first().map(|u| host(u)).unwrap_or_default())
}

/// Keys apt trusts for every source (`/etc/apt/trusted.gpg`, `trusted.gpg.d`), except
/// Debian's own archive keyring fragments.
pub fn legacy_keys(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let main = files::under_root(root, Path::new("/etc/apt/trusted.gpg"));
    if std::fs::metadata(&main).is_ok_and(|m| m.len() > 0) {
        out.push("/etc/apt/trusted.gpg".to_string());
    }
    let dir = files::under_root(root, Path::new("/etc/apt/trusted.gpg.d"));
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    names.sort();
    out.extend(
        names
            .into_iter()
            .filter(|n| !n.starts_with("debian-archive-") && (n.ends_with(".gpg") || n.ends_with(".asc")))
            .map(|n| format!("/etc/apt/trusted.gpg.d/{n}")),
    );
    out
}

/// Every check over the classified entries (`classes[f][e]` for `scan.files[f].entries[e]`).
pub fn check(root: &Path, scan: &Scan, classes: &[Vec<Class>], codename: Option<&str>, last: Option<&RepoCheck>) -> Checked {
    let mut infos = Vec::new();
    let mut unsupported: BTreeSet<String> = BTreeSet::new();
    let cur = codename.and_then(|c| CODENAMES.iter().position(|x| *x == c));
    let (mut has_main, mut has_sec, mut has_upd) = (false, false, false);
    let mut bases: BTreeSet<String> = BTreeSet::new();
    let mut seen: HashMap<(String, String, String, String), (String, usize)> = HashMap::new();
    for f in &scan.files {
        if let Some(err) = &f.error {
            infos.push(info(RepoLevel::Critical, "parse_error", &[("error", err)], format!("{}: {err} (apt refuses to run with it)", f.path), Some((&f.path, 0))));
        }
    }
    for (fi, f) in scan.files.iter().enumerate() {
        for (ei, e) in f.entries.iter().enumerate() {
            let Some(c) = classes.get(fi).and_then(|v| v.get(ei)) else { continue };
            let at = Some((f.path.as_str(), ei));
            // a disabled entry is not checked (not even its last fetch problems)
            if !e.enabled {
                continue;
            }
            for x in errors_for(e, last) {
                let (level, code) = match x.kind {
                    FetchErrorKind::BadSignature => (RepoLevel::Critical, "key_changed"),
                    FetchErrorKind::ExpiredKey => (RepoLevel::Critical, "key_expired"),
                    FetchErrorKind::NoPubkey => (RepoLevel::Critical, "no_pubkey"),
                    FetchErrorKind::NotSigned => (RepoLevel::Critical, "not_signed"),
                    FetchErrorKind::Duplicate => continue,
                    _ if x.error => (RepoLevel::Warning, "fetch_failed"),
                    _ => (RepoLevel::Warning, "fetch_warning"),
                };
                infos.push(info(level, code, &[("uri", &x.uri), ("message", &x.message)], x.message.clone(), at));
            }
            // insecure options
            let insecure = e.options.iter().any(|(k, v)| {
                let k = k.to_ascii_lowercase();
                ["trusted", "allow-insecure", "allow-weak", "allow-downgrade-to-insecure"].contains(&k.as_str()) && truthy_opt(v)
            });
            if insecure {
                infos.push(info(RepoLevel::Critical, "insecure", &[("uri", &e.uris.join(" "))], format!("{}: signature checks are turned off (Trusted/Allow-Insecure)", e.uris.join(" ")), at));
                unsupported.insert(label_of(e, c));
            }
            if e.uris.iter().any(|u| other_distro(u, c.origin.as_deref())) {
                infos.push(info(RepoLevel::Critical, "other_distro", &[("uri", &e.uris.join(" "))], format!("{}: packages of another distribution, incompatible with Debian", e.uris.join(" ")), at));
                unsupported.insert(label_of(e, c));
            }
            if c.kind == RepoKind::LocalMedia && e.uris.iter().any(|u| u.trim_start().to_ascii_lowercase().starts_with("cdrom:")) {
                infos.push(info(
                    RepoLevel::Warning,
                    "cdrom_enabled",
                    &[],
                    "the installation disc is enabled: apt asks for the disc at every update; disable this entry".into(),
                    at,
                ));
            }
            if c.kind == RepoKind::ThirdParty {
                unsupported.insert(label_of(e, c));
                match e.signed_by() {
                    None => infos.push(info(RepoLevel::Warning, "no_signed_by", &[("uri", &e.uris.join(" "))], format!("{}: no Signed-By, so every trusted key may sign it", e.uris.join(" ")), at)),
                    Some(k) if k.starts_with('/') && !files::under_root(root, Path::new(k)).exists() => {
                        infos.push(info(RepoLevel::Warning, "key_missing", &[("key", k)], format!("the key file {k} does not exist"), at))
                    }
                    _ => {}
                }
            }
            // duplicates (apt warns)
            for t in &e.types {
                for u in &e.uris {
                    for s in &e.suites {
                        for comp in if e.components.is_empty() { vec![String::new()] } else { e.components.clone() } {
                            let k = (t.clone(), u.trim_end_matches('/').to_string(), s.clone(), comp);
                            if let Some((p, i)) = seen.get(&k)
                                && (p.as_str(), *i) != (f.path.as_str(), ei)
                            {
                                infos.push(info(RepoLevel::Warning, "duplicate", &[("other", p.as_str())], format!("{u} {s} is also configured in {p}"), at));
                                break;
                            }
                            seen.insert(k, (f.path.clone(), ei));
                        }
                    }
                }
            }
            if c.kind != RepoKind::Debian || !e.is_deb() {
                continue;
            }
            for s in &e.suites {
                let (base, suffix) = split_suite(s);
                if Some(s.as_str()) == codename {
                    has_main = true;
                }
                if codename.is_some_and(|c| s == &format!("{c}-security")) {
                    has_sec = true;
                }
                if codename.is_some_and(|c| s == &format!("{c}-updates")) {
                    has_upd = true;
                }
                if suffix == "-proposed-updates" {
                    infos.push(info(RepoLevel::Info, "proposed", &[("suite", s)], format!("{s}: packages waiting for the next point release, not yet tested"), at));
                }
                match base {
                    "testing" | "unstable" | "sid" | "experimental" | "rc-buggy" => {
                        infos.push(info(RepoLevel::Critical, "testing", &[("suite", s)], format!("{s} is not for production and mixes Debian releases"), at));
                        unsupported.insert(format!("Debian {s}"));
                    }
                    "stable" => infos.push(info(RepoLevel::Warning, "use_codename", &[("suite", s), ("codename", codename.unwrap_or("?"))], format!("{s}: use the release name instead, or the next release arrives unannounced"), at)),
                    "oldstable" | "oldoldstable" => infos.push(info(RepoLevel::Warning, "old_suite", &[("suite", s)], format!("{s} is an older Debian release"), at)),
                    _ => {
                        if let (Some(cur), Some(i)) = (cur, CODENAMES.iter().position(|x| *x == base)) {
                            if i < cur {
                                infos.push(info(RepoLevel::Warning, "old_suite", &[("suite", s)], format!("{s} is an older Debian release"), at));
                                bases.insert(base.to_string());
                            } else if i == cur + 1 {
                                infos.push(info(RepoLevel::Info, "next_release", &[("suite", s)], format!("{s} is the next Debian release: only while upgrading to it"), at));
                            } else if i > cur + 1 {
                                infos.push(info(RepoLevel::Critical, "testing", &[("suite", s)], format!("{s} is not for production and mixes Debian releases"), at));
                                unsupported.insert(format!("Debian {s}"));
                            } else {
                                bases.insert(base.to_string());
                            }
                        }
                    }
                }
                if suffix == "/updates" {
                    infos.push(info(RepoLevel::Warning, "security_suite", &[("suite", s), ("codename", base)], format!("{s}: the security suite is called {base}-security since Debian 11"), at));
                }
                if suffix == "-security" && !e.uris.iter().all(|u| u.contains("debian-security")) {
                    infos.push(info(RepoLevel::Warning, "security_uri", &[("suite", s)], format!("{s} is served from …/debian-security, not from the main archive"), at));
                }
            }
            let bookworm_or_later = cur.is_some_and(|c| c >= 4);
            if bookworm_or_later && e.components.iter().any(|c| c == "non-free") && !e.components.iter().any(|c| c == "non-free-firmware") {
                infos.push(info(RepoLevel::Info, "non_free_firmware", &[], "non-free is used without non-free-firmware: firmware moved there in Debian 12".into(), at));
            }
        }
    }
    if let Some(c) = codename {
        if !has_main {
            infos.push(info(RepoLevel::Critical, "no_debian", &[("codename", c)], format!("no enabled Debian source for {c}: the system gets no updates"), None));
        } else {
            if !has_sec {
                infos.push(info(RepoLevel::Warning, "no_security", &[("codename", c)], format!("{c}-security is not configured: no security updates"), None));
            }
            if !has_upd {
                infos.push(info(RepoLevel::Info, "no_updates", &[("codename", c)], format!("{c}-updates is not configured: urgent fixes arrive only with point releases"), None));
            }
        }
    }
    if bases.len() > 1 {
        let list = bases.iter().cloned().collect::<Vec<_>>().join(", ");
        infos.push(info(RepoLevel::Warning, "mixed_suites", &[("suites", &list)], format!("Debian sources of several releases are enabled: {list}"), None));
    }
    for k in legacy_keys(root) {
        infos.push(info(RepoLevel::Warning, "legacy_keyring", &[("key", &k)], format!("{k}: keys there are trusted for every source (deprecated)"), None));
    }
    let status = match infos.iter().map(|i| i.level).max() {
        Some(RepoLevel::Critical) => RepoStatus::Critical,
        Some(RepoLevel::Warning) => RepoStatus::Warning,
        _ => RepoStatus::Good,
    };
    Checked { infos, status, unsupported: unsupported.into_iter().filter(|s| !s.is_empty()).collect() }
}

fn truthy_opt(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "yes" | "true" | "1" | "on")
}
