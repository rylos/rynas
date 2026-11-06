//! Managed files: final content with the anti-drift header or block markers, hashes, and
//! atomic writes.
//!
//! - [`FileKind::Whole`]: rynas owns the whole file. With `header`, the first line is
//!   `# Managed by rynas — do not edit (sha256:<hash>)`; files whose format allows no comment
//!   (or that other tools rewrite, like `/etc/hostname`) go without it.
//! - [`FileKind::Block`]: rynas owns a block between `# BEGIN rynas <id> …` and
//!   `# END rynas <id>` inside a file that also has other content (e.g. `/etc/hosts`).
//!
//! The stored hash is the SHA-256 of what rynas generated (the content without header or
//! markers), so a drift check needs no copy of the file.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const HEADER_PREFIX: &str = "# Managed by rynas — do not edit";
/// Suffix of candidate files written next to their target before the atomic rename.
pub const CANDIDATE_SUFFIX: &str = ".rynas-new";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum FileKind {
    Whole {
        header: bool,
    },
    Block {
        id: String,
        /// Lines outside the block whose first word is one of these are removed (they would
        /// contradict the block, e.g. another `127.0.1.1` line in `/etc/hosts`). An entry
        /// ending in `=` matches a shell assignment (`SAFEMODE_AFTER=` drops
        /// `SAFEMODE_AFTER=600`). An entry with a space matches the start of a line
        /// (`MONITOR ups@` drops `MONITOR ups@localhost 1 …`); an entry `[name]` drops that
        /// whole ini section, from its header to the next section, block or end of file
        /// (`[ups]` of NUT's `ups.conf`).
        #[serde(default)]
        drop_outside: Vec<String>,
    },
    /// The file must not exist (e.g. an Avahi service file of a disabled service): an apply
    /// removes it (keeping a backup for the rollback). Not tracked by the drift check.
    Absent,
}

/// A file a domain wants on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedFile {
    /// Absolute path on the target system (prefixed with the configured root when written).
    pub path: PathBuf,
    /// Generated content; a final newline is added when missing.
    pub content: String,
    pub mode: u32,
    pub kind: FileKind,
    /// Group owner of the written file (owner root), e.g. `nut` for NUT's files that its
    /// daemons read after dropping privileges; `None`: the writer's group. Skipped when the
    /// group does not exist below the configuration root.
    pub group: Option<String>,
}

impl ManagedFile {
    pub fn whole(path: impl Into<PathBuf>, content: String, header: bool) -> Self {
        Self {
            path: path.into(),
            content,
            mode: 0o644,
            kind: FileKind::Whole { header },
            group: None,
        }
    }

    pub fn block(path: impl Into<PathBuf>, id: &str, content: String) -> Self {
        Self {
            path: path.into(),
            content,
            mode: 0o644,
            kind: FileKind::Block {
                id: id.into(),
                drop_outside: Vec::new(),
            },
            group: None,
        }
    }

    /// A file that must not exist.
    pub fn absent(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            content: String::new(),
            mode: 0o644,
            kind: FileKind::Absent,
            group: None,
        }
    }

    pub fn mode(mut self, mode: u32) -> Self {
        self.mode = mode;
        self
    }

    /// Group owner of the file (see [`ManagedFile::group`]).
    pub fn group(mut self, group: &str) -> Self {
        self.group = Some(group.into());
        self
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    let d = Sha256::digest(data);
    let mut s = String::with_capacity(64);
    for b in d {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn with_newline(s: &str) -> String {
    if s.is_empty() || s.ends_with('\n') {
        s.to_string()
    } else {
        format!("{s}\n")
    }
}

fn begin_marker(id: &str) -> String {
    format!("# BEGIN rynas {id} ")
}

fn end_marker(id: &str) -> String {
    format!("# END rynas {id}")
}

/// Final file content and the hash to store. `existing` is the current file (blocks keep
/// the rest of it).
pub fn render_final(kind: &FileKind, content: &str, existing: Option<&str>) -> (String, String) {
    let body = with_newline(content);
    let hash = sha256_hex(body.as_bytes());
    let text = match kind {
        FileKind::Whole { header: true } => format!("{HEADER_PREFIX} (sha256:{hash})\n{body}"),
        FileKind::Whole { header: false } => body,
        FileKind::Absent => String::new(),
        FileKind::Block { id, drop_outside } => {
            let block = format!(
                "{}— managed by rynas, do not edit (sha256:{hash})\n{body}{}\n",
                begin_marker(id),
                end_marker(id)
            );
            let mut out = String::new();
            let mut placed = false;
            let mut inside = false;
            // inside an ini section listed as `[name]` in `drop_outside`
            let mut dropping = false;
            for line in existing.unwrap_or("").lines() {
                if inside {
                    if line.trim_end() == end_marker(id) {
                        inside = false;
                    }
                    continue;
                }
                let t = line.trim();
                if t.starts_with('[') || line.starts_with("# BEGIN rynas ") {
                    dropping = t.starts_with('[') && drop_outside.iter().any(|d| d == t);
                }
                if line.starts_with(&begin_marker(id)) {
                    inside = true;
                    if !placed {
                        out.push_str(&block);
                        placed = true;
                    }
                    continue;
                }
                if dropping {
                    continue;
                }
                let first = line.split_whitespace().next().unwrap_or("");
                let drops = |d: &String| {
                    if d.contains(' ') {
                        t.starts_with(d.as_str())
                    } else {
                        d == first || (d.ends_with('=') && first.starts_with(d.as_str()))
                    }
                };
                if !first.starts_with('#') && drop_outside.iter().any(drops) {
                    continue;
                }
                out.push_str(line);
                out.push('\n');
            }
            if !placed {
                out.push_str(&block);
            }
            out
        }
    };
    (text, hash)
}

/// Hash of the generated part of a file as found on disk; `None` when the header or the
/// block markers are gone.
pub fn current_hash(kind: &FileKind, text: &str) -> Option<String> {
    match kind {
        FileKind::Whole { header: true } => {
            let (first, rest) = text.split_once('\n')?;
            first
                .starts_with(HEADER_PREFIX)
                .then(|| sha256_hex(rest.as_bytes()))
        }
        FileKind::Whole { header: false } => Some(sha256_hex(text.as_bytes())),
        FileKind::Absent => None,
        FileKind::Block { id, .. } => {
            let mut body = String::new();
            let mut inside = false;
            for line in text.lines() {
                if inside {
                    if line.trim_end() == end_marker(id) {
                        return Some(sha256_hex(body.as_bytes()));
                    }
                    body.push_str(line);
                    body.push('\n');
                } else if line.starts_with(&begin_marker(id)) {
                    inside = true;
                }
            }
            None
        }
    }
}

/// `root` + absolute `path`.
pub fn under_root(root: &Path, path: &Path) -> PathBuf {
    root.join(path.strip_prefix("/").unwrap_or(path))
}

pub fn candidate_path(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(CANDIDATE_SUFFIX);
    PathBuf::from(s)
}

/// Writes and fsyncs `data` to `path` with `mode` (replacing it).
pub fn write_synced(path: &Path, data: &[u8], mode: u32) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(path);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    f.write_all(data)?;
    // the umask may have cut the mode
    f.set_permissions(std::fs::Permissions::from_mode(mode))?;
    f.sync_all()
}

/// Atomic replace: rename + fsync of the directory.
pub fn rename_synced(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)?;
    if let Some(dir) = to.parent()
        && let Ok(d) = std::fs::File::open(dir)
    {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_file_with_header() {
        let k = FileKind::Whole { header: true };
        let (text, hash) = render_final(&k, "a = 1", None);
        assert!(text.starts_with("# Managed by rynas — do not edit (sha256:"));
        assert!(text.ends_with(")\na = 1\n"));
        assert_eq!(current_hash(&k, &text).as_deref(), Some(hash.as_str()));
        assert_ne!(current_hash(&k, &format!("{text}b = 2\n")), Some(hash));
        assert_eq!(current_hash(&k, "a = 1\n"), None, "header removed");
    }

    #[test]
    fn block_replaces_itself_and_conflicting_lines() {
        let k = FileKind::Block {
            id: "hostname".into(),
            drop_outside: vec!["127.0.1.1".into()],
        };
        let hosts =
            "127.0.0.1\tlocalhost\n127.0.1.1\told\n# 127.0.1.1 comment kept\n::1 ip6-localhost\n";
        let (t1, h1) = render_final(&k, "127.0.1.1\tnas\n", Some(hosts));
        assert!(!t1.contains("\told\n"));
        assert!(t1.contains("# 127.0.1.1 comment kept\n"));
        assert!(t1.starts_with("127.0.0.1\tlocalhost\n"));
        assert_eq!(current_hash(&k, &t1), Some(h1));
        // applying again replaces the block in place
        let (t2, h2) = render_final(&k, "127.0.1.1\tnas2\n", Some(&t1));
        assert_eq!(t2.matches("# BEGIN rynas hostname").count(), 1);
        assert!(t2.contains("127.0.1.1\tnas2\n") && !t2.contains("\tnas\n"));
        assert_eq!(current_hash(&k, &t2), Some(h2.clone()));
        // edits outside the block are not drift, inside they are
        let outside = format!("{t2}10.0.0.1 other\n");
        assert_eq!(current_hash(&k, &outside), Some(h2.clone()));
        let inside = t2.replace("nas2", "hacked");
        assert_ne!(current_hash(&k, &inside), Some(h2));
        assert_eq!(current_hash(&k, "127.0.0.1 localhost\n"), None);
    }

    #[test]
    fn atomic_write_helpers() {
        let dir = tempfile::tempdir().unwrap();
        let target = under_root(dir.path(), Path::new("/etc/x.conf"));
        assert_eq!(target, dir.path().join("etc/x.conf"));
        let cand = candidate_path(&target);
        assert!(cand.to_string_lossy().ends_with("x.conf.rynas-new"));
        write_synced(&cand, b"hi", 0o640).unwrap();
        rename_synced(&cand, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"hi");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }

    #[test]
    fn block_drops_ini_sections_and_line_prefixes() {
        let k = FileKind::Block {
            id: "ups-device".into(),
            drop_outside: vec!["[ups]".into(), "MONITOR ups@".into()],
        };
        let conf = "maxretry = 3\n[ups]\n\tdriver = usbhid-ups\n\tport = auto\n\n[other]\n\tdriver = snmp-ups\n\
            MONITOR ups@localhost 1 u p primary\nMONITOR other@localhost 1 u p primary\n# MONITOR ups@x kept\n";
        let (t, _) = render_final(&k, "[ups]\n\tdriver = nutdrv_qx\n", Some(conf));
        assert!(t.starts_with("maxretry = 3\n[other]\n"), "{t}");
        assert!(!t.contains("usbhid-ups") && t.contains("snmp-ups"), "{t}");
        assert!(!t.contains("MONITOR ups@localhost") && t.contains("MONITOR other@"), "{t}");
        assert!(t.contains("# MONITOR ups@x kept"), "{t}");
        assert_eq!(t.matches("[ups]").count(), 1, "{t}");
        // the block itself is not a dropped section, and a second apply keeps it
        let (t2, _) = render_final(&k, "[ups]\n\tdriver = nutdrv_qx\n", Some(&t));
        assert_eq!(t2, t);
    }
}
