//! Image references and the update check: the local image's registry digest against the
//! registry's digest of the same tag, read from the distribution endpoint without pulling.

use std::collections::{BTreeMap, BTreeSet};

use rynas_api_types::docker::ImageUpdate;

/// `(repository, tag)` of a reference; digests (`@sha256:…`) give `None` as the tag. The
/// repository is normalized (`nginx` → `docker.io/library/nginx`).
pub fn split_ref(image: &str) -> (String, Option<String>) {
    let (name, digest) = match image.split_once('@') {
        Some((n, _)) => (n, true),
        None => (image, false),
    };
    // a tag is after the last ':' that comes after the last '/' (a registry port is not)
    let slash = name.rfind('/').map(|i| i + 1).unwrap_or(0);
    let (repo, tag) = match name[slash..].rfind(':') {
        Some(i) => (&name[..slash + i], Some(name[slash + i + 1..].to_string())),
        None => (name, None),
    };
    let tag = if digest {
        None
    } else {
        Some(tag.unwrap_or_else(|| "latest".into()))
    };
    (normalize_repo(repo), tag)
}

/// `nginx` → `docker.io/library/nginx`, `user/app` → `docker.io/user/app`,
/// `ghcr.io/a/b` unchanged.
pub fn normalize_repo(repo: &str) -> String {
    let first = repo.split('/').next().unwrap_or("");
    let has_registry =
        repo.contains('/') && (first.contains('.') || first.contains(':') || first == "localhost");
    if has_registry {
        repo.to_string()
    } else if repo.contains('/') {
        format!("docker.io/{repo}")
    } else {
        format!("docker.io/library/{repo}")
    }
}

/// The registry digest (`sha256:…`) of the local image for `image`'s repository, from the
/// image's `RepoDigests` (`nginx@sha256:…`).
pub fn local_digest(image: &str, repo_digests: &[String]) -> Option<String> {
    let (repo, _) = split_ref(image);
    repo_digests.iter().find_map(|d| {
        let (r, dg) = d.split_once('@')?;
        (normalize_repo(r) == repo).then(|| dg.to_string())
    })
}

/// A random number in `0..max` (0 when `max` is 0 or the random source fails): the spread of
/// registry requests and scheduled runs.
pub fn random_below(max: u64) -> u64 {
    if max == 0 {
        return 0;
    }
    let mut b = [0u8; 8];
    if getrandom::fill(&mut b).is_err() {
        return 0;
    }
    u64::from_le_bytes(b) % max
}

/// A reference worth checking: a tag (not a digest, not an image id).
pub fn checkable(image: &str) -> bool {
    !image.is_empty() && !image.starts_with("sha256:") && split_ref(image).1.is_some()
}

/// One image in use, before the registry is asked.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InUse {
    pub image: String,
    pub local_digest: Option<String>,
    pub stacks: BTreeSet<String>,
    pub containers: BTreeSet<String>,
}

/// The images containers use, grouped by reference: `(image ref, image repo digests,
/// stack, container name)` per container.
pub fn in_use<'a>(
    rows: impl IntoIterator<Item = (&'a str, &'a [String], Option<&'a str>, &'a str)>,
) -> Vec<InUse> {
    let mut m: BTreeMap<String, InUse> = BTreeMap::new();
    for (image, digests, stack, name) in rows {
        if !checkable(image) {
            continue;
        }
        let e = m.entry(image.to_string()).or_insert_with(|| InUse {
            image: image.to_string(),
            local_digest: local_digest(image, digests),
            ..Default::default()
        });
        if let Some(s) = stack {
            e.stacks.insert(s.to_string());
        }
        e.containers.insert(name.to_string());
    }
    m.into_values().collect()
}

/// The report line of one image once the registry answered (`remote`: its digest, or why
/// it failed).
pub fn compare(u: InUse, remote: Result<String, String>) -> ImageUpdate {
    let (remote_digest, error) = match remote {
        Ok(d) => (Some(d), None),
        Err(e) => (None, Some(e)),
    };
    let update_available = match (&u.local_digest, &remote_digest) {
        (Some(l), Some(r)) => l != r,
        _ => false,
    };
    ImageUpdate {
        image: u.image,
        local_digest: u.local_digest,
        remote_digest,
        update_available,
        error,
        stacks: u.stacks.into_iter().collect(),
        containers: u.containers.into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs() {
        assert_eq!(
            split_ref("nginx"),
            ("docker.io/library/nginx".into(), Some("latest".into()))
        );
        assert_eq!(
            split_ref("nginx:1.27"),
            ("docker.io/library/nginx".into(), Some("1.27".into()))
        );
        assert_eq!(
            split_ref("user/app:2"),
            ("docker.io/user/app".into(), Some("2".into()))
        );
        assert_eq!(
            split_ref("registry.local:5000/team/app"),
            ("registry.local:5000/team/app".into(), Some("latest".into()))
        );
        assert_eq!(
            split_ref("ghcr.io/a/b@sha256:abc"),
            ("ghcr.io/a/b".into(), None)
        );
        assert!(checkable("postgres:16") && !checkable("sha256:abcd") && !checkable("x@sha256:1"));
    }

    #[test]
    fn digests() {
        let d = vec![
            "nginx@sha256:old".to_string(),
            "other/x@sha256:zz".to_string(),
        ];
        assert_eq!(
            local_digest("docker.io/library/nginx:1.27", &d).as_deref(),
            Some("sha256:old")
        );
        assert_eq!(local_digest("redis", &d), None);
        let rows = vec![
            ("nginx:1.27", d.as_slice(), Some("web"), "web-nginx-1"),
            ("nginx:1.27", d.as_slice(), Some("blog"), "blog-nginx-1"),
            ("sha256:123", d.as_slice(), None, "x"),
        ];
        let u = in_use(rows);
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].stacks.len(), 2);
        let r = compare(u[0].clone(), Ok("sha256:new".into()));
        assert!(r.update_available);
        let r = compare(u[0].clone(), Ok("sha256:old".into()));
        assert!(!r.update_available);
        let r = compare(u[0].clone(), Err("unauthorized".into()));
        assert!(!r.update_available && r.error.is_some());
    }
}
