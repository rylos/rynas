//! What the daemon, the worker and the CGI say to each other: JSON, one
//! message per line.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Copy,
    Move,
    Delete,
    /// into a zip archive
    Compress,
    /// an archive into a folder
    Extract,
    /// after a restart: the temporary files of an interrupted job go
    Cleanup,
}

/// What to do when the destination already has an item of that name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    #[default]
    Ask,
    Overwrite,
    Skip,
    /// keep both: the new one gets " (1)", " (2)"...
    Rename,
    /// overwrite only when the source is newer
    Newer,
}

/// A job as the worker receives it, real paths only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spec {
    pub op: Op,
    pub sources: Vec<PathBuf>,
    #[serde(default)]
    pub dest: Option<PathBuf>,
    #[serde(default)]
    pub conflict: Policy,
    #[serde(default)]
    pub verify: bool,
    /// bytes per second, 0 for no limit
    #[serde(default)]
    pub limit: u64,
    /// delete: the shares whose recycle bin takes the items. When it is not
    /// empty the bin was asked for, and an item none of them takes is an
    /// error, never a deletion for good
    #[serde(default)]
    pub recycle: Vec<RecycleRoot>,
    /// compress: the archive's file name
    #[serde(default)]
    pub name: Option<String>,
    /// compress: store, fast or normal (zip); fast, normal or best (tar.zst)
    #[serde(default)]
    pub level: Option<String>,
    /// compress: "zip" (the default) or "tar.zst"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// extract: into a new folder named after the archive
    #[serde(default)]
    pub into_folder: bool,
    /// extract: the archive's password
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// extract: only these items (paths inside the archive), each into dest
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
    /// the remote connections the paths name (`/:<id>/…`), secrets included
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remotes: Vec<crate::conns::Conn>,
    /// the shared folders the local paths lie in: the worker resolves each path inside its
    /// own share, links included, and refuses one that leads out (`crate::confine`). A local
    /// path no root holds is refused
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<PathBuf>,
    /// the apps' folders an administrator's job writes in (`@apps`): rynasd starts its worker
    /// with the powers to change them and a Landlock ruleset that allows writes below these
    /// folders only (`manager::worker_command`); empty for every other job
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub app_roots: Vec<PathBuf>,
}

impl Spec {
    /// The spec as the journal keeps it on disk: no archive password and no
    /// secrets of the connections, which come back from the connection
    /// store (`remote::with_secrets`) when the job is run again.
    pub fn without_secrets(&self) -> Spec {
        let mut spec = self.clone();
        spec.password = None;
        for c in &mut spec.remotes {
            c.password.clear();
            c.ssh_key.clear();
        }
        spec
    }
}

/// The message of a job that failed for want of the archive's password.
pub const NEED_PASSWORD: &str = "need_password";

/// A share with its recycle bin on: its name and its folder; deleted items go to
/// `<folder>/#recycle/<path inside the share>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecycleRoot {
    pub name: String,
    pub path: PathBuf,
}

/// Worker to daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Event {
    Progress(Progress),
    Conflict(ConflictInfo),
    /// `code`: an `error_codes` constant for the interface to translate; `msg` the English
    /// detail.
    Error {
        path: String,
        msg: String,
        #[serde(default)]
        code: String,
    },
    /// `code`: empty when the job went well, else why it stopped (`password_required`…).
    Done {
        status: Status,
        msg: String,
        #[serde(default)]
        code: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Progress {
    /// scan, copy, verify, delete, cleanup
    pub phase: String,
    pub paused: bool,
    pub bytes_total: u64,
    pub bytes_done: u64,
    pub files_total: u64,
    pub files_done: u64,
    pub dirs: u64,
    pub skipped: u64,
    pub errors: u64,
    /// the item in flight, relative to the source folder
    pub current: String,
    pub current_size: u64,
    pub current_done: u64,
    /// clone (reflink), copy, rename, recycle, delete
    pub method: String,
    /// files moved by renaming or cloned: done without moving data
    pub instant: u64,
    pub scanning: bool,
    /// bytes read back by the verification
    pub verify_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Side {
    pub size: u64,
    pub mtime: i64,
    pub dir: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictInfo {
    pub id: u64,
    pub name: String,
    pub path: String,
    pub source: Side,
    pub target: Side,
    /// the name "keep both" would use
    pub rename_to: String,
}

/// Daemon to worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "c", rename_all = "lowercase")]
pub enum Command {
    Pause,
    Resume,
    Cancel,
    Limit { bps: u64 },
    Answer { id: u64, action: Policy, all: bool },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_spec_keeps_no_secrets() {
        let conn = crate::conns::Conn {
            id: "ab12".into(),
            kind: crate::conns::Kind::Sftp,
            host: "server".into(),
            user: "marco".into(),
            host_key: "ssh-ed25519 AAAA".into(),
            password: "conn-pässword".into(),
            ssh_key: "-----BEGIN OPENSSH PRIVATE KEY-----secret".into(),
            ..Default::default()
        };
        let spec = Spec {
            op: Op::Extract,
            sources: vec![PathBuf::from("/srv/vol1/a.rar")],
            dest: Some(PathBuf::from("/:ab12/x")),
            conflict: Policy::Ask,
            verify: false,
            limit: 0,
            recycle: Vec::new(),
            name: None,
            level: None,
            format: None,
            into_folder: false,
            password: Some("archive-secret".into()),
            members: Vec::new(),
            remotes: vec![conn],
            roots: Vec::new(),
            app_roots: Vec::new(),
        };
        let clean = spec.without_secrets();
        let text = serde_json::to_string(&clean).unwrap();
        for secret in ["conn-pässword", "secret", "PRIVATE KEY", "archive-secret"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        // what is not secret stays, so the job can run again
        assert_eq!(clean.remotes[0].host_key, "ssh-ed25519 AAAA");
        assert_eq!(clean.remotes[0].id, "ab12");
        assert_eq!(clean.sources, spec.sources);
        // the original is untouched
        assert_eq!(spec.password.as_deref(), Some("archive-secret"));
    }
}
