//! What `apt-get update` (run with `LC_ALL=C`) said about each source: its `Err:<n>` blocks
//! and `W:`/`E:` lines (`Warning:`/`Error:` for apt 3's `apt`), mapped to a kind, a URI and a
//! suite so that the page can show them on the entry they concern. apt's status-fd has no
//! per-source failure; these lines are the only place apt names them.

use rynas_api_types::repositories::{FetchError, FetchErrorKind};

/// The kind of problem a line of apt describes.
pub fn kind_of(text: &str) -> FetchErrorKind {
    let t = text.to_ascii_lowercase();
    if text.contains("NO_PUBKEY") {
        FetchErrorKind::NoPubkey
    } else if text.contains("EXPKEYSIG") || text.contains("KEYEXPIRED") || t.contains("expired key") {
        FetchErrorKind::ExpiredKey
    } else if text.contains("BADSIG") {
        FetchErrorKind::BadSignature
    } else if t.contains("is not signed") {
        FetchErrorKind::NotSigned
    } else if t.contains("is expired (invalid since") || t.contains("is not valid yet") {
        FetchErrorKind::ExpiredRelease
    } else if t.contains("changed its '") {
        FetchErrorKind::ReleaseInfoChanged
    } else if t.contains("configured multiple times") {
        FetchErrorKind::Duplicate
    } else if t.contains(" 404 ") || t.contains("404  not found") || t.contains("does not have a release file") || t.contains("no longer has a release file") {
        FetchErrorKind::NotFound
    } else if [
        "temporary failure resolving",
        "could not resolve",
        "could not connect",
        "connection failed",
        "connection timed out",
        "network is unreachable",
        "connection refused",
        "no route to host",
        "unable to connect",
    ]
    .iter()
    .any(|p| t.contains(p))
    {
        FetchErrorKind::Network
    } else {
        FetchErrorKind::Other
    }
}

/// The key ids after `NO_PUBKEY`/`EXPKEYSIG`/`BADSIG`/`KEYEXPIRED`.
fn keys_of(text: &str) -> Vec<String> {
    let w: Vec<&str> = text.split_whitespace().collect();
    w.windows(2)
        .filter(|p| ["NO_PUBKEY", "EXPKEYSIG", "BADSIG", "KEYEXPIRED"].contains(&p[0]))
        .map(|p| p[1].trim_end_matches([',', '.']).to_string())
        .collect()
}

fn is_uri(s: &str) -> bool {
    ["http://", "https://", "ftp://", "file:", "cdrom:", "mirror+", "tor+"].iter().any(|p| s.starts_with(p))
}

/// `<uri> <suite> <file>` at the start of `s` (as in `Err:` lines, `GPG error:` and
/// `The repository '…'`).
fn uri_suite(s: &str) -> Option<(String, Option<String>)> {
    let mut w = s.split_whitespace();
    let u = w.next()?.trim_matches('\'');
    if !is_uri(u) {
        return None;
    }
    let suite = w.next().map(|x| x.trim_matches('\'').trim_end_matches(':').to_string());
    Some((u.to_string(), suite.filter(|x| !x.is_empty())))
}

/// The URI and suite named in a `W:`/`E:` message.
fn locate(msg: &str) -> Option<(String, Option<String>)> {
    for p in ["GPG error: ", "The repository '", "Repository '"] {
        if let Some(i) = msg.find(p) {
            return uri_suite(&msg[i + p.len()..]);
        }
    }
    for p in ["Failed to fetch ", "Release file for "] {
        if let Some(i) = msg.find(p) {
            let u = msg[i + p.len()..].split_whitespace().next()?;
            return is_uri(u).then(|| (u.to_string(), None));
        }
    }
    // `W: https://x/dists/y/InRelease: Key is stored in legacy trusted.gpg keyring`
    let first = msg.split_whitespace().next()?;
    let u = first.trim_end_matches(':');
    is_uri(u).then(|| (u.to_string(), None))
}

/// Lines that say nothing about a particular source.
fn generic(msg: &str) -> bool {
    msg.starts_with("Some index files failed to download")
        || msg.starts_with("See apt-secure")
        || msg.starts_with("Updating from such a repository")
        || msg.starts_with("Data from such a repository")
}

/// Every problem of an `apt-get update` output, duplicates merged.
pub fn parse(out: &str) -> Vec<FetchError> {
    let mut v: Vec<FetchError> = Vec::new();
    let lines: Vec<&str> = out.lines().collect();
    // `Err:3 http://x suite InRelease` and the reason on the next (indented) line
    for (i, l) in lines.iter().enumerate() {
        let Some(rest) = l.strip_prefix("Err:") else { continue };
        let rest = rest.split_once(char::is_whitespace).map_or("", |(_, r)| r);
        let Some((uri, suite)) = uri_suite(rest) else { continue };
        let reason = lines.get(i + 1).filter(|n| n.starts_with([' ', '\t'])).map_or("", |n| n.trim());
        v.push(FetchError { kind: kind_of(reason), error: true, uri, suite, keys: keys_of(reason), message: reason.to_string() });
    }
    for l in &lines {
        let (error, msg) = if let Some(m) = l.strip_prefix("E: ").or_else(|| l.strip_prefix("Error: ")) {
            (true, m)
        } else if let Some(m) = l.strip_prefix("W: ").or_else(|| l.strip_prefix("Warning: ")) {
            (false, m)
        } else {
            continue;
        };
        if generic(msg) {
            continue;
        }
        let kind = kind_of(msg);
        let (uri, suite) = locate(msg).unwrap_or_default();
        // already known from its Err block (same source, same kind)
        if suite.is_none()
            && !uri.is_empty()
            && v.iter().any(|x| x.kind == kind && x.suite.as_ref().is_some_and(|s| uri.starts_with(x.uri.trim_end_matches('/')) && uri.contains(&format!("/dists/{s}/"))))
        {
            continue;
        }
        if let Some(x) = v.iter_mut().find(|x| x.kind == kind && x.uri == uri && x.suite == suite) {
            x.error |= error;
            x.message = msg.to_string();
            continue;
        }
        v.push(FetchError { kind, error, uri, suite, keys: keys_of(msg), message: msg.to_string() });
    }
    // "not signed" because of a missing key: one problem, the key's, as an error
    let unsigned: Vec<(String, Option<String>)> = v.iter().filter(|x| x.kind == FetchErrorKind::NotSigned).map(|x| (x.uri.clone(), x.suite.clone())).collect();
    for x in v.iter_mut() {
        if matches!(x.kind, FetchErrorKind::NoPubkey | FetchErrorKind::ExpiredKey | FetchErrorKind::BadSignature) && unsigned.contains(&(x.uri.clone(), x.suite.clone())) {
            x.error = true;
        }
    }
    let keyed: Vec<(String, Option<String>)> = v
        .iter()
        .filter(|y| matches!(y.kind, FetchErrorKind::NoPubkey | FetchErrorKind::ExpiredKey | FetchErrorKind::BadSignature))
        .map(|y| (y.uri.clone(), y.suite.clone()))
        .collect();
    v.retain(|x| x.kind != FetchErrorKind::NotSigned || !keyed.contains(&(x.uri.clone(), x.suite.clone())));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `apt-get update` with LC_ALL=C (apt 2.6/3.0), several sources failing at once.
    pub const SAMPLE: &str = "Hit:1 http://deb.debian.org/debian trixie InRelease
Get:2 http://deb.debian.org/debian trixie-updates InRelease [47.3 kB]
Err:3 https://repo.example.com/apt stable InRelease
  The following signatures couldn't be verified because the public key is not available: NO_PUBKEY 0123456789ABCDEF
Err:4 http://deb.debian.org/debian trixie-backportz Release
  404  Not Found [IP: 151.101.2.132 80]
Ign:5 http://bad.example.invalid/debian trixie InRelease
Err:5 http://bad.example.invalid/debian trixie InRelease
  Temporary failure resolving 'bad.example.invalid'
Reading package lists...
W: GPG error: https://repo.example.com/apt stable InRelease: The following signatures couldn't be verified because the public key is not available: NO_PUBKEY 0123456789ABCDEF
E: The repository 'https://repo.example.com/apt stable InRelease' is not signed.
N: Updating from such a repository can't be done securely, and is therefore disabled by default.
E: The repository 'http://deb.debian.org/debian trixie-backportz Release' does not have a Release file.
W: Failed to fetch http://bad.example.invalid/debian/dists/trixie/InRelease  Temporary failure resolving 'bad.example.invalid'
W: Some index files failed to download. They have been ignored, or old ones used instead.
E: Release file for http://archive.example.org/debian/dists/buster/InRelease is expired (invalid since 300d 2h 1min 5s). Updates for this repository will not be applied.
W: GPG error: http://x.example/deb stable InRelease: The following signatures were invalid: EXPKEYSIG ABCDEF0123456789 Example Signing Key <k@example.org>
W: GPG error: http://y.example/deb stable InRelease: The following signatures were invalid: BADSIG 1111222233334444 Example
E: Repository 'http://z.example/deb stable InRelease' changed its 'Origin' value from 'Old' to 'New'
W: Target Packages (main/binary-amd64/Packages) is configured multiple times in /etc/apt/sources.list:1 and /etc/apt/sources.list.d/debian.sources:1
W: http://legacy.example/deb/dists/stable/InRelease: Key is stored in legacy trusted.gpg keyring (/etc/apt/trusted.gpg), see the DEPRECATION section in apt-key(8) for details.
";

    fn find<'a>(v: &'a [FetchError], uri: &str) -> Vec<&'a FetchError> {
        v.iter().filter(|x| x.uri.starts_with(uri)).collect()
    }

    #[test]
    fn maps_every_kind() {
        let v = parse(SAMPLE);
        let repo = find(&v, "https://repo.example.com");
        assert_eq!(repo.len(), 1, "{repo:?}");
        assert_eq!(repo[0].kind, FetchErrorKind::NoPubkey);
        assert!(repo[0].error);
        assert_eq!(repo[0].keys, ["0123456789ABCDEF"]);
        assert_eq!(repo[0].suite.as_deref(), Some("stable"));
        let bp = find(&v, "http://deb.debian.org");
        assert_eq!(bp.len(), 1, "{bp:?}");
        assert_eq!(bp[0].kind, FetchErrorKind::NotFound);
        assert_eq!(bp[0].suite.as_deref(), Some("trixie-backportz"));
        let dns = find(&v, "http://bad.example.invalid");
        assert_eq!(dns.len(), 1, "{dns:?}");
        assert_eq!(dns[0].kind, FetchErrorKind::Network);
        assert_eq!(find(&v, "http://archive.example.org")[0].kind, FetchErrorKind::ExpiredRelease);
        let exp = find(&v, "http://x.example");
        assert_eq!(exp[0].kind, FetchErrorKind::ExpiredKey);
        assert_eq!(exp[0].keys, ["ABCDEF0123456789"]);
        assert!(!exp[0].error);
        assert_eq!(find(&v, "http://y.example")[0].kind, FetchErrorKind::BadSignature);
        assert_eq!(find(&v, "http://z.example")[0].kind, FetchErrorKind::ReleaseInfoChanged);
        assert!(v.iter().any(|x| x.kind == FetchErrorKind::Duplicate && x.uri.is_empty()));
        assert_eq!(find(&v, "http://legacy.example")[0].kind, FetchErrorKind::Other);
        assert!(!v.iter().any(|x| x.message.starts_with("Some index files")));
        // apt 3's `apt` words
        let w = parse("Warning: GPG error: http://x.example/deb stable InRelease: The following signatures were invalid: KEYEXPIRED 1700000000\n");
        assert_eq!(w[0].kind, FetchErrorKind::ExpiredKey);
        assert!(parse("Hit:1 http://deb.debian.org/debian trixie InRelease\nReading package lists...\n").is_empty());
    }
}
