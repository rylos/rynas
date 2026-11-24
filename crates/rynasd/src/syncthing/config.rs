//! The pure parts of a Syncthing adoption (`docs/phase2/syncthing.md` §3): reading
//! `config.xml`, rewriting folder paths and pausing folders with the smallest text edit, the
//! volume table and share mapping, the `.stfolder` marker rule, `.stignore` cleaning, and
//! finding a home among a container's mounts. No I/O here except through the closures given.

use std::collections::BTreeMap;
use std::ops::Range;

use rynas_api_types::syncthing::{AdoptVolume, StignoreProposal, UnmappedReason};
use sha2::{Digest, Sha256};

/// Syncthing's default marker folder name.
pub const DEFAULT_MARKER: &str = ".stfolder";

/// A folder of `config.xml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StFolder {
    pub id: String,
    pub label: String,
    pub path: String,
    pub folder_type: String,
    /// Full device ids.
    pub devices: Vec<String>,
    pub paused: bool,
    pub marker_name: String,
}

/// A device of `config.xml` (`<configuration><device>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StDevice {
    pub id: String,
    pub name: String,
}

/// What the adoption reads of `config.xml`. The GUI's user, password and API key are never
/// read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StConfig {
    pub folders: Vec<StFolder>,
    pub devices: Vec<StDevice>,
    pub gui_address: Option<String>,
    /// `<gui tls="true">`: the GUI serves HTTPS itself.
    pub gui_tls: bool,
    /// `<options><listenAddress>` values (sync connections).
    pub listen: Vec<String>,
}

fn child<'a, 'i>(n: roxmltree::Node<'a, 'i>, name: &str) -> Option<roxmltree::Node<'a, 'i>> {
    n.children().find(|c| c.is_element() && c.tag_name().name() == name)
}

fn child_text(n: roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    child(n, name).map(|c| c.text().unwrap_or("").trim().to_string())
}

/// Parses `config.xml`.
pub fn parse_config(xml: &str) -> Result<StConfig, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| format!("config.xml: {e}"))?;
    let root = doc.root_element();
    if root.tag_name().name() != "configuration" {
        return Err("config.xml: the root element is not <configuration>".into());
    }
    let mut out = StConfig::default();
    for n in root.children().filter(|c| c.is_element()) {
        match n.tag_name().name() {
            "folder" => {
                let a = |k: &str| n.attribute(k).unwrap_or("").to_string();
                let marker = child_text(n, "markerName").filter(|m| !m.is_empty()).unwrap_or_else(|| DEFAULT_MARKER.into());
                out.folders.push(StFolder {
                    id: a("id"),
                    label: a("label"),
                    path: a("path"),
                    folder_type: n.attribute("type").unwrap_or("sendreceive").to_string(),
                    devices: n
                        .children()
                        .filter(|c| c.is_element() && c.tag_name().name() == "device")
                        .filter_map(|c| c.attribute("id").map(str::to_string))
                        .collect(),
                    // older files carry it as an attribute
                    paused: child_text(n, "paused").as_deref() == Some("true") || n.attribute("paused") == Some("true"),
                    marker_name: marker,
                });
            }
            "device" => out.devices.push(StDevice {
                id: n.attribute("id").unwrap_or("").to_string(),
                name: n.attribute("name").unwrap_or("").to_string(),
            }),
            "gui" => {
                out.gui_address = child_text(n, "address").filter(|a| !a.is_empty());
                out.gui_tls = n.attribute("tls") == Some("true");
            }
            "options" => {
                out.listen = n
                    .children()
                    .filter(|c| c.is_element() && c.tag_name().name() == "listenAddress")
                    .map(|c| c.text().unwrap_or("").trim().to_string())
                    .filter(|a| !a.is_empty())
                    .collect();
            }
            _ => {}
        }
    }
    Ok(out)
}

/// A change to one folder of `config.xml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderEdit {
    pub id: String,
    /// The new `path`.
    pub path: Option<String>,
    /// Set `<paused>true</paused>`.
    pub pause: bool,
}

/// An attribute value escaped for XML: markup characters, and the whitespace an attribute
/// would otherwise normalize to spaces as character references.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
        .replace('\t', "&#9;")
        .replace('\n', "&#10;")
        .replace('\r', "&#13;")
}

/// Folder ids that appear more than once (Syncthing refuses such a file; so does adoption).
pub fn duplicate_ids(c: &StConfig) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut dup = std::collections::BTreeSet::new();
    for f in &c.folders {
        if !seen.insert(f.id.as_str()) {
            dup.insert(f.id.clone());
        }
    }
    dup.into_iter().collect()
}

/// The index of the `>` closing the start tag that begins at `start`.
fn start_tag_end(xml: &str, start: usize) -> Option<usize> {
    let mut quote: Option<u8> = None;
    for (i, b) in xml.as_bytes().iter().enumerate().skip(start) {
        match (quote, *b) {
            (None, b'"' | b'\'') => quote = Some(*b),
            (Some(q), c) if c == q => quote = None,
            (None, b'>') => return Some(i),
            _ => {}
        }
    }
    None
}

/// Applies `edits` to `xml` as text splices: only the changed attribute values and `paused`
/// elements differ, every other byte stays as it was.
pub fn rewrite_config(xml: &str, edits: &[FolderEdit]) -> Result<String, String> {
    let before = parse_config(xml)?;
    let dup = duplicate_ids(&before);
    if !dup.is_empty() {
        return Err(format!("config.xml: folder ids used twice: {}", dup.join(", ")));
    }
    let doc = roxmltree::Document::parse(xml).map_err(|e| format!("config.xml: {e}"))?;
    let mut splices: Vec<(Range<usize>, String)> = Vec::new();
    for e in edits {
        let n = doc
            .root_element()
            .children()
            .find(|c| c.is_element() && c.tag_name().name() == "folder" && c.attribute("id") == Some(e.id.as_str()))
            .ok_or_else(|| format!("config.xml: no folder {}", e.id))?;
        if let Some(p) = &e.path {
            let a = n.attribute_node("path").ok_or_else(|| format!("config.xml: folder {} has no path", e.id))?;
            splices.push((a.range_value(), xml_escape(p)));
        }
        if !e.pause {
            continue;
        }
        if let Some(a) = n.attribute_node("paused") {
            splices.push((a.range_value(), "true".into()));
        }
        match child(n, "paused") {
            Some(p) => match p.first_child().filter(|t| t.is_text()) {
                Some(t) => splices.push((t.range(), "true".into())),
                None => splices.push((p.range(), "<paused>true</paused>".into())),
            },
            None if n.attribute_node("paused").is_some() => {}
            None => {
                let gt = start_tag_end(xml, n.range().start).ok_or("config.xml: unterminated folder tag")?;
                if xml.as_bytes()[gt - 1] == b'/' {
                    splices.push((gt - 1..gt + 1, "><paused>true</paused></folder>".into()));
                } else {
                    splices.push((gt + 1..gt + 1, "<paused>true</paused>".into()));
                }
            }
        }
    }
    splices.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    let mut out = xml.to_string();
    for (r, s) in splices {
        out.replace_range(r, &s);
    }
    // the result must hold the same folders, each with exactly the expected path and state
    let back = parse_config(&out)?;
    if back.folders.len() != before.folders.len() || back.devices != before.devices {
        return Err("config.xml: the rewrite changed the folders or devices".into());
    }
    for (old, new) in before.folders.iter().zip(&back.folders) {
        let e = edits.iter().find(|e| e.id == old.id);
        let want_path = e.and_then(|e| e.path.clone()).unwrap_or_else(|| old.path.clone());
        let want_paused = old.paused || e.is_some_and(|e| e.pause);
        if new.id != old.id || new.path != want_path || new.paused != want_paused || new.devices != old.devices || new.marker_name != old.marker_name {
            return Err(format!("config.xml: the rewrite of folder {} did not give the expected result", old.id));
        }
    }
    Ok(out)
}

/// The port the app publishes for sync connections.
pub const SYNC_PORT: &str = "22000";

/// Sync listen addresses the app cannot serve: not `default`, not a relay, and not on port
/// 22000.
pub fn custom_listen(listen: &[String]) -> Vec<String> {
    listen
        .iter()
        .filter(|a| {
            let a = a.trim();
            if a == "default" || a.starts_with("relay://") || a.starts_with("dynamic+") {
                return false;
            }
            let rest = a.split_once("://").map(|(_, r)| r).unwrap_or(a);
            let host_port = rest.split(['/', '?']).next().unwrap_or("");
            host_port.rsplit_once(':').map(|(_, p)| p) != Some(SYNC_PORT)
        })
        .cloned()
        .collect()
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// The GUI's own HTTPS off (`<gui tls="false">`, rynas' proxy serves HTTPS) and, with
/// `reset_listen`, the sync listen addresses replaced by one `default`; text splices like
/// [`rewrite_config`], the result read back and checked.
pub fn rewrite_options(xml: &str, gui_tls_off: bool, reset_listen: bool) -> Result<String, String> {
    let before = parse_config(xml)?;
    let doc = roxmltree::Document::parse(xml).map_err(|e| format!("config.xml: {e}"))?;
    let root = doc.root_element();
    let mut splices: Vec<(Range<usize>, String)> = Vec::new();
    if gui_tls_off
        && let Some(a) = child(root, "gui").and_then(|g| g.attribute_node("tls"))
        && a.value() == "true"
    {
        splices.push((a.range_value(), "false".into()));
    }
    if reset_listen && let Some(o) = child(root, "options") {
        let ls: Vec<roxmltree::Node> = o.children().filter(|c| c.is_element() && c.tag_name().name() == "listenAddress").collect();
        for (i, l) in ls.iter().enumerate() {
            if i == 0 {
                match l.first_child().filter(|t| t.is_text()) {
                    Some(t) => splices.push((t.range(), "default".into())),
                    None => splices.push((l.range(), "<listenAddress>default</listenAddress>".into())),
                }
            } else {
                // the element and the indentation before it
                let mut start = l.range().start;
                while start > 0 && is_space(xml.as_bytes()[start - 1]) {
                    start -= 1;
                }
                splices.push((start..l.range().end, String::new()));
            }
        }
    }
    splices.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    let mut out = xml.to_string();
    for (r, s) in splices {
        out.replace_range(r, &s);
    }
    let back = parse_config(&out)?;
    let want_listen = if reset_listen && !before.listen.is_empty() { vec!["default".to_string()] } else { before.listen.clone() };
    if back.folders != before.folders
        || back.devices != before.devices
        || back.gui_address != before.gui_address
        || back.gui_tls != (before.gui_tls && !gui_tls_off)
        || back.listen != want_listen
    {
        return Err("config.xml: the rewrite of the GUI and listen settings did not give the expected result".into());
    }
    Ok(out)
}

/// RFC 4648 base32 (no padding) of `bytes`.
fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for b in bytes {
        buf = (buf << 8) | u32::from(*b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buf >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// The identity of a `cert.pem`: the first 7 characters of its device id (base32 of the
/// SHA-256 of the certificate's DER: the Luhn characters come later) and that SHA-256 in
/// hex. `None` without a certificate.
pub fn cert_identity(pem: &str) -> Option<(String, String)> {
    use base64::Engine;
    let body: String = pem
        .lines()
        .skip_while(|l| !l.starts_with("-----BEGIN CERTIFICATE-----"))
        .skip(1)
        .take_while(|l| !l.starts_with("-----END CERTIFICATE-----"))
        .map(str::trim)
        .collect();
    let der = base64::engine::general_purpose::STANDARD.decode(body).ok().filter(|d| !d.is_empty())?;
    let h = Sha256::digest(&der);
    let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
    Some((base32(&h).chars().take(7).collect(), hex))
}

/// The file Syncthing writes inside a folder's marker: `syncthing-folder-` + the first three
/// bytes of the SHA-256 of the folder id, hex, + `.txt` (`lib/config/folderconfiguration.go`).
pub fn marker_file_name(folder_id: &str) -> String {
    let h = Sha256::digest(folder_id.as_bytes());
    format!("syncthing-folder-{:02x}{:02x}{:02x}.txt", h[0], h[1], h[2])
}

/// `path` relative to `prefix` (`Some("")` for the prefix itself), `None` outside it.
pub fn under(path: &str, prefix: &str) -> Option<String> {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return Some(path.trim_start_matches('/').to_string());
    }
    if path == prefix {
        return Some(String::new());
    }
    path.strip_prefix(prefix).and_then(|r| r.strip_prefix('/')).map(|r| r.trim_end_matches('/').to_string())
}

/// `base/rest`, or `base` for an empty `rest`.
pub fn join(base: &str, rest: &str) -> String {
    let base = base.trim_end_matches('/');
    if rest.is_empty() { if base.is_empty() { "/".into() } else { base.to_string() } } else { format!("{base}/{rest}") }
}

fn normalize(p: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => return None,
            s => out.push(s),
        }
    }
    Some(format!("/{}", out.join("/")))
}

/// A folder path of the old configuration → its folder on this NAS. `table` maps the old
/// instance's paths to this host's (longest container prefix wins); an empty table means the
/// paths are host paths already. `home`: the old user's home, for `~/` paths.
pub fn host_path(path: &str, table: &[AdoptVolume], home: Option<&str>) -> Result<String, UnmappedReason> {
    let p = match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(h)) => join(h, rest),
        (Some(_), None) => return Err(UnmappedReason::BadPath),
        _ => path.to_string(),
    };
    if !p.starts_with('/') {
        return Err(UnmappedReason::BadPath);
    }
    let p = normalize(&p).ok_or(UnmappedReason::BadPath)?;
    if table.is_empty() {
        return Ok(p);
    }
    let joined = table
        .iter()
        .filter_map(|v| under(&p, &v.container).map(|rest| (v.container.trim_end_matches('/').len(), join(&v.host, &rest))))
        .max_by_key(|(n, _)| *n)
        .map(|(_, h)| h)
        .ok_or(UnmappedReason::NoVolume)?;
    // the table's host side is checked too, but never trusted
    if !joined.starts_with('/') {
        return Err(UnmappedReason::BadPath);
    }
    normalize(&joined).ok_or(UnmappedReason::BadPath)
}

/// Checks volume table lines typed by a user: both sides absolute, without `..`.
pub fn check_volumes(v: &[AdoptVolume]) -> Result<(), String> {
    for l in v {
        for (side, p) in [("container", &l.container), ("host", &l.host)] {
            if !p.starts_with('/') || p.split('/').any(|s| s == "..") || p.chars().any(char::is_control) {
                return Err(format!("volumes: {side} path `{p}` must be absolute, without `..`"));
            }
        }
    }
    Ok(())
}

/// `inner` is `outer` or below it (absolute, normalized paths).
pub fn is_within(inner: &str, outer: &str) -> bool {
    under(inner, outer).is_some()
}

/// The restart policy of one `docker inspect` object (`no` when none).
pub fn restart_policy(v: &serde_json::Value) -> String {
    let obj = v.as_array().and_then(|a| a.first()).unwrap_or(v);
    obj.pointer("/HostConfig/RestartPolicy/Name").and_then(serde_json::Value::as_str).filter(|s| !s.is_empty()).unwrap_or("no").to_string()
}

/// The Syncthing home folders a process names: `--home`/`-home`, `--config`/`--data` (the
/// value after `=` or the next argument), `STHOMEDIR`/`STCONFDIR`/`STDATADIR` in its
/// environment. Empty when it uses the default.
pub fn process_homes(cmdline: &[String], environ: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let keys = ["home", "config", "data"];
    let mut it = cmdline.iter().skip(1).peekable();
    while let Some(a) = it.next() {
        let flag = a.trim_start_matches('-');
        if !a.starts_with('-') {
            continue;
        }
        match flag.split_once('=') {
            Some((k, v)) if keys.contains(&k) => out.push(v.to_string()),
            None if keys.contains(&flag) => {
                if let Some(v) = it.next() {
                    out.push(v.clone());
                }
            }
            _ => {}
        }
    }
    for e in environ {
        if let Some((k, v)) = e.split_once('=')
            && matches!(k, "STHOMEDIR" | "STCONFDIR" | "STDATADIR")
            && !v.is_empty()
        {
            out.push(v.to_string());
        }
    }
    out.into_iter().map(|p| p.trim_end_matches('/').to_string()).collect()
}

/// A file of a home to copy: the file itself, or the target of a symlink when that target
/// is a regular file inside the home (`home` is the real, canonical folder). `None`: missing,
/// not a regular file, or a link out of the home.
pub fn home_file(home: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    let p = home.join(name);
    let m = std::fs::symlink_metadata(&p).ok()?;
    if m.is_file() {
        return Some(p);
    }
    if !m.file_type().is_symlink() {
        return None;
    }
    let target = std::fs::canonicalize(&p).ok()?;
    (target.starts_with(home) && target.is_file()).then_some(target)
}

/// A folder on this NAS → (share, path inside the app): the share whose path holds it
/// (longest wins), mounted at `<target>/<share>`. `shares` are (name, path).
pub fn share_path(host: &str, shares: &[(String, String)], target: &str) -> Result<(String, String), UnmappedReason> {
    shares
        .iter()
        .filter(|(_, p)| !p.is_empty())
        .filter_map(|(n, p)| under(host, p).map(|rest| (p.trim_end_matches('/').len(), n.clone(), rest)))
        .max_by_key(|(n, _, _)| *n)
        .map(|(_, n, rest)| (n.clone(), join(&format!("{}/{n}", target.trim_end_matches('/')), &rest)))
        .ok_or(UnmappedReason::NoShare)
}

/// Patterns of other NAS systems' own folders, never meant to be synchronized.
const FOREIGN: &[&str] = &["#snapshot", "@eaDir", ".SynologyWorkingDirectory", "@Recycle", ".@__thumb", ".streams", "#recycle"];
/// The recycle bin of rynas shares: ignored and deletable (`(?d)`), so a removed folder that
/// held deleted files can go.
pub const RECYCLE_LINE: &str = "(?d)#recycle";

/// The bare name a `.stignore` line matches, without prefixes (`(?d)`, `(?i)`, `!`), a leading
/// `/` or `**/` and a trailing `/` or `/**`.
fn bare_pattern(line: &str) -> &str {
    let mut l = line.trim();
    loop {
        let before = l;
        for p in ["(?d)", "(?i)", "!", "/", "**/"] {
            l = l.strip_prefix(p).unwrap_or(l);
        }
        if l == before {
            break;
        }
    }
    let l = l.strip_suffix("/**").unwrap_or(l);
    l.strip_suffix('/').unwrap_or(l)
}

/// The names of other systems' folders a `.stignore` cleaning looks for in the folder.
pub fn foreign_names() -> &'static [&'static str] {
    FOREIGN
}

/// The proposed `.stignore`: lines that only name other systems' folders out, unless
/// `present` says the folder holds a folder of that name (the line keeps it out of the
/// synchronization: kept, listed in `kept`); `(?d)#recycle` in. `None` when nothing changes.
pub fn clean_stignore(text: &str, present: &dyn Fn(&str) -> bool) -> Option<StignoreProposal> {
    let mut removed = Vec::new();
    let mut kept: Vec<&str> = Vec::new();
    let mut kept_foreign = Vec::new();
    let mut has_recycle = false;
    for line in text.lines() {
        let bare = bare_pattern(line);
        if line.trim() == RECYCLE_LINE {
            if has_recycle {
                removed.push(line.to_string());
                continue;
            }
            has_recycle = true;
            kept.push(line);
        } else if FOREIGN.contains(&bare) && present(bare) {
            kept_foreign.push(line.to_string());
            kept.push(line);
        } else if FOREIGN.contains(&bare) {
            removed.push(line.to_string());
        } else {
            kept.push(line);
        }
    }
    let mut added = Vec::new();
    if !has_recycle {
        added.push(RECYCLE_LINE.to_string());
    }
    if removed.is_empty() && added.is_empty() {
        return None;
    }
    let mut after: String = kept.iter().map(|l| format!("{l}\n")).collect();
    for a in &added {
        after.push_str(a);
        after.push('\n');
    }
    Some(StignoreProposal { before: text.to_string(), after, removed, added, kept: kept_foreign })
}

/// The most common owner and how many entries have another one.
pub fn owner_tally(owners: &[(u32, u32)]) -> Option<((u32, u32), u32)> {
    let mut count: BTreeMap<(u32, u32), u32> = BTreeMap::new();
    for o in owners {
        *count.entry(*o).or_default() += 1;
    }
    let (best, n) = count.iter().max_by_key(|(o, n)| (**n, std::cmp::Reverse(**o)))?;
    Some((*best, owners.len() as u32 - n))
}

/// A container mount for the adoption: host source → container destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub source: String,
    pub destination: String,
}

/// The Syncthing home among a container's mounts: `/var/syncthing` (the official image:
/// `<src>/config` when it holds `config.xml`, else `<src>`), `/config` (other images), else
/// any mount holding `config.xml` and `cert.pem`. `has` tells whether a host file exists.
pub fn home_from_mounts(mounts: &[Mount], has: &dyn Fn(&str) -> bool) -> Option<String> {
    let at = |dest: &str| mounts.iter().find(|m| m.destination.trim_end_matches('/') == dest).map(|m| m.source.trim_end_matches('/').to_string());
    if let Some(src) = at("/var/syncthing") {
        let cfg = format!("{src}/config");
        return Some(if has(&format!("{cfg}/config.xml")) { cfg } else { src });
    }
    if let Some(src) = at("/config") {
        return Some(src);
    }
    mounts.iter().map(|m| m.source.trim_end_matches('/').to_string()).find(|s| has(&format!("{s}/config.xml")) && has(&format!("{s}/cert.pem")))
}

/// The volume table of a container: every mount, destination → source.
pub fn table_from_mounts(mounts: &[Mount]) -> Vec<AdoptVolume> {
    mounts.iter().map(|m| AdoptVolume { container: m.destination.clone(), host: m.source.clone() }).collect()
}

/// The table read from the source with the user's lines on top (same container path:
/// the user's line wins).
pub fn merge_tables(base: Vec<AdoptVolume>, extra: &[AdoptVolume]) -> Vec<AdoptVolume> {
    let mut out: Vec<AdoptVolume> = base.into_iter().filter(|b| !extra.iter().any(|e| e.container.trim_end_matches('/') == b.container.trim_end_matches('/'))).collect();
    out.extend(extra.iter().cloned());
    out
}

/// Does an image or command line run Syncthing?
pub fn is_syncthing(image: &str, command: &str) -> bool {
    image.to_ascii_lowercase().contains("syncthing") || command.to_ascii_lowercase().contains("syncthing")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"<configuration version="37">
    <folder id="abcd-1234" label="Photos" path="/var/syncthing/data/photos" type="sendreceive" rescanIntervalS="3600" fsWatcherEnabled="true">
        <filesystemType>basic</filesystemType>
        <device id="AAAAAAA-BBBBBBB-CCCCCCC-DDDDDDD-EEEEEEE-FFFFFFF-GGGGGGG-HHHHHHH" introducedBy="">
            <encryptionPassword></encryptionPassword>
        </device>
        <paused>false</paused>
        <markerName>.stfolder</markerName>
    </folder>
    <folder id="docs" label="Docs &amp; notes" path='/var/syncthing/data/docs' type="sendonly">
        <device id="IIIIIII-JJJJJJJ-KKKKKKK-LLLLLLL-MMMMMMM-NNNNNNN-OOOOOOO-PPPPPPP"></device>
        <markerName>.marker</markerName>
    </folder>
    <folder id="music" label="Music" path="~/Music" type="receiveonly"><paused>true</paused></folder>
    <device id="AAAAAAA-BBBBBBB-CCCCCCC-DDDDDDD-EEEEEEE-FFFFFFF-GGGGGGG-HHHHHHH" name="laptop" compression="metadata">
        <address>dynamic</address>
    </device>
    <device id="IIIIIII-JJJJJJJ-KKKKKKK-LLLLLLL-MMMMMMM-NNNNNNN-OOOOOOO-PPPPPPP" name="phone"></device>
    <gui enabled="true" tls="false" debugging="false">
        <address>127.0.0.1:8384</address>
        <user>admin</user>
        <password>$2a$10$secretbcrypthashvalue</password>
        <apikey>SECRETAPIKEYvalue0123456789abcdef</apikey>
        <theme>default</theme>
    </gui>
</configuration>
"#;

    #[test]
    fn parses_folders_devices_and_gui_but_no_secrets() {
        let c = parse_config(FIXTURE).unwrap();
        assert_eq!(c.folders.len(), 3);
        let f = &c.folders[0];
        assert_eq!((f.id.as_str(), f.label.as_str(), f.path.as_str(), f.folder_type.as_str()), ("abcd-1234", "Photos", "/var/syncthing/data/photos", "sendreceive"));
        assert_eq!(f.devices, ["AAAAAAA-BBBBBBB-CCCCCCC-DDDDDDD-EEEEEEE-FFFFFFF-GGGGGGG-HHHHHHH"]);
        assert!(!f.paused);
        assert_eq!(f.marker_name, ".stfolder");
        assert_eq!(c.folders[1].label, "Docs & notes");
        assert_eq!(c.folders[1].marker_name, ".marker");
        assert!(c.folders[2].paused);
        assert_eq!(c.devices.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["laptop", "phone"]);
        assert_eq!(c.gui_address.as_deref(), Some("127.0.0.1:8384"));
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("SECRETAPIKEY") && !dbg.contains("secretbcrypt"));
        assert!(parse_config("<other/>").is_err());
        assert!(parse_config("<configuration>").is_err());
    }

    #[test]
    fn rewrite_changes_only_paths_and_paused() {
        let edits = [
            FolderEdit { id: "abcd-1234".into(), path: Some("/shares/photos/Phone & Co".into()), pause: false },
            FolderEdit { id: "docs".into(), path: Some("/shares/docs".into()), pause: true },
            FolderEdit { id: "music".into(), path: None, pause: true },
        ];
        let out = rewrite_config(FIXTURE, &edits).unwrap();
        let c = parse_config(&out).unwrap();
        assert_eq!(c.folders[0].path, "/shares/photos/Phone & Co");
        assert!(!c.folders[0].paused);
        assert_eq!(c.folders[1].path, "/shares/docs");
        assert!(c.folders[1].paused);
        assert!(c.folders[2].paused);
        // byte-identical outside the edits
        let expected = FIXTURE
            .replace(r#"path="/var/syncthing/data/photos""#, r#"path="/shares/photos/Phone &amp; Co""#)
            .replace(r#"path='/var/syncthing/data/docs' type="sendonly">"#, r#"path='/shares/docs' type="sendonly"><paused>true</paused>"#);
        assert_eq!(out, expected);
        // pausing a folder that has <paused>false</paused> flips the text only
        let out = rewrite_config(FIXTURE, &[FolderEdit { id: "abcd-1234".into(), path: None, pause: true }]).unwrap();
        assert_eq!(out, FIXTURE.replacen("<paused>false</paused>", "<paused>true</paused>", 1));
        assert!(rewrite_config(FIXTURE, &[FolderEdit { id: "nope".into(), path: None, pause: true }]).is_err());
        // a self-closing folder
        let x = r#"<configuration><folder id="a" path="/x"/></configuration>"#;
        let out = rewrite_config(x, &[FolderEdit { id: "a".into(), path: None, pause: true }]).unwrap();
        assert_eq!(out, r#"<configuration><folder id="a" path="/x"><paused>true</paused></folder></configuration>"#);
    }

    #[test]
    fn marker_file_follows_the_folder_id() {
        // sha256("default") = 37a8eec1…
        assert_eq!(marker_file_name("default"), "syncthing-folder-37a8ee.txt");
        assert_eq!(marker_file_name("abcd-1234").len(), "syncthing-folder-".len() + 6 + 4);
    }

    #[test]
    fn paths_map_through_the_table_and_the_shares() {
        let table = vec![
            AdoptVolume { container: "/var/syncthing".into(), host: "/volume1/docker/st".into() },
            AdoptVolume { container: "/var/syncthing/data".into(), host: "/volume1".into() },
        ];
        assert_eq!(host_path("/var/syncthing/data/photos/2024", &table, None), Ok("/volume1/photos/2024".into()));
        assert_eq!(host_path("/var/syncthing/Sync", &table, None), Ok("/volume1/docker/st/Sync".into()));
        assert_eq!(host_path("/elsewhere", &table, None), Err(UnmappedReason::NoVolume));
        assert_eq!(host_path("relative", &table, None), Err(UnmappedReason::BadPath));
        assert_eq!(host_path("/var/syncthing/../etc", &table, None), Err(UnmappedReason::BadPath));
        assert_eq!(host_path("~/Music", &[], Some("/home/ann")), Ok("/home/ann/Music".into()));
        assert_eq!(host_path("~/Music", &[], None), Err(UnmappedReason::BadPath));
        assert_eq!(host_path("/srv/data/", &[], None), Ok("/srv/data".into()));

        let shares = vec![("photos".to_string(), "/volume1/photos".to_string()), ("all".to_string(), "/volume1".to_string()), ("empty".to_string(), String::new())];
        assert_eq!(share_path("/volume1/photos/2024", &shares, "/shares"), Ok(("photos".into(), "/shares/photos/2024".into())));
        assert_eq!(share_path("/volume1/photos", &shares, "/shares"), Ok(("photos".into(), "/shares/photos".into())));
        assert_eq!(share_path("/volume1/photosX", &shares, "/shares"), Ok(("all".into(), "/shares/all/photosX".into())));
        assert_eq!(share_path("/volume2/x", &shares, "/shares"), Err(UnmappedReason::NoShare));
        let merged = merge_tables(table, &[AdoptVolume { container: "/var/syncthing/data/".into(), host: "/volume2".into() }]);
        assert_eq!(host_path("/var/syncthing/data/a", &merged, None), Ok("/volume2/a".into()));
    }

    #[test]
    fn stignore_drops_other_systems_patterns() {
        let before = "// mine\n*.tmp\n@eaDir\n(?d)/#snapshot/\n**/.SynologyWorkingDirectory\n!@Recycle\n.@__thumb/**\n.streams\n#recycle\n#include more.txt\n";
        let none = |_: &str| false;
        let p = clean_stignore(before, &none).unwrap();
        assert_eq!(p.after, "// mine\n*.tmp\n#include more.txt\n(?d)#recycle\n");
        assert_eq!(p.removed.len(), 7);
        assert_eq!(p.added, ["(?d)#recycle"]);
        assert!(p.kept.is_empty());
        assert_eq!(clean_stignore("*.tmp\n(?d)#recycle\n", &none), None);
        let p = clean_stignore("", &none).unwrap();
        assert_eq!(p.after, "(?d)#recycle\n");
        // folders of that name in the folder: their lines stay, so they stay out
        let has = |n: &str| n == "@eaDir" || n == "#snapshot";
        let p = clean_stignore(before, &has).unwrap();
        assert_eq!(p.kept, ["@eaDir", "(?d)/#snapshot/"]);
        assert_eq!(p.after, "// mine\n*.tmp\n@eaDir\n(?d)/#snapshot/\n#include more.txt\n(?d)#recycle\n");
        assert_eq!(p.removed.len(), 5);
    }

    #[test]
    fn gui_tls_and_listen_addresses() {
        let x = "<configuration>\n    <gui enabled=\"true\" tls=\"true\">\n        <address>0.0.0.0:8384</address>\n    </gui>\n    <options>\n        <listenAddress>tcp://0.0.0.0:22001</listenAddress>\n        <listenAddress>quic://0.0.0.0:22001</listenAddress>\n        <listenAddress>dynamic+https://relays.syncthing.net/endpoint</listenAddress>\n        <globalAnnounceEnabled>true</globalAnnounceEnabled>\n    </options>\n    <folder id=\"a\" path=\"/x\"/>\n</configuration>\n";
        let c = parse_config(x).unwrap();
        assert!(c.gui_tls);
        assert_eq!(custom_listen(&c.listen), ["tcp://0.0.0.0:22001", "quic://0.0.0.0:22001"]);
        let ok = ["default", "tcp://:22000", "quic://0.0.0.0:22000", "tcp://[::]:22000/?x=1", "relay://r.example:443/?id=X", "dynamic+https://relays"];
        assert!(custom_listen(&ok.map(String::from)).is_empty());
        // TLS off only
        let out = rewrite_options(x, true, false).unwrap();
        assert_eq!(out, x.replace("tls=\"true\"", "tls=\"false\""));
        // both: one default line, the rest of the file as it was
        let out = rewrite_options(x, true, true).unwrap();
        let c2 = parse_config(&out).unwrap();
        assert_eq!((c2.gui_tls, c2.listen.clone()), (false, vec!["default".to_string()]));
        assert!(out.contains("<options>\n        <listenAddress>default</listenAddress>\n        <globalAnnounceEnabled>"), "{out}");
        assert_eq!(c2.folders, c.folders);
        // nothing to change
        assert_eq!(rewrite_options(FIXTURE, true, true).unwrap(), FIXTURE);
    }

    #[test]
    fn device_id_prefix_of_a_certificate() {
        assert_eq!(base32(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32(b"f"), "MY");
        let pem = "-----BEGIN CERTIFICATE-----\nZm9v\nYmFy\n-----END CERTIFICATE-----\n";
        let (short, hex) = cert_identity(pem).unwrap();
        // sha256("foobar") = c3ab8ff1…
        assert!(hex.starts_with("c3ab8ff1"), "{hex}");
        assert_eq!(short, base32(&Sha256::digest(b"foobar")).chars().take(7).collect::<String>());
        assert_eq!(short.len(), 7);
        assert_eq!(cert_identity("no certificate"), None);
    }

    #[test]
    fn owners_and_homes() {
        assert_eq!(owner_tally(&[(1000, 100), (1000, 100), (0, 0)]), Some(((1000, 100), 1)));
        assert_eq!(owner_tally(&[]), None);

        // canned `docker inspect` of an official-image container
        let inspect: serde_json::Value = serde_json::from_str(
            r#"[{"Id":"c0ffee","Path":"/bin/entrypoint.sh","Args":["/bin/syncthing"],
                "Config":{"Image":"syncthing/syncthing:1.27","Cmd":null},
                "Mounts":[
                  {"Type":"bind","Source":"/volume1/docker/st","Destination":"/var/syncthing","Mode":"","RW":true,"Propagation":"rprivate"},
                  {"Type":"bind","Source":"/volume1/photos","Destination":"/var/syncthing/photos","Mode":"","RW":true,"Propagation":"rprivate"},
                  {"Type":"tmpfs","Destination":"/tmp"}
                ]}]"#,
        )
        .unwrap();
        let (image, cmd) = rynas_docker::model::inspect_image_and_command(&inspect);
        assert!(is_syncthing(&image, &cmd));
        assert!(!is_syncthing("nginx:1", "/docker-entrypoint.sh nginx"));
        let mounts: Vec<Mount> = rynas_docker::model::inspect_mounts(&inspect).into_iter().map(|m| Mount { source: m.source, destination: m.destination }).collect();
        assert_eq!(mounts.len(), 2);
        let has = |p: &str| p == "/volume1/docker/st/config/config.xml";
        assert_eq!(home_from_mounts(&mounts, &has).as_deref(), Some("/volume1/docker/st/config"));
        assert_eq!(home_from_mounts(&mounts, &|_| false).as_deref(), Some("/volume1/docker/st"));
        let table = table_from_mounts(&mounts);
        assert_eq!(host_path("/var/syncthing/photos/x", &table, None), Ok("/volume1/photos/x".into()));
        // another image: /config, or any mount with the home files
        let ls = vec![Mount { source: "/srv/st".into(), destination: "/config".into() }];
        assert_eq!(home_from_mounts(&ls, &|_| false).as_deref(), Some("/srv/st"));
        let other = vec![Mount { source: "/srv/home/".into(), destination: "/st".into() }];
        assert_eq!(home_from_mounts(&other, &|p| p.starts_with("/srv/home/")).as_deref(), Some("/srv/home"));
        assert_eq!(home_from_mounts(&other, &|_| false), None);
        assert_eq!(restart_policy(&inspect), "no");
        let js: serde_json::Value = serde_json::from_str(r#"{"HostConfig":{"RestartPolicy":{"Name":"always"}}}"#).unwrap();
        assert_eq!(restart_policy(&js), "always");
    }

    #[test]
    fn rewrite_refuses_duplicates_and_escapes_whitespace() {
        let x = r#"<configuration><folder id="a" path="/x"></folder><folder id="a" path="/y"></folder></configuration>"#;
        assert_eq!(duplicate_ids(&parse_config(x).unwrap()), ["a"]);
        assert!(rewrite_config(x, &[]).unwrap_err().contains("used twice"));
        let x = r#"<configuration><folder id="a" path="/x"></folder></configuration>"#;
        let out = rewrite_config(x, &[FolderEdit { id: "a".into(), path: Some("/s/tab\there\nnl\rcr".into()), pause: false }]).unwrap();
        assert!(out.contains("/s/tab&#9;here&#10;nl&#13;cr"), "{out}");
        assert_eq!(parse_config(&out).unwrap().folders[0].path, "/s/tab\there\nnl\rcr");
    }

    #[test]
    fn typed_volumes_are_checked_and_results_normalized() {
        let ok = [AdoptVolume { container: "/data".into(), host: "/volume1/x".into() }];
        assert!(check_volumes(&ok).is_ok());
        for (c, h) in [("data", "/v"), ("/data", "v"), ("/data", "/v/../etc"), ("/a/../b", "/v")] {
            assert!(check_volumes(&[AdoptVolume { container: c.into(), host: h.into() }]).is_err(), "{c} {h}");
        }
        // a host side with `..` that slipped into a table never escapes
        let bad = [AdoptVolume { container: "/data".into(), host: "/volume1/../etc".into() }];
        assert_eq!(host_path("/data/x", &bad, None), Err(UnmappedReason::BadPath));
        let dots = [AdoptVolume { container: "/data".into(), host: "/volume1/./x/".into() }];
        assert_eq!(host_path("/data/y", &dots, None), Ok("/volume1/x/y".into()));
        assert!(is_within("/a/b/c", "/a/b") && is_within("/a/b", "/a/b") && !is_within("/a/bc", "/a/b"));
    }

    #[test]
    fn process_homes_from_flags_and_environment() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(process_homes(&s(&["/usr/bin/syncthing", "serve", "--home=/srv/st/", "--no-browser"]), &[]), ["/srv/st"]);
        assert_eq!(process_homes(&s(&["syncthing", "-home", "/a", "--config", "/b"]), &s(&["PATH=/bin", "STHOMEDIR=/c"])), ["/a", "/b", "/c"]);
        assert!(process_homes(&s(&["syncthing", "serve"]), &[]).is_empty());
    }

    #[test]
    fn home_files_follow_links_only_inside_the_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join("keys")).unwrap();
        std::fs::write(home.join("keys/cert.pem"), "c").unwrap();
        std::fs::write(home.join("key.pem"), "k").unwrap();
        std::fs::write(tmp.path().join("outside.pem"), "o").unwrap();
        std::os::unix::fs::symlink("keys/cert.pem", home.join("cert.pem")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("outside.pem"), home.join("https-key.pem")).unwrap();
        std::os::unix::fs::symlink("keys", home.join("https-cert.pem")).unwrap();
        let real = std::fs::canonicalize(&home).unwrap();
        assert_eq!(home_file(&real, "key.pem"), Some(real.join("key.pem")));
        assert_eq!(home_file(&real, "cert.pem"), Some(real.join("keys/cert.pem")));
        assert_eq!(home_file(&real, "https-key.pem"), None, "a link out of the home");
        assert_eq!(home_file(&real, "https-cert.pem"), None, "a link to a folder");
        assert_eq!(home_file(&real, "missing.pem"), None);
    }
}
