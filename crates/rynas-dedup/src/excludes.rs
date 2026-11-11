//! The exclude list of a run (oans `--exclude`, gitignore syntax).
//!
//! One run covers several shares and has one exclude list, so everything tied to a share is
//! anchored at the share's absolute path (a pattern starting with `/` is absolute in oans; a
//! bare `#recycle` would be a gitignore comment). Name patterns valid everywhere (`.snapshots`,
//! partial copies of the file manager, the names a hardware module adds) stay unanchored.

use rynas_api_types::SNAPSHOT_FOLDER;
use rynas_api_types::dedup::ShareDedup;

/// Folders and files never deduplicated, matched by name at any depth.
pub const ALWAYS_NAMES: &[&str] = &[
    ".snapshots/",
    "@rynas-cache/",
    ".replicas/",
    ".rynas-crypt/",
    ".*.rynas-part",
    ".*.rynas-aside",
];

/// The recycle bin folder at a share's root.
pub const RECYCLE_FOLDER: &str = "#recycle";

/// What the UI lists as always excluded (relative to each share), the recycle bin first
/// (it can be included per share).
pub fn shown_always(module_names: &[String]) -> Vec<String> {
    let mut v = vec![format!("{SNAPSHOT_FOLDER}/"), format!("{RECYCLE_FOLDER}/")];
    v.extend(ALWAYS_NAMES.iter().map(|s| s.to_string()));
    for m in module_names {
        if let Some(p) = name_pattern(m)
            && !v.contains(&p)
        {
            v.push(p);
        }
    }
    v
}

/// A path as the literal start of a pattern: `\` before the characters gitignore treats
/// specially.
pub fn escape_path(p: &str) -> String {
    let mut s = String::with_capacity(p.len() + 4);
    for c in p.chars() {
        if matches!(c, '\\' | '*' | '?' | '[' | ']' | ' ' | '!' | '#') {
            s.push('\\');
        }
        s.push(c);
    }
    s
}

/// A module's name pattern made safe as an unanchored oans pattern (`None` when unusable):
/// a leading `#` would start a comment, `!` is not supported, `/` inside would anchor it.
pub fn name_pattern(n: &str) -> Option<String> {
    let t = n.trim();
    let base = t.trim_end_matches('/');
    if base.is_empty() || base.contains('/') || t.contains('!') || t.chars().any(char::is_control) || base == ".." || base == "." {
        return None;
    }
    let mut p = if t.starts_with('#') { format!("\\{t}") } else { t.to_string() };
    if t.ends_with('/') && !p.ends_with('/') {
        p.push('/');
    }
    Some(p)
}

/// One user pattern of a share, anchored at `root`: a pattern without `/` (but a trailing
/// one) matches at any depth inside the share; one with a `/` is relative to the share's
/// root (as in a `.gitignore` there).
pub fn anchor(root: &str, pattern: &str) -> String {
    let base = escape_path(root.trim_end_matches('/'));
    let p = pattern.trim();
    let body = p.trim_end_matches('/');
    let trailing = if p.ends_with('/') { "/" } else { "" };
    if body.contains('/') {
        format!("{base}/{}{trailing}", body.trim_start_matches('/'))
    } else {
        format!("{base}/**/{body}{trailing}")
    }
}

/// A share in a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root<'a> {
    pub path: &'a str,
    pub settings: &'a ShareDedup,
}

/// The whole exclude list of a run: per share the snapshot folder, the recycle bin (unless
/// included) and its own patterns; then the names valid everywhere, the module's names and
/// absolute folders (Docker's engine data, local backup repositories).
pub fn build(roots: &[Root<'_>], module_names: &[String], absolute: &[String]) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    let push = |p: String, v: &mut Vec<String>| {
        if !v.contains(&p) {
            v.push(p);
        }
    };
    for r in roots {
        let base = escape_path(r.path.trim_end_matches('/'));
        push(format!("{base}/{}/", escape_path(SNAPSHOT_FOLDER)), &mut v);
        if !r.settings.include_recycle {
            push(format!("{base}/{}/", escape_path(RECYCLE_FOLDER)), &mut v);
        }
        for p in &r.settings.excludes {
            push(anchor(r.path, p), &mut v);
        }
    }
    for n in ALWAYS_NAMES {
        push(n.to_string(), &mut v);
    }
    for m in module_names {
        if let Some(p) = name_pattern(m) {
            push(p, &mut v);
        }
    }
    for a in absolute {
        if a.starts_with('/') {
            let t = a.trim_end_matches('/');
            push(format!("{}/", escape_path(if t.is_empty() { "/" } else { t })), &mut v);
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_patterns_are_anchored() {
        assert_eq!(anchor("/vol1/docs", "cache/"), "/vol1/docs/**/cache/");
        assert_eq!(anchor("/vol1/docs", "*.iso"), "/vol1/docs/**/*.iso");
        assert_eq!(anchor("/vol1/docs/", "/tmp/"), "/vol1/docs/tmp/");
        assert_eq!(anchor("/vol1/docs", "a/b*"), "/vol1/docs/a/b*");
        assert_eq!(anchor("/srv/my data", "x"), "/srv/my\\ data/**/x");
    }

    #[test]
    fn run_excludes() {
        let a = ShareDedup { enabled: true, excludes: vec!["cache/".into()], include_recycle: false };
        let b = ShareDedup { enabled: true, excludes: vec![], include_recycle: true };
        let ex = build(
            &[Root { path: "/vol1/docs", settings: &a }, Root { path: "/vol1/media", settings: &b }],
            &["@eaDir".into(), "#bad/".into(), "a/b".into(), "!x".into()],
            &["/vol1/docker/engine".into(), "relative".into()],
        );
        assert_eq!(
            ex,
            [
                "/vol1/docs/\\#snapshot/",
                "/vol1/docs/\\#recycle/",
                "/vol1/docs/**/cache/",
                "/vol1/media/\\#snapshot/",
                ".snapshots/",
                "@rynas-cache/",
                ".replicas/",
                ".rynas-crypt/",
                ".*.rynas-part",
                ".*.rynas-aside",
                "@eaDir",
                "\\#bad/",
                "/vol1/docker/engine/",
            ]
        );
        // nothing tied to a share is left unanchored
        assert!(!ex.iter().any(|p| p.starts_with('#')));
    }

    #[test]
    fn shown_list() {
        let v = shown_always(&[".@__thumb/".into()]);
        assert_eq!(v[0], "#snapshot/");
        assert_eq!(v[1], "#recycle/");
        assert!(v.contains(&".@__thumb/".to_string()));
    }
}
