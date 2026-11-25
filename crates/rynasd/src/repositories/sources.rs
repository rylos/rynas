//! Reading and editing apt sources files without losing a byte of what rynas does not change.
//!
//! - One-line files (`/etc/apt/sources.list`, `*.list`): one entry per `deb`/`deb-src` line;
//!   a commented-out entry (`# deb …`) is a disabled entry. Other comment lines stay as they are.
//! - deb822 files (`*.sources`): one entry per stanza, multi-value fields kept together;
//!   `Enabled: no` disables it.
//! - Edits touch only the line or stanza of the entry (comment/uncomment, the `Enabled:` line).
//! - A file that does not parse is reported with its error and never blocks the others.

use std::path::{Path, PathBuf};

use rynas_api_types::repositories::{IgnoredFile, RepoFormat, apt_file_name_ok};
use rynas_config::files;
use sha2::Digest;

pub const MAIN_LIST: &str = "/etc/apt/sources.list";
pub const LIST_DIR: &str = "/etc/apt/sources.list.d";
/// Larger sources files are not read.
const MAX_FILE: u64 = 1024 * 1024;

/// Where an entry sits in its file (line numbers, 0-based).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Span {
    Line(usize),
    /// Lines `start..end` of a stanza (blank separators excluded).
    Stanza(usize, usize),
}

/// One `deb`/`deb-src` line or deb822 stanza.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub enabled: bool,
    pub types: Vec<String>,
    pub uris: Vec<String>,
    pub suites: Vec<String>,
    pub components: Vec<String>,
    /// Other options as written: one-line `[key=value]` (`arch`, `signed-by`, `arch+`…) or
    /// deb822 fields (`Architectures`, `Signed-By`…).
    pub options: Vec<(String, String)>,
    pub comment: String,
    pub span: Span,
}

impl Entry {
    /// The value of option `names` (any of them, case-insensitive).
    pub fn option(&self, names: &[&str]) -> Option<&str> {
        self.options
            .iter()
            .find(|(k, _)| names.iter().any(|n| k.eq_ignore_ascii_case(n)))
            .map(|(_, v)| v.as_str())
    }

    pub fn signed_by(&self) -> Option<&str> {
        self.option(&["signed-by"])
    }

    pub fn is_deb(&self) -> bool {
        self.types.iter().any(|t| t == "deb")
    }
}

/// A sources file as read.
#[derive(Debug, Clone)]
pub struct SourceFile {
    /// The system path (`/etc/apt/…`).
    pub path: String,
    /// Where it is read from (below the root).
    pub real: PathBuf,
    pub text: String,
    pub format: RepoFormat,
    pub entries: Vec<Entry>,
    pub error: Option<String>,
}

impl SourceFile {
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

/// Every sources file of the system, the files apt ignores, and the digest.
#[derive(Debug, Clone, Default)]
pub struct Scan {
    pub files: Vec<SourceFile>,
    pub ignored: Vec<IgnoredFile>,
    pub digest: String,
}

impl Scan {
    pub fn file(&self, path: &str) -> Option<&SourceFile> {
        self.files.iter().find(|f| f.path == path)
    }
}

/// apt's `Dir::Ignore-Files-Silently`: backups and package manager leftovers.
fn silently_ignored(name: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        "~", ".disabled", ".bak", ".save", ".orig", ".distUpgrade", ".rynas-new",
    ];
    if SUFFIXES.iter().any(|s| name.ends_with(s)) {
        return true;
    }
    // `.dpkg-old`, `.ucf-dist`…
    name.rsplit_once('.').is_some_and(|(_, ext)| {
        ["dpkg-", "ucf-"].iter().any(|p| {
            ext.strip_prefix(p)
                .is_some_and(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_lowercase()))
        })
    })
}

/// Reads every sources file below `root`.
pub fn scan(root: &Path) -> Scan {
    let mut out = Scan::default();
    let main = files::under_root(root, Path::new(MAIN_LIST));
    if main.is_file() {
        out.files.push(read_file(MAIN_LIST.to_string(), main));
    }
    let dir = files::under_root(root, Path::new(LIST_DIR));
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_file())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    for name in names {
        let path = format!("{LIST_DIR}/{name}");
        let ext = name.rsplit_once('.').map(|(_, e)| e);
        let known = matches!(ext, Some("list" | "sources"));
        if known && apt_file_name_ok(&name) {
            out.files.push(read_file(path, dir.join(&name)));
        } else {
            let reason = if silently_ignored(&name) {
                "backup"
            } else if known {
                "name"
            } else {
                "extension"
            };
            out.ignored.push(IgnoredFile { path, reason: reason.into() });
        }
    }
    out.digest = digest(&out);
    out
}

fn read_file(path: String, real: PathBuf) -> SourceFile {
    let format = if path.ends_with(".sources") { RepoFormat::Deb822 } else { RepoFormat::OneLine };
    let mut f = SourceFile { path, real, text: String::new(), format, entries: Vec::new(), error: None };
    match std::fs::metadata(&f.real) {
        Ok(m) if m.len() > MAX_FILE => {
            f.error = Some(format!("larger than {} KiB: not read", MAX_FILE / 1024));
            return f;
        }
        Err(e) => {
            f.error = Some(e.to_string());
            return f;
        }
        _ => {}
    }
    match std::fs::read(&f.real) {
        Ok(b) => match String::from_utf8(b) {
            Ok(t) => f.text = t,
            Err(_) => {
                f.error = Some("not UTF-8 text".into());
                return f;
            }
        },
        Err(e) => {
            f.error = Some(e.to_string());
            return f;
        }
    }
    let parsed = match format {
        RepoFormat::OneLine => parse_one_line(&f.text),
        RepoFormat::Deb822 => parse_deb822(&f.text),
    };
    match parsed {
        Ok(e) => f.entries = e,
        Err(e) => f.error = Some(e),
    }
    f
}

/// sha256 over the sorted paths and contents of the files read and the names of the
/// ignored ones.
pub fn digest(s: &Scan) -> String {
    let mut parts: Vec<(&str, &str)> = s.files.iter().map(|f| (f.path.as_str(), f.text.as_str())).collect();
    parts.extend(s.ignored.iter().map(|i| (i.path.as_str(), "")));
    parts.sort();
    let mut h = sha2::Sha256::new();
    for (p, t) in parts {
        h.update(p.as_bytes());
        h.update([0]);
        h.update(t.as_bytes());
        h.update([0]);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

// ------------------------------------------------------------------ one-line

/// One `deb …` line (no leading `#`, inline comment already cut): `None` if it is not one.
pub fn parse_line(s: &str) -> Option<Result<Entry, String>> {
    let s = s.trim();
    let (ty, rest) = s.split_once(|c: char| c.is_whitespace() || c == '[')?;
    if ty != "deb" && ty != "deb-src" {
        return None;
    }
    // the bracket may follow the type without a space (`deb[arch=amd64] …`)
    let rest = if s.as_bytes().get(ty.len()) == Some(&b'[') { &s[ty.len()..] } else { rest };
    let rest = rest.trim_start();
    let (opts, rest) = match rest.strip_prefix('[') {
        Some(r) => match r.split_once(']') {
            Some((o, r)) => (o, r),
            None => return Some(Err("option list without its closing ]".into())),
        },
        None => ("", rest),
    };
    let mut options = Vec::new();
    for o in opts.split_whitespace() {
        let Some((k, v)) = o.split_once('=') else {
            return Some(Err(format!("option {o:?} has no value")));
        };
        options.push((k.to_string(), v.to_string()));
    }
    let rest = rest.trim_start();
    // `cdrom:[Debian GNU/Linux 13 …]/`: the label holds spaces
    let (cd, rest) = match rest.strip_prefix("cdrom:[").and_then(|r| r.split_once(']')) {
        Some((label, r)) => {
            let (tail, r) = r.split_once(char::is_whitespace).unwrap_or((r, ""));
            (Some(format!("cdrom:[{label}]{tail}")), r)
        }
        None => (None, rest),
    };
    let mut w = rest.split_whitespace();
    let uri_owned;
    let uri = match cd {
        Some(u) => {
            uri_owned = u;
            Some(uri_owned.as_str())
        }
        None => w.next(),
    };
    let (Some(uri), Some(suite)) = (uri, w.next()) else {
        return Some(Err("an entry needs a URI and a suite".into()));
    };
    let components: Vec<String> = w.map(String::from).collect();
    if suite.ends_with('/') && !components.is_empty() {
        return Some(Err(format!("the exact path {suite} takes no components")));
    }
    if !suite.ends_with('/') && components.is_empty() {
        return Some(Err(format!("{uri} {suite}: no component")));
    }
    Some(Ok(Entry {
        enabled: true,
        types: vec![ty.to_string()],
        uris: vec![uri.to_string()],
        suites: vec![suite.to_string()],
        components,
        options,
        comment: String::new(),
        span: Span::Line(0),
    }))
}

/// Entries of a one-line file. Comment lines right above an entry are its comment.
pub fn parse_one_line(text: &str) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    let mut comment: Vec<String> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() {
            comment.clear();
            continue;
        }
        if let Some(c) = t.strip_prefix('#') {
            let c = c.trim_start_matches('#').trim();
            match parse_line(c) {
                Some(Ok(mut e)) => {
                    e.enabled = false;
                    e.span = Span::Line(i);
                    e.comment = comment.join("\n");
                    comment.clear();
                    out.push(e);
                }
                _ => comment.push(c.to_string()),
            }
            continue;
        }
        let body = t.split('#').next().unwrap_or("");
        match parse_line(body) {
            Some(Ok(mut e)) => {
                e.span = Span::Line(i);
                e.comment = comment.join("\n");
                comment.clear();
                out.push(e);
            }
            Some(Err(e)) => return Err(format!("line {}: {e}", i + 1)),
            None => return Err(format!("line {}: not an apt source line", i + 1)),
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------ deb822

/// apt's string to boolean (`no`, `false`, `off`… are false).
pub fn truthy(v: &str) -> bool {
    !matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "no" | "false" | "off" | "0" | "disable" | "without"
    )
}

/// The stanzas of a deb822 text as line ranges (comment-only blocks are not stanzas).
pub fn stanzas(text: &str) -> Vec<(usize, usize)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, l) in lines.iter().enumerate() {
        if l.trim().is_empty() {
            if let Some(s) = start.take() {
                out.push((s, i));
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        out.push((s, lines.len()));
    }
    out.into_iter()
        .filter(|&(s, e)| lines[s..e].iter().any(|l| !l.trim_start().starts_with('#')))
        .collect()
}

/// The fields of a stanza: `(key as written, value, first line, last line)`; continuation
/// lines are joined with `\n`.
pub fn fields(lines: &[&str], start: usize) -> Vec<(String, String, usize, usize)> {
    let mut out: Vec<(String, String, usize, usize)> = Vec::new();
    for (k, l) in lines.iter().enumerate() {
        let i = start + k;
        if l.trim_start().starts_with('#') {
            continue;
        }
        if l.starts_with([' ', '\t']) {
            if let Some(f) = out.last_mut() {
                let v = l.trim();
                if !f.1.is_empty() {
                    f.1.push('\n');
                }
                // a lone `.` is an empty line of a multi-line value
                f.1.push_str(if v == "." { "" } else { v });
                f.3 = i;
            }
            continue;
        }
        if let Some((k, v)) = l.split_once(':') {
            out.push((k.trim().to_string(), v.trim().to_string(), i, i));
        }
    }
    out
}

pub fn parse_deb822(text: &str) -> Result<Vec<Entry>, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (n, (s, e)) in stanzas(text).into_iter().enumerate() {
        let fs = fields(&lines[s..e], s);
        let get = |k: &str| fs.iter().find(|f| f.0.eq_ignore_ascii_case(k)).map(|f| f.1.as_str());
        let words = |k: &str| get(k).map(|v| v.split_whitespace().map(String::from).collect::<Vec<_>>()).unwrap_or_default();
        let enabled = get("enabled").is_none_or(truthy);
        let entry = Entry {
            enabled,
            types: words("types"),
            uris: words("uris"),
            suites: words("suites"),
            components: words("components"),
            options: fs
                .iter()
                .filter(|f| !["types", "uris", "suites", "components", "enabled"].iter().any(|k| f.0.eq_ignore_ascii_case(k)))
                .map(|f| (f.0.clone(), f.1.clone()))
                .collect(),
            comment: lines[s..e]
                .iter()
                .take_while(|l| l.trim_start().starts_with('#'))
                .map(|l| l.trim_start().trim_start_matches('#').trim())
                .collect::<Vec<_>>()
                .join("\n"),
            span: Span::Stanza(s, e),
        };
        let bad = if entry.types.is_empty() {
            Some("no Types")
        } else if entry.types.iter().any(|t| t != "deb" && t != "deb-src") {
            Some("Types other than deb and deb-src")
        } else if entry.uris.is_empty() {
            Some("no URIs")
        } else if entry.suites.is_empty() {
            Some("no Suites")
        } else if entry.suites.iter().any(|x| x.ends_with('/')) && !entry.components.is_empty() {
            Some("an exact path takes no Components")
        } else if entry.suites.iter().any(|x| !x.ends_with('/')) && entry.components.is_empty() {
            Some("no Components")
        } else {
            None
        };
        if let Some(b) = bad {
            // apt skips a disabled stanza before checking it
            if enabled {
                return Err(format!("stanza {} (line {}): {b}", n + 1, s + 1));
            }
            continue;
        }
        out.push(entry);
    }
    Ok(out)
}

// ------------------------------------------------------------------ edits

fn split_keep(text: &str) -> Vec<String> {
    text.split_inclusive('\n').map(String::from).collect()
}

fn eol(l: &str) -> &str {
    if l.ends_with('\n') { "\n" } else { "" }
}

/// `text` with `entry` enabled or disabled; every other byte unchanged.
pub fn toggle(text: &str, entry: &Entry, on: bool) -> String {
    let mut lines = split_keep(text);
    match entry.span {
        Span::Line(i) => {
            let Some(l) = lines.get_mut(i) else { return text.to_string() };
            let body = l.trim_end_matches('\n');
            let indent = &body[..body.len() - body.trim_start().len()];
            let new = if on {
                let t = body.trim_start().trim_start_matches('#').trim_start();
                format!("{indent}{t}")
            } else if body.trim_start().starts_with('#') {
                body.to_string()
            } else {
                format!("{indent}# {}", body.trim_start())
            };
            *l = format!("{new}{}", eol(l));
        }
        Span::Stanza(s, e) => {
            let view: Vec<&str> = text.lines().collect();
            let fs = fields(&view[s..e.min(view.len())], s);
            let en: Vec<&(String, String, usize, usize)> = fs.iter().filter(|f| f.0.eq_ignore_ascii_case("enabled")).collect();
            if on {
                // drop every Enabled line (and its continuation lines)
                let mut drop: Vec<usize> = en.iter().flat_map(|f| f.2..=f.3).collect();
                drop.sort_unstable();
                for i in drop.into_iter().rev() {
                    if i < lines.len() {
                        lines.remove(i);
                    }
                }
            } else if let Some(f) = en.first() {
                let l = &lines[f.2];
                lines[f.2] = format!("{}: no{}", f.0, eol(l));
                for i in (f.2 + 1..=f.3).rev() {
                    lines.remove(i);
                }
            } else {
                // before the first field (after the stanza's leading comments)
                let at = fs.first().map_or(s, |f| f.2);
                lines.insert(at, "Enabled: no\n".into());
            }
        }
    }
    lines.concat()
}

/// The text of a stanza (its lines).
pub fn stanza_text(text: &str, s: usize, e: usize) -> String {
    text.lines().skip(s).take(e - s).map(|l| format!("{l}\n")).collect()
}

/// 0644 root file written next to it then renamed (as the backports switch writes).
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".rynas-new");
    let tmp = PathBuf::from(tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    // rynasd's UMask=0077 strips the mode given to open
    f.set_permissions(std::fs::Permissions::from_mode(0o644))?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "# main archive\ndeb http://deb.debian.org/debian trixie main non-free non-free-firmware\ndeb-src http://deb.debian.org/debian trixie main non-free non-free-firmware\n\n#deb [arch=amd64 signed-by=/etc/apt/keyrings/x.asc] https://x.example/apt stable main # vendor\ndeb http://security.debian.org/debian-security trixie-security main\n";

    const DEB822: &str = "# Debian\nTypes: deb deb-src\nURIs: http://deb.debian.org/debian\nSuites: trixie trixie-updates\nComponents: main contrib\nSigned-By: /usr/share/keyrings/debian-archive-keyring.pgp\nX-Note: kept\n\n# off\nTypes: deb\nEnabled: no\nURIs: https://x.example/apt\nSuites: ./\n";

    #[test]
    fn one_line_entries_and_comments() {
        let e = parse_one_line(LIST).unwrap();
        assert_eq!(e.len(), 4);
        assert_eq!(e[0].comment, "main archive");
        assert_eq!(e[1].types, ["deb-src"]);
        assert!(!e[2].enabled);
        assert_eq!(e[2].signed_by(), Some("/etc/apt/keyrings/x.asc"));
        assert_eq!(e[2].option(&["arch"]), Some("amd64"));
        assert_eq!(e[3].span, Span::Line(5));
        assert!(parse_one_line("deb http://x\n").unwrap_err().contains("line 1"));
        assert!(parse_one_line("deb http://x trixie\n").is_err());
        assert!(parse_one_line("garbage\n").is_err());
        assert_eq!(parse_one_line("deb[trusted=yes] http://x ./\n").unwrap()[0].option(&["trusted"]), Some("yes"));
    }

    #[test]
    fn deb822_entries_multi_value() {
        let e = parse_deb822(DEB822).unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].types, ["deb", "deb-src"]);
        assert_eq!(e[0].suites, ["trixie", "trixie-updates"]);
        assert_eq!(e[0].comment, "Debian");
        assert!(e[0].options.contains(&("X-Note".into(), "kept".into())));
        assert!(!e[1].enabled);
        assert_eq!(e[1].suites, ["./"]);
        // malformed and enabled: an error; malformed and disabled: skipped
        assert!(parse_deb822("Types: deb\nURIs: http://x\n").is_err());
        assert!(parse_deb822("Types: deb\nEnabled: false\nURIs: http://x\n").unwrap().is_empty());
        // a multi-line inline key
        let k = parse_deb822("Types: deb\nURIs: http://x\nSuites: s\nComponents: main\nSigned-By:\n -----BEGIN PGP PUBLIC KEY BLOCK-----\n .\n abc\n -----END PGP PUBLIC KEY BLOCK-----\n").unwrap();
        assert!(k[0].signed_by().unwrap().starts_with("-----BEGIN"));
    }

    #[test]
    fn toggles_keep_every_other_byte() {
        let e = parse_one_line(LIST).unwrap();
        let on = toggle(LIST, &e[2], true);
        assert!(on.contains("\ndeb [arch=amd64 signed-by=/etc/apt/keyrings/x.asc] https://x.example/apt stable main # vendor\n"), "{on}");
        assert_eq!(on.lines().count(), LIST.lines().count());
        let e2 = parse_one_line(&on).unwrap();
        assert!(e2[2].enabled);
        let off = toggle(&on, &e2[2], false);
        assert!(off.contains("\n# deb [arch=amd64"), "{off}");
        // the other lines are the same bytes
        let off0 = toggle(LIST, &e[0], false);
        assert_eq!(off0.replacen("# deb http://deb.debian.org/debian trixie main", "deb http://deb.debian.org/debian trixie main", 1), LIST);

        let d = parse_deb822(DEB822).unwrap();
        let off = toggle(DEB822, &d[0], false);
        assert!(off.starts_with("# Debian\nEnabled: no\nTypes: deb deb-src\n"), "{off}");
        assert!(off.contains("X-Note: kept\n"));
        assert!(!parse_deb822(&off).unwrap()[0].enabled);
        let on = toggle(&off, &parse_deb822(&off).unwrap()[0], true);
        assert_eq!(on, DEB822);
        let on1 = toggle(DEB822, &d[1], true);
        assert!(!on1.contains("Enabled"), "{on1}");
        assert!(parse_deb822(&on1).unwrap()[1].enabled);
        // an existing Enabled: yes becomes no in place
        let y = "Types: deb\nURIs: http://x\nSuites: s\nEnabled: yes\nComponents: main\n";
        assert_eq!(toggle(y, &parse_deb822(y).unwrap()[0], false), "Types: deb\nURIs: http://x\nSuites: s\nEnabled: no\nComponents: main\n");
    }

    #[test]
    fn scan_ignored_files_and_digest() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("etc/apt/sources.list.d");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(d.path().join("etc/apt/sources.list"), LIST).unwrap();
        std::fs::write(dir.join("debian.sources"), DEB822).unwrap();
        std::fs::write(dir.join("broken.sources"), "Types: deb\n").unwrap();
        std::fs::write(dir.join("old.list.bak"), "x").unwrap();
        std::fs::write(dir.join("my repo.list"), "x").unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        std::fs::write(dir.join("x.list.dpkg-old"), "x").unwrap();
        let s = scan(d.path());
        let paths: Vec<&str> = s.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["/etc/apt/sources.list", "/etc/apt/sources.list.d/broken.sources", "/etc/apt/sources.list.d/debian.sources"]);
        assert!(s.files[1].error.is_some() && s.files[2].error.is_none());
        let ign: Vec<(&str, &str)> = s.ignored.iter().map(|i| (i.path.rsplit('/').next().unwrap(), i.reason.as_str())).collect();
        assert_eq!(ign, [("my repo.list", "name"), ("notes.txt", "extension"), ("old.list.bak", "backup"), ("x.list.dpkg-old", "backup")]);
        assert_eq!(s.digest.len(), 64);
        let before = s.digest.clone();
        assert_eq!(scan(d.path()).digest, before);
        std::fs::write(dir.join("debian.sources"), format!("{DEB822}\n")).unwrap();
        assert_ne!(scan(d.path()).digest, before);
    }

    #[test]
    fn atomic_write_is_0644() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("etc/apt/sources.list.d/x.sources");
        write_atomic(&p, "x\n").unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o644);
    }
}
