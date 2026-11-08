//! A job's destination and secrets → how the worker opens the repository ([`RepoSpec`]).
//!
//! | Destination | rustic backend | Notes |
//! |---|---|---|
//! | local folder | `local` (the path) | USB disk, other volume |
//! | REST server | `rest:https://user:pw@host/path` | `rest-server`, also `--append-only` |
//! | SFTP | `opendal:sftp` + `key` file | the system `ssh`; new host keys are added (TOFU) |
//! | S3 | `opendal:s3` | never reads `~/.aws` (`disable_config_load`) |
//! | B2 | `opendal:b2` | native B2 API |
//! | rclone | `rclone:remote:path` | `rclone serve restic`, `RCLONE_CONFIG` file |
//!
//! The bandwidth limit becomes OpenDAL's `throttle` or rclone's `--bwlimit`; a local folder
//! and a REST server have none.

use std::collections::BTreeMap;

use rynas_api_types::backup::{Destination, RepoSpec};

/// The secrets of a job, as sealed in [`BackupJob::sealed`](rynas_api_types::backup::BackupJob::sealed).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct JobSecrets {
    pub repository_password: String,
    /// REST password, S3 secret key or B2 application key.
    pub password: String,
    pub rclone_config: String,
    /// OpenSSH private key of an SFTP destination.
    pub ssh_private_key: String,
    /// The SFTP server's pinned host key (`known_hosts` lines): from the destination test,
    /// or first use. Not a secret, sealed with the rest so that it changes only with them.
    pub ssh_known_hosts: String,
}

impl std::fmt::Debug for JobSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JobSecrets(***)")
    }
}

impl JobSecrets {
    pub fn to_map(&self) -> BTreeMap<String, String> {
        [
            ("repository_password", &self.repository_password),
            ("password", &self.password),
            ("rclone_config", &self.rclone_config),
            ("ssh_private_key", &self.ssh_private_key),
            ("ssh_known_hosts", &self.ssh_known_hosts),
        ]
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
    }

    pub fn from_map(m: &BTreeMap<String, String>) -> Self {
        let g = |k: &str| m.get(k).cloned().unwrap_or_default();
        JobSecrets {
            repository_password: g("repository_password"),
            password: g("password"),
            rclone_config: g("rclone_config"),
            ssh_private_key: g("ssh_private_key"),
            ssh_known_hosts: g("ssh_known_hosts"),
        }
    }

    /// Every secret value (to hide from logs), longest first; the host key is not one.
    pub fn values(&self) -> Vec<String> {
        let mut v: Vec<String> = [
            &self.repository_password,
            &self.password,
            &self.rclone_config,
            &self.ssh_private_key,
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.len()));
        v
    }

    /// A secret file the worker reports changed ([`WorkerEvent::SecretChanged`]): the
    /// field it updates, if any.
    ///
    /// [`WorkerEvent::SecretChanged`]: rynas_api_types::backup::WorkerEvent::SecretChanged
    pub fn field_of_file(name: &str) -> Option<&'static str> {
        match name {
            "RCLONE_CONFIG" => Some("rclone_config"),
            "known_hosts" => Some("ssh_known_hosts"),
            _ => None,
        }
    }
}

/// Percent-encoding of a URL user or password.
fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A random password: 32 characters of `[A-Za-z0-9]` (~190 bits) from the kernel.
pub fn generate_password() -> std::io::Result<String> {
    const SET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut out = String::with_capacity(32);
    let mut buf = [0u8; 64];
    while out.len() < 32 {
        // SAFETY: getrandom fills the buffer it is given
        let n = unsafe { libc::getrandom(buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        for &b in &buf[..n as usize] {
            // rejection sampling: no modulo bias
            let lim = 256 - (256 % SET.len());
            if (b as usize) < lim && out.len() < 32 {
                out.push(SET[b as usize % SET.len()] as char);
            }
        }
    }
    Ok(out)
}

/// The repository a destination names, normalized: two jobs with the same key share one
/// repository and never run a worker operation at the same time (the maintenance lock).
pub fn repo_key(d: &Destination) -> String {
    let low = |s: &str| s.to_ascii_lowercase();
    match d {
        Destination::Local { path } => format!("local:{}", path.trim_end_matches('/')),
        Destination::Rest { url, .. } => {
            let (scheme, rest) = url.split_once("://").unwrap_or(("https", url));
            let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
            format!("rest:{}://{}/{}", low(scheme), low(host), path.trim_matches('/'))
        }
        Destination::Sftp { host, port, path, .. } => {
            format!("sftp:{}:{port}/{}", low(host), path.trim_end_matches('/'))
        }
        Destination::S3 {
            endpoint,
            bucket,
            prefix,
            ..
        } => format!(
            "s3:{}/{bucket}/{}",
            low(endpoint.trim_end_matches('/')),
            prefix.trim_matches('/')
        ),
        Destination::B2 { bucket, prefix, .. } => {
            format!("b2:{bucket}/{}", prefix.trim_matches('/'))
        }
        Destination::Rclone { remote } => format!("rclone:{}", remote.trim_end_matches('/')),
    }
}

/// The worker's view of a destination.
pub fn repo_spec(d: &Destination, s: &JobSecrets, bandwidth_kib: u32) -> RepoSpec {
    let mut options = BTreeMap::new();
    let mut files = BTreeMap::new();
    let throttle = |o: &mut BTreeMap<String, String>| {
        if bandwidth_kib > 0 {
            // bandwidth, then a burst of 4 s
            o.insert(
                "throttle".into(),
                format!("{}KiB,{}KiB", bandwidth_kib, bandwidth_kib.saturating_mul(4)),
            );
        }
    };
    let repository = match d {
        Destination::Local { path } => path.clone(),
        Destination::Rest { url, user } => {
            let (scheme, rest) = url.split_once("://").unwrap_or(("https", url));
            if user.is_empty() {
                format!("rest:{scheme}://{rest}/")
            } else {
                format!(
                    "rest:{scheme}://{}:{}@{rest}/",
                    encode(user),
                    encode(&s.password)
                )
            }
        }
        Destination::Sftp {
            host,
            port,
            user,
            path,
        } => {
            options.insert("endpoint".into(), format!("ssh://{host}:{port}"));
            options.insert("user".into(), user.clone());
            options.insert("root".into(), path.clone());
            // the worker's ssh reads only this file (`-F none`, see the worker): pinned
            // keys are checked strictly, an empty one is filled on first use
            let pinned = !s.ssh_known_hosts.trim().is_empty();
            options.insert(
                "known_hosts_strategy".into(),
                if pinned { "strict" } else { "add" }.into(),
            );
            files.insert("known_hosts".into(), s.ssh_known_hosts.clone());
            files.insert("key".into(), s.ssh_private_key.clone());
            throttle(&mut options);
            "opendal:sftp".into()
        }
        Destination::S3 {
            endpoint,
            region,
            bucket,
            prefix,
            access_key_id,
        } => {
            if !endpoint.is_empty() {
                options.insert("endpoint".into(), endpoint.clone());
            }
            let region = if region.is_empty() { "us-east-1" } else { region };
            options.insert("region".into(), region.into());
            options.insert("bucket".into(), bucket.clone());
            options.insert("root".into(), format!("/{prefix}"));
            options.insert("access_key_id".into(), access_key_id.clone());
            options.insert("secret_access_key".into(), s.password.clone());
            options.insert("disable_config_load".into(), "true".into());
            throttle(&mut options);
            "opendal:s3".into()
        }
        Destination::B2 {
            bucket,
            bucket_id,
            prefix,
            key_id,
        } => {
            options.insert("bucket".into(), bucket.clone());
            options.insert("bucket_id".into(), bucket_id.clone());
            options.insert("root".into(), format!("/{prefix}"));
            options.insert("application_key_id".into(), key_id.clone());
            options.insert("application_key".into(), s.password.clone());
            throttle(&mut options);
            "opendal:b2".into()
        }
        Destination::Rclone { remote } => {
            if !s.rclone_config.is_empty() {
                files.insert("RCLONE_CONFIG".into(), s.rclone_config.clone());
            }
            if bandwidth_kib > 0 {
                options.insert(
                    "rclone-command".into(),
                    format!("rclone serve restic --addr localhost:0 --bwlimit {bandwidth_kib}k"),
                );
            }
            format!("rclone:{remote}")
        }
    };
    RepoSpec {
        repository,
        options,
        password: s.repository_password.clone(),
        files,
        env: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sec() -> JobSecrets {
        JobSecrets {
            repository_password: "repo-pw".into(),
            password: "p@ss:w/rd".into(),
            rclone_config: "[gdrive]\ntype = drive\n".into(),
            ssh_private_key: "-----BEGIN OPENSSH PRIVATE KEY-----".into(),
            ssh_known_hosts: String::new(),
        }
    }

    #[test]
    fn specs() {
        let s = sec();
        let r = repo_spec(
            &Destination::Rest {
                url: "https://nas2:8000/docs".into(),
                user: "nas 1".into(),
            },
            &s,
            100,
        );
        assert_eq!(r.repository, "rest:https://nas%201:p%40ss%3Aw%2Frd@nas2:8000/docs/");
        assert_eq!(r.password, "repo-pw");
        assert!(r.options.is_empty());
        let r = repo_spec(
            &Destination::S3 {
                endpoint: String::new(),
                region: String::new(),
                bucket: "b".into(),
                prefix: "nas1".into(),
                access_key_id: "AK".into(),
            },
            &s,
            512,
        );
        assert_eq!(r.repository, "opendal:s3");
        assert_eq!(r.options["root"], "/nas1");
        assert_eq!(r.options["region"], "us-east-1");
        assert_eq!(r.options["throttle"], "512KiB,2048KiB");
        assert_eq!(r.options["disable_config_load"], "true");
        assert!(!r.options.contains_key("endpoint"));
        let r = repo_spec(
            &Destination::Sftp {
                host: "nas2".into(),
                port: 2222,
                user: "bk".into(),
                path: "/volume1/bk".into(),
            },
            &s,
            0,
        );
        assert_eq!(r.options["endpoint"], "ssh://nas2:2222");
        assert!(r.files["key"].starts_with("-----BEGIN"));
        assert!(!r.options.contains_key("throttle"));
        let r = repo_spec(
            &Destination::Rclone {
                remote: "gdrive:nas".into(),
            },
            &s,
            256,
        );
        assert_eq!(r.repository, "rclone:gdrive:nas");
        assert!(r.files.contains_key("RCLONE_CONFIG"));
        assert!(r.options["rclone-command"].ends_with("--bwlimit 256k"));
        let r = repo_spec(&Destination::Local { path: "/media/usb/b".into() }, &s, 0);
        assert_eq!(r.repository, "/media/usb/b");
        // SFTP host key: pinned → strict, none → added on first use
        let sftp = Destination::Sftp {
            host: "nas2".into(),
            port: 22,
            user: "bk".into(),
            path: "/b".into(),
        };
        let mut pinned = s.clone();
        pinned.ssh_known_hosts = "nas2 ssh-ed25519 AAAA".into();
        let r = repo_spec(&sftp, &pinned, 0);
        assert_eq!(r.options["known_hosts_strategy"], "strict");
        assert_eq!(r.files["known_hosts"], "nas2 ssh-ed25519 AAAA");
        assert!(!r.secret_values().iter().any(|v| v.contains("AAAA")));
        let mut unpinned = s.clone();
        unpinned.ssh_known_hosts = " ".into();
        assert_eq!(repo_spec(&sftp, &unpinned, 0).options["known_hosts_strategy"], "add");
    }

    #[test]
    fn repository_keys() {
        let rest = |url: &str, user: &str| Destination::Rest {
            url: url.into(),
            user: user.into(),
        };
        // the same repository through another user or spelling: one key
        assert_eq!(
            repo_key(&rest("https://NAS2:8000/docs/", "a")),
            repo_key(&rest("https://nas2:8000/docs", "b"))
        );
        assert_ne!(
            repo_key(&rest("https://nas2:8000/docs", "")),
            repo_key(&rest("https://nas2:8000/photos", ""))
        );
        let s3 = |bucket: &str, prefix: &str, ak: &str| Destination::S3 {
            endpoint: String::new(),
            region: String::new(),
            bucket: bucket.into(),
            prefix: prefix.into(),
            access_key_id: ak.into(),
        };
        assert_eq!(repo_key(&s3("b", "nas1", "AK1")), repo_key(&s3("b", "/nas1/", "AK2")));
        assert_ne!(repo_key(&s3("b", "nas1", "AK")), repo_key(&s3("b", "nas2", "AK")));
        assert_eq!(
            repo_key(&Destination::Local { path: "/media/usb/b".into() }),
            "local:/media/usb/b"
        );
    }

    #[test]
    fn secret_values_and_redaction() {
        use rynas_api_types::backup::redact_secrets;
        let s = sec();
        let r = repo_spec(
            &Destination::Rest {
                url: "https://nas2/r".into(),
                user: "u".into(),
            },
            &s,
            0,
        );
        let v = r.secret_values();
        assert!(v.contains(&"repo-pw".to_string()));
        assert!(v.contains(&"p%40ss%3Aw%2Frd".to_string()));
        // short secrets too (no length floor), empty ones never
        let short = vec!["ab".to_string(), String::new()];
        assert_eq!(redact_secrets("xaby", &short), "x***y");
    }

    #[test]
    fn secrets_roundtrip_and_password() {
        let s = sec();
        assert_eq!(JobSecrets::from_map(&s.to_map()), s);
        assert_eq!(format!("{s:?}"), "JobSecrets(***)");
        let a = generate_password().unwrap();
        let b = generate_password().unwrap();
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
        assert!(a.bytes().all(|c| c.is_ascii_alphanumeric()));
    }
}
