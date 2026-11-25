//! The Debian (official) section: its state read from the entries, the single deb822
//! `debian.sources` rynas writes when it is changed (Debian 13's layout), and the conversion
//! of one-line files to deb822.
//!
//! Plans are lists of [`Change`]s, shown as a preview first and applied by the task: the
//! entries they replace are disabled in place (`# ` / `Enabled: no`) or, when a file held
//! only those, the file is renamed to `.bak` (as `apt modernize-sources` does).

use std::path::Path;

use rynas_api_types::repositories::{
    DEBIAN_COMPONENTS, FileChange, OfficialRequest, OfficialState, RepoFormat, RepoKind, official_suites,
};
use rynas_config::files;

use super::classify::{BACKPORTS_FILE, Class};
use super::sources::{Entry, LIST_DIR, MAIN_LIST, Scan, SourceFile, Span, stanza_text, toggle};

pub const DEBIAN_FILE: &str = "/etc/apt/sources.list.d/debian.sources";
pub const DEFAULT_MIRROR: &str = "http://deb.debian.org/debian";
pub const DEFAULT_SECURITY: &str = "http://security.debian.org/debian-security";
const MARKER: &str = "# Written by rynas (System > Software repositories): change it there.";

/// Country mirrors offered in the list (any other may be typed).
pub const MIRRORS: &[&str] = &[
    "http://deb.debian.org/debian",
    "http://ftp.at.debian.org/debian",
    "http://ftp.au.debian.org/debian",
    "http://ftp.br.debian.org/debian",
    "http://ftp.ca.debian.org/debian",
    "http://ftp.ch.debian.org/debian",
    "http://ftp.cz.debian.org/debian",
    "http://ftp.de.debian.org/debian",
    "http://ftp.dk.debian.org/debian",
    "http://ftp.es.debian.org/debian",
    "http://ftp.fi.debian.org/debian",
    "http://ftp.fr.debian.org/debian",
    "http://ftp.it.debian.org/debian",
    "http://ftp.jp.debian.org/debian",
    "http://ftp.nl.debian.org/debian",
    "http://ftp.pl.debian.org/debian",
    "http://ftp.se.debian.org/debian",
    "http://ftp.uk.debian.org/debian",
    "http://ftp.us.debian.org/debian",
];

/// One change of a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Write(String),
    Rename(String),
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub before: String,
    pub action: Action,
}

impl Change {
    pub fn to_api(&self, existed: bool) -> FileChange {
        let (action, rename_to, after) = match &self.action {
            Action::Write(t) => (if existed { "edit" } else { "write" }, None, t.clone()),
            Action::Rename(to) => ("rename", Some(to.clone()), String::new()),
            Action::Remove => ("remove", None, String::new()),
        };
        FileChange { path: self.path.clone(), action: action.into(), rename_to, before: self.before.clone(), after }
    }
}

/// Debian's archive keyring (`.pgp` on Debian 13, `.gpg` before).
pub fn keyring(root: &Path) -> &'static str {
    const PGP: &str = "/usr/share/keyrings/debian-archive-keyring.pgp";
    if files::under_root(root, Path::new(PGP)).exists() { PGP } else { super::super::backports::KEYRING }
}

/// A mirror URI rynas accepts: http(s), printable, no quotes or spaces.
pub fn valid_mirror(u: &str) -> bool {
    let rest = u.strip_prefix("http://").or_else(|| u.strip_prefix("https://"));
    rest.is_some_and(|r| !r.is_empty() && !r.starts_with('/'))
        && u.len() <= 256
        && u.bytes().all(|b| b.is_ascii_graphic() && !matches!(b, b'"' | b'\\' | b'\'' | b'#'))
}

/// An enabled official entry of this release that a rewrite replaces.
pub fn replaceable(e: &Entry, c: &Class, codename: &str) -> bool {
    let ours = official_suites(codename);
    c.kind == RepoKind::Debian && e.enabled && e.suites.iter().all(|s| ours.contains(s))
}

/// The section's state from the classified entries.
pub fn state(root: &Path, scan: &Scan, classes: &[Vec<Class>], codename: Option<&str>, system_layer: bool) -> OfficialState {
    let mut st = OfficialState {
        manageable: codename.is_some() && !system_layer,
        reason: if codename.is_none() {
            Some("not_debian".into())
        } else if system_layer {
            Some("system_layer".into())
        } else {
            None
        },
        mirror: DEFAULT_MIRROR.into(),
        security_uri: DEFAULT_SECURITY.into(),
        mirrors: MIRRORS.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
    let Some(cn) = codename else { return st };
    let [main, sec, upd, bp, prop] = official_suites(cn);
    let mut comps_set = false;
    for (fi, f) in scan.files.iter().enumerate() {
        for (ei, e) in f.entries.iter().enumerate() {
            let Some(c) = classes.get(fi).and_then(|v| v.get(ei)) else { continue };
            if c.kind != RepoKind::Debian && !(c.kind == RepoKind::Rynas && f.name() == BACKPORTS_FILE) || !e.enabled {
                continue;
            }
            let has = |s: &str| e.suites.iter().any(|x| x == s);
            if e.is_deb() {
                if has(&main) {
                    st.main = true;
                    if let Some(u) = e.uris.iter().find(|u| valid_mirror(u))
                        && !comps_set
                    {
                        st.mirror = u.trim_end_matches('/').to_string();
                    }
                    if !comps_set {
                        st.components = e.components.clone();
                        comps_set = true;
                    }
                }
                if has(&sec) {
                    st.security = true;
                    if let Some(u) = e.uris.iter().find(|u| valid_mirror(u)) {
                        st.security_uri = u.trim_end_matches('/').to_string();
                    }
                }
                st.updates |= has(&upd);
                st.proposed |= has(&prop);
                if has(&bp) {
                    st.backports = true;
                    if f.name() != BACKPORTS_FILE {
                        st.backports_by_system = true;
                    }
                }
            }
            if e.types.iter().any(|t| t == "deb-src") && has(&main) {
                st.sources = true;
            }
        }
    }
    if st.components.is_empty() {
        st.components = vec!["main".into()];
    }
    st.by_rynas = std::fs::read_to_string(files::under_root(root, Path::new(DEBIAN_FILE))).is_ok_and(|t| t.starts_with(MARKER));
    st
}

/// Checks a request of the section.
pub fn validate(req: &OfficialRequest) -> Result<(), (&'static str, String)> {
    if !valid_mirror(req.mirror.trim()) {
        return Err(("mirror", "the mirror must be an http:// or https:// address".into()));
    }
    if req.components.is_empty() || !req.components.iter().any(|c| c == "main") {
        return Err(("components", "main is always needed".into()));
    }
    if let Some(c) = req.components.iter().find(|c| !DEBIAN_COMPONENTS.contains(&c.as_str())) {
        return Err(("components", format!("{c} is not a component of Debian's archive")));
    }
    Ok(())
}

/// The security archive that goes with `mirror`.
fn security_for(mirror: &str, current: &str) -> String {
    if super::classify::host(mirror) == "deb.debian.org" {
        let scheme = if mirror.starts_with("https://") { "https" } else { "http" };
        return format!("{scheme}://deb.debian.org/debian-security");
    }
    if valid_mirror(current) && current.contains("debian-security") { current.to_string() } else { DEFAULT_SECURITY.to_string() }
}

/// The text of `debian.sources` for a request.
pub fn render(codename: &str, req: &OfficialRequest, security_uri: &str, keyring: &str) -> String {
    let types = if req.sources { "deb deb-src" } else { "deb" };
    let comps: Vec<&str> = DEBIAN_COMPONENTS.iter().copied().filter(|c| req.components.iter().any(|x| x == c)).collect();
    let mut suites = vec![codename.to_string()];
    if req.updates {
        suites.push(format!("{codename}-updates"));
    }
    if req.proposed {
        suites.push(format!("{codename}-proposed-updates"));
    }
    let mut out = format!(
        "{MARKER}\nTypes: {types}\nURIs: {}\nSuites: {}\nComponents: {}\nSigned-By: {keyring}\n",
        req.mirror.trim().trim_end_matches('/'),
        suites.join(" "),
        comps.join(" ")
    );
    if req.security {
        out.push_str(&format!(
            "\nTypes: {types}\nURIs: {security_uri}\nSuites: {codename}-security\nComponents: {}\nSigned-By: {keyring}\n",
            comps.join(" ")
        ));
    }
    out
}

fn bak_name(scan: &Scan, root: &Path, path: &str) -> String {
    let base = format!("{path}.bak");
    let taken = |p: &str| files::under_root(root, Path::new(p)).exists() || scan.ignored.iter().any(|i| i.path == p);
    if !taken(&base) {
        return base;
    }
    (1..).map(|n| format!("{path}.bak.{n}")).find(|p| !taken(p)).unwrap_or(base)
}

/// Disables `which` entries of `f`: renamed to `.bak` when nothing else is in it, else each
/// one commented out / `Enabled: no`.
fn disable_entries(scan: &Scan, root: &Path, f: &SourceFile, which: &[usize]) -> Option<Change> {
    if which.is_empty() {
        return None;
    }
    if which.len() == f.entries.len() {
        return Some(Change { path: f.path.clone(), before: f.text.clone(), action: Action::Rename(bak_name(scan, root, &f.path)) });
    }
    let mut text = f.text.clone();
    // from the last: an inserted `Enabled: no` line shifts the stanzas after it
    for &i in which.iter().rev() {
        text = toggle(&text, &f.entries[i], false);
    }
    Some(Change { path: f.path.clone(), before: f.text.clone(), action: Action::Write(text) })
}

/// The plan of a change of the Debian section.
pub fn plan(root: &Path, scan: &Scan, classes: &[Vec<Class>], codename: &str, req: &OfficialRequest, current: &OfficialState) -> Vec<Change> {
    let keyring = keyring(root);
    let security = security_for(req.mirror.trim(), &current.security_uri);
    let mut ours = render(codename, req, &security, keyring);
    let mut out = Vec::new();
    // debian.sources: our stanzas, then the stanzas of the old file that are not official
    let old = scan.file(DEBIAN_FILE);
    if let Some(f) = old {
        let fi = scan.files.iter().position(|x| x.path == f.path).unwrap_or(0);
        for (ei, e) in f.entries.iter().enumerate() {
            let replace = classes.get(fi).and_then(|v| v.get(ei)).is_some_and(|c| c.kind == RepoKind::Debian && e.suites.iter().all(|s| official_suites(codename).contains(s)));
            if let (false, Span::Stanza(s, e2)) = (replace, e.span) {
                ours.push('\n');
                ours.push_str(&stanza_text(&f.text, s, e2));
            }
        }
        if f.error.is_some() {
            // a broken file is replaced as a whole (its text is kept in the backup)
            ours = render(codename, req, &security, keyring);
        }
    }
    if old.map(|f| f.text.as_str()) != Some(ours.as_str()) {
        out.push(Change { path: DEBIAN_FILE.into(), before: old.map(|f| f.text.clone()).unwrap_or_default(), action: Action::Write(ours) });
    }
    // backports: rynas's own file
    let bp_path = format!("{LIST_DIR}/{BACKPORTS_FILE}");
    let bp_old = scan.file(&bp_path);
    if req.backports {
        let src = crate::backports::AptSource {
            uris: vec![req.mirror.trim().trim_end_matches('/').to_string()],
            suites: vec![codename.to_string()],
            components: req.components.clone(),
        };
        let text = crate::backports::render(codename, &[(std::path::PathBuf::new(), src)]);
        if bp_old.map(|f| f.text.as_str()) != Some(text.as_str()) {
            out.push(Change { path: bp_path.clone(), before: bp_old.map(|f| f.text.clone()).unwrap_or_default(), action: Action::Write(text) });
        }
    } else if let Some(f) = bp_old {
        out.push(Change { path: bp_path.clone(), before: f.text.clone(), action: Action::Remove });
    }
    // the official entries elsewhere: disabled
    for (fi, f) in scan.files.iter().enumerate() {
        if f.path == DEBIAN_FILE || f.path == bp_path || f.error.is_some() {
            continue;
        }
        let which: Vec<usize> = f
            .entries
            .iter()
            .enumerate()
            .filter(|(ei, e)| classes.get(fi).and_then(|v| v.get(*ei)).is_some_and(|c| replaceable(e, c, codename)))
            .map(|(ei, _)| ei)
            .collect();
        // renamed only if every other entry is a disabled official one as well
        let all_official = f.entries.iter().enumerate().all(|(ei, e)| {
            which.contains(&ei)
                || classes.get(fi).and_then(|v| v.get(ei)).is_some_and(|c| c.kind == RepoKind::Debian && !e.enabled)
        });
        let which_all: Vec<usize> = if all_official && !which.is_empty() { (0..f.entries.len()).collect() } else { which };
        if let Some(c) = disable_entries(scan, root, f, &which_all) {
            out.push(c);
        }
    }
    out
}

// ------------------------------------------------------------------ deb822 conversion

/// One-line option → deb822 field.
fn field_name(k: &str) -> String {
    let (base, op) = match k.strip_suffix('+') {
        Some(b) => (b, "-Add"),
        None => match k.strip_suffix('-') {
            Some(b) => (b, "-Remove"),
            None => (k, ""),
        },
    };
    let name = match base.to_ascii_lowercase().as_str() {
        "arch" => "Architectures",
        "lang" => "Languages",
        "target" => "Targets",
        "pdiffs" => "PDiffs",
        "by-hash" => "By-Hash",
        "allow-insecure" => "Allow-Insecure",
        "allow-weak" => "Allow-Weak",
        "allow-downgrade-to-insecure" => "Allow-Downgrade-To-Insecure",
        "trusted" => "Trusted",
        "signed-by" => "Signed-By",
        "check-valid-until" => "Check-Valid-Until",
        "valid-until-min" => "Valid-Until-Min",
        "valid-until-max" => "Valid-Until-Max",
        "check-date" => "Check-Date",
        "date-max-future" => "Date-Max-Future",
        "inrelease-path" => "InRelease-Path",
        "snapshot" => "Snapshot",
        other => return format!("{other}{op}"),
    };
    format!("{name}{op}")
}

/// A one-line file as deb822 text: entries with the same state, URI, components and options
/// become one stanza; suites with the same types are kept together (nothing apt reads more
/// or less than before). Comment lines go first.
pub fn convert(f: &SourceFile, classes: &[Class], keyring: &str) -> String {
    type Key = (bool, String, Vec<String>, Vec<(String, String)>);
    /// suite → types
    type Suites = Vec<(String, Vec<String>)>;
    let mut groups: Vec<(Key, Suites, String)> = Vec::new();
    for (i, e) in f.entries.iter().enumerate() {
        let mut opts: Vec<(String, String)> = e.options.iter().map(|(k, v)| (field_name(k), v.replace(',', " "))).collect();
        let debian = classes.get(i).is_some_and(|c| c.kind == RepoKind::Debian);
        if debian && !opts.iter().any(|(k, _)| k == "Signed-By") {
            opts.push(("Signed-By".into(), keyring.into()));
        }
        let key: Key = (e.enabled, e.uris[0].clone(), e.components.clone(), opts);
        let g = match groups.iter_mut().position(|g| g.0 == key) {
            Some(p) => &mut groups[p],
            None => {
                groups.push((key, Vec::new(), e.comment.clone()));
                groups.last_mut().expect("just pushed")
            }
        };
        let suite = e.suites[0].clone();
        match g.1.iter_mut().find(|(s, _)| *s == suite) {
            Some((_, ts)) => {
                if !ts.contains(&e.types[0]) {
                    ts.push(e.types[0].clone());
                }
            }
            None => g.1.push((suite, vec![e.types[0].clone()])),
        }
    }
    let lines: Vec<&str> = f.text.lines().collect();
    let entry_lines: Vec<usize> = f.entries.iter().filter_map(|e| if let Span::Line(l) = e.span { Some(l) } else { None }).collect();
    let mut out = format!("# Converted from {} by rynas; the old file is kept as .bak.\n", f.path);
    let comments: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter(|(i, l)| !entry_lines.contains(i) && l.trim_start().starts_with('#'))
        .map(|(_, l)| *l)
        .collect();
    for c in &comments {
        // entry comments are written above their stanza
        if !groups.iter().any(|g| !g.2.is_empty() && g.2.lines().any(|x| c.trim_start_matches('#').trim() == x)) {
            out.push_str(c);
            out.push('\n');
        }
    }
    for ((enabled, uri, comps, opts), suites, comment) in groups {
        // suites with the same set of types, in their order
        let mut sets: Vec<(Vec<String>, Vec<String>)> = Vec::new();
        for (s, mut ts) in suites {
            ts.sort_by_key(|t| t != "deb");
            match sets.iter_mut().find(|(t, _)| *t == ts) {
                Some((_, ss)) => ss.push(s),
                None => sets.push((ts, vec![s])),
            }
        }
        for (types, ss) in sets {
            out.push('\n');
            for c in comment.lines() {
                out.push_str(&format!("# {c}\n"));
            }
            out.push_str(&format!("Types: {}\nURIs: {uri}\nSuites: {}\n", types.join(" "), ss.join(" ")));
            if !comps.is_empty() {
                out.push_str(&format!("Components: {}\n", comps.join(" ")));
            }
            for (k, v) in &opts {
                out.push_str(&format!("{k}: {v}\n"));
            }
            if !enabled {
                out.push_str("Enabled: no\n");
            }
        }
    }
    out
}

/// The plan of "Convert to the modern format": every one-line file with entries that rynas
/// may change becomes a `.sources` file; the old one is renamed `.bak`.
pub fn modernize(root: &Path, scan: &Scan, classes: &[Vec<Class>]) -> Vec<Change> {
    let keyring = keyring(root);
    let mut out = Vec::new();
    for (fi, f) in scan.files.iter().enumerate() {
        if f.format != RepoFormat::OneLine || f.error.is_some() || f.entries.is_empty() {
            continue;
        }
        let cls = classes.get(fi).cloned().unwrap_or_default();
        if cls.iter().any(|c| c.read_only.is_some()) {
            continue;
        }
        let target = if f.path == MAIN_LIST {
            let all_debian = cls.iter().all(|c| c.kind == RepoKind::Debian);
            let free = |p: &str| scan.file(p).is_none() && !files::under_root(root, Path::new(p)).exists();
            if all_debian && free(DEBIAN_FILE) {
                DEBIAN_FILE.to_string()
            } else {
                let p = format!("{LIST_DIR}/moved-from-main.sources");
                if !free(&p) {
                    continue;
                }
                p
            }
        } else {
            let p = format!("{}.sources", f.path.trim_end_matches(".list"));
            if scan.file(&p).is_some() || files::under_root(root, Path::new(&p)).exists() {
                continue;
            }
            p
        };
        out.push(Change { path: target, before: String::new(), action: Action::Write(convert(f, &cls, keyring)) });
        out.push(Change { path: f.path.clone(), before: f.text.clone(), action: Action::Rename(bak_name(scan, root, &f.path)) });
    }
    out
}
