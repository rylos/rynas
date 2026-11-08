//! The `backup` configuration key: the jobs, checked and normalized. Secrets are stored
//! sealed in each job ([`BackupJob::sealed`], rynasd's age key); the key renders no file.

use rynas_api_types::backup::{BACKUP_KEY, BackupConfig, BackupJob, BackupSource, Destination};
use rynas_config::{Applier, ApplyCtx, ManagedFile};
use serde_json::Value;

/// `[a-z0-9][a-z0-9-]{0,31}`.
pub fn valid_job_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// An absolute path without `..`, control characters or a trailing slash (but `/`).
pub fn clean_abs(p: &str) -> bool {
    p.starts_with('/')
        && p.len() <= 1024
        && !p.chars().any(char::is_control)
        && !p.split('/').any(|c| c == ".." || c == ".")
        && (p == "/" || !p.ends_with('/'))
        && !p.contains("//")
}

fn plain(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && !s.chars().any(|c| c.is_control() || c.is_whitespace())
}

fn check_destination(d: &mut Destination) -> Result<(), String> {
    match d {
        Destination::Local { path } => {
            *path = path.trim().trim_end_matches('/').to_string();
            if !clean_abs(path) || path.is_empty() || path.matches('/').count() < 2 {
                return Err(format!(
                    "local destination: {path:?} must be a folder below a mount point, e.g. /media/usb/backup"
                ));
            }
        }
        Destination::Rest { url, user } => {
            *url = url.trim().trim_end_matches('/').to_string();
            *user = user.trim().to_string();
            let ok = (url.starts_with("http://") || url.starts_with("https://"))
                && plain(url, 512)
                && url.split_once("://").is_some_and(|(_, r)| !r.is_empty());
            if !ok || url.contains('@') {
                return Err("REST server: an http:// or https:// address, without user or password in it".into());
            }
            if user.contains(':') || user.chars().any(char::is_control) || user.len() > 64 {
                return Err("REST server: invalid user".into());
            }
        }
        Destination::Sftp {
            host,
            port,
            user,
            path,
        } => {
            *host = host.trim().to_string();
            *user = user.trim().to_string();
            *path = path.trim().trim_end_matches('/').to_string();
            if !plain(host, 253) || host.contains(['@', '/']) || host.starts_with('-') {
                return Err("SFTP: invalid server name".into());
            }
            if *port == 0 {
                return Err("SFTP: invalid port".into());
            }
            if !plain(user, 64) || user.contains(['@', ':', '/']) || user.starts_with('-') {
                return Err("SFTP: invalid user".into());
            }
            if path.is_empty() || path.len() > 1024 || path.chars().any(char::is_control) {
                return Err("SFTP: invalid folder".into());
            }
        }
        Destination::S3 {
            endpoint,
            region,
            bucket,
            prefix,
            access_key_id,
        } => {
            *endpoint = endpoint.trim().trim_end_matches('/').to_string();
            *region = region.trim().to_string();
            *bucket = bucket.trim().to_string();
            *prefix = prefix.trim().trim_matches('/').to_string();
            *access_key_id = access_key_id.trim().to_string();
            if !endpoint.is_empty()
                && !(endpoint.starts_with("https://") || endpoint.starts_with("http://"))
            {
                return Err("S3: the endpoint is an http:// or https:// address".into());
            }
            if !plain(bucket, 255) || !plain(access_key_id, 256) {
                return Err("S3: bucket and access key are required".into());
            }
            if !region.is_empty() && !plain(region, 64) {
                return Err("S3: invalid region".into());
            }
            if prefix.len() > 512 || prefix.chars().any(char::is_control) {
                return Err("S3: invalid folder".into());
            }
        }
        Destination::B2 {
            bucket,
            bucket_id,
            prefix,
            key_id,
        } => {
            *bucket = bucket.trim().to_string();
            *bucket_id = bucket_id.trim().to_string();
            *prefix = prefix.trim().trim_matches('/').to_string();
            *key_id = key_id.trim().to_string();
            if !plain(bucket, 64) || !plain(bucket_id, 64) || !plain(key_id, 128) {
                return Err("B2: bucket, bucket id and key id are required".into());
            }
            if prefix.len() > 512 || prefix.chars().any(char::is_control) {
                return Err("B2: invalid folder".into());
            }
        }
        Destination::Rclone { remote } => {
            *remote = remote.trim().to_string();
            let ok = remote.len() <= 512
                && !remote.starts_with('-')
                && !remote.chars().any(char::is_control)
                && remote
                    .split_once(':')
                    .is_some_and(|(n, _)| !n.is_empty() && !n.contains(char::is_whitespace));
            if !ok {
                return Err("rclone: a remote as name:path".into());
            }
        }
    }
    Ok(())
}

/// Checks and normalizes one job.
pub fn normalize_job(mut j: BackupJob) -> Result<BackupJob, String> {
    j.name = j.name.trim().to_string();
    if !valid_job_name(&j.name) {
        return Err(format!(
            "invalid job name {:?}: lowercase letters, digits and -, up to 32",
            j.name
        ));
    }
    let n = j.name.clone();
    j.description = j.description.trim().chars().take(200).collect();
    if j.sources.is_empty() || j.sources.len() > 32 {
        return Err(format!("job {n}: 1 to 32 sources"));
    }
    for s in &mut j.sources {
        match s {
            BackupSource::Share { name } => {
                *name = name.trim().to_string();
                if name.is_empty() || name.len() > 64 || name.contains('/') {
                    return Err(format!("job {n}: invalid share {name:?}"));
                }
            }
            BackupSource::Folder { path } => {
                *path = path.trim().trim_end_matches('/').to_string();
                if !clean_abs(path) || path.is_empty() {
                    return Err(format!("job {n}: invalid folder {path:?}"));
                }
            }
            BackupSource::Config => {}
        }
    }
    let mut seen = Vec::new();
    for s in &j.sources {
        if seen.contains(&s) {
            return Err(format!("job {n}: a source is listed twice"));
        }
        seen.push(s);
    }
    check_destination(&mut j.destination).map_err(|e| format!("job {n}: {e}"))?;
    if j.retention.is_empty() {
        return Err(format!("job {n}: keep at least one kind of snapshot"));
    }
    let r = &j.retention;
    if [r.last, r.hourly, r.daily, r.weekly, r.monthly, r.yearly]
        .iter()
        .any(|v| *v > 10_000)
    {
        return Err(format!("job {n}: at most 10000 per retention rule"));
    }
    let m = &j.maintenance;
    if !(1..=365).contains(&m.prune_every_days) || m.check_every_days > 365 {
        return Err(format!(
            "job {n}: prune every 1 to 365 days, check every 0 to 365 days"
        ));
    }
    if m.check_read_percent > 100 {
        return Err(format!("job {n}: read 0 to 100 % of the data"));
    }
    j.excludes = j
        .excludes
        .iter()
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty())
        .collect();
    if j.excludes.len() > 64
        || j.excludes
            .iter()
            .any(|e| e.len() > 256 || e.chars().any(char::is_control) || e.starts_with('!'))
    {
        return Err(format!(
            "job {n}: up to 64 exclude patterns of up to 256 characters"
        ));
    }
    j.stop_stacks.retain(|s| !s.trim().is_empty());
    if j.stop_stacks.iter().any(|s| {
        s.len() > 64
            || !s
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    }) {
        return Err(format!("job {n}: invalid stack name"));
    }
    if j.bandwidth_kib > 10_000_000 {
        return Err(format!("job {n}: invalid bandwidth limit"));
    }
    if !matches!(j.destination, Destination::Sftp { .. }) {
        j.ssh_public_key.clear();
    }
    if j.ssh_public_key.len() > 1024 || j.ssh_public_key.contains('\n') {
        return Err(format!("job {n}: invalid public key"));
    }
    if j.sealed.as_deref().is_none_or(str::is_empty) {
        return Err(format!("job {n}: no stored repository password"));
    }
    Ok(j)
}

/// Checks a whole document.
pub fn normalize(c: BackupConfig) -> Result<BackupConfig, String> {
    let mut jobs: Vec<BackupJob> = Vec::with_capacity(c.jobs.len());
    for j in c.jobs {
        let j = normalize_job(j)?;
        if jobs.iter().any(|x| x.name == j.name) {
            return Err(format!("job {} is defined twice", j.name));
        }
        jobs.push(j);
    }
    if jobs.len() > 64 {
        return Err("at most 64 backup jobs".into());
    }
    Ok(BackupConfig { jobs })
}

/// The `backup` key.
pub struct BackupDomain;

impl Applier for BackupDomain {
    fn key(&self) -> &str {
        BACKUP_KEY
    }

    /// Before `scheduler` (40), whose units run the jobs.
    fn order(&self) -> i32 {
        35
    }

    fn validate(&self, value: Value) -> Result<Value, rynas_config::Invalid> {
        let c: BackupConfig = serde_json::from_value(value)
            .map_err(|e| format!("invalid backup document: {e}"))?;
        let c = normalize(c)?;
        serde_json::to_value(c).map_err(|e| e.to_string().into())
    }

    fn render(&self, _value: &Value, _ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::backup::{Maintenance, Retention};

    pub(crate) fn job(name: &str, dest: Destination) -> BackupJob {
        BackupJob {
            name: name.into(),
            description: String::new(),
            sources: vec![BackupSource::Share {
                name: "docs".into(),
            }],
            destination: dest,
            retention: Retention::SMART,
            maintenance: Maintenance::default(),
            excludes: vec![],
            use_snapshot: true,
            stop_stacks: vec![],
            bandwidth_kib: 0,
            sealed: Some("c2VhbGVk".into()),
            ssh_public_key: String::new(),
        }
    }

    #[test]
    fn names_and_destinations() {
        assert!(valid_job_name("docs-nightly"));
        for bad in ["", "Docs", "-x", "a b", &"x".repeat(33)] {
            assert!(!valid_job_name(bad), "{bad}");
        }
        let ok = [
            Destination::Local {
                path: "/media/usb1/backup/".into(),
            },
            Destination::Rest {
                url: "https://nas2:8000/docs".into(),
                user: "nas1".into(),
            },
            Destination::Sftp {
                host: "nas2.lan".into(),
                port: 22,
                user: "backup".into(),
                path: "/volume1/backup/nas1".into(),
            },
            Destination::S3 {
                endpoint: "https://s3.eu-central-003.backblazeb2.com".into(),
                region: String::new(),
                bucket: "b".into(),
                prefix: "/nas1/".into(),
                access_key_id: "AK".into(),
            },
            Destination::B2 {
                bucket: "b".into(),
                bucket_id: "id".into(),
                prefix: String::new(),
                key_id: "k".into(),
            },
            Destination::Rclone {
                remote: "gdrive:backup/nas1".into(),
            },
        ];
        for d in ok {
            let j = normalize_job(job("x", d.clone())).unwrap_or_else(|e| panic!("{d:?}: {e}"));
            if let Destination::Local { path } = &j.destination {
                assert_eq!(path, "/media/usb1/backup");
            }
            if let Destination::S3 { prefix, .. } = &j.destination {
                assert_eq!(prefix, "nas1");
            }
        }
        let bad = [
            Destination::Local { path: "/".into() },
            Destination::Local {
                path: "/media".into(),
            },
            Destination::Local {
                path: "/a/../b".into(),
            },
            Destination::Rest {
                url: "https://u:p@nas2/x".into(),
                user: String::new(),
            },
            Destination::Rest {
                url: "ftp://nas2".into(),
                user: String::new(),
            },
            Destination::Sftp {
                host: "-oProxyCommand=x".into(),
                port: 22,
                user: "a".into(),
                path: "/x".into(),
            },
            Destination::Rclone {
                remote: "--config=/etc/x".into(),
            },
            Destination::Rclone {
                remote: "nocolon".into(),
            },
        ];
        for d in bad {
            assert!(normalize_job(job("x", d.clone())).is_err(), "{d:?}");
        }
    }

    #[test]
    fn job_rules() {
        let local = || Destination::Local {
            path: "/media/usb/b".into(),
        };
        let mut j = job("x", local());
        j.retention = Retention {
            last: 0,
            hourly: 0,
            daily: 0,
            weekly: 0,
            monthly: 0,
            yearly: 0,
        };
        assert!(normalize_job(j).is_err());
        let mut j = job("x", local());
        j.sealed = None;
        assert!(normalize_job(j).unwrap_err().contains("password"));
        let mut j = job("x", local());
        j.sources.push(j.sources[0].clone());
        assert!(normalize_job(j).is_err());
        let mut j = job("x", local());
        j.excludes = vec!["!keep".into()];
        assert!(normalize_job(j).is_err());
        let mut j = job("x", local());
        j.ssh_public_key = "ssh-ed25519 AAAA".into();
        assert!(normalize_job(j).unwrap().ssh_public_key.is_empty());
        let c = BackupConfig {
            jobs: vec![job("a", local()), job("a", local())],
        };
        assert!(normalize(c).is_err());
        let v = BackupDomain
            .validate(serde_json::to_value(BackupConfig { jobs: vec![job("a", local())] }).unwrap())
            .unwrap();
        assert_eq!(v["jobs"][0]["name"], "a");
    }
}
