//! Installing `docker-ce` from Docker's own apt repository (deb822, signed-by key in
//! `/etc/apt/keyrings`), pinned to the major version it installs. Only on request and only
//! when no engine exists: an existing one is adopted, never reinstalled.
//!
//! The steps are data ([`InstallStep`]) so that rynasd runs them in a task (logging every
//! command, `--dry-system` only logs them) and tests read them.

use std::time::Duration;

use rynas_tasks::Cmd;

/// Docker's repository for Debian.
pub const REPO_URL: &str = "https://download.docker.com/linux/debian";
/// The repository's signing key.
pub const KEY_URL: &str = "https://download.docker.com/linux/debian/gpg";
pub const KEYRING: &str = "/etc/apt/keyrings/docker.asc";
/// Where the download lands until its fingerprint is checked.
pub const KEY_STAGING: &str = "/etc/apt/keyrings/.docker.asc.rynas-new";
/// The fingerprint Docker publishes for its repository key ("Docker Release (CE deb)",
/// key id `0EBFCD88`, docs.docker.com install pages): a download with another primary key
/// is refused and never reaches `/etc/apt/keyrings` (review 2026-11-07, M7).
pub const KEY_FINGERPRINT: &str = "9DC858229FC7DD38854AE2D88D81803C0EBFCD88";
pub const SOURCES: &str = "/etc/apt/sources.list.d/docker.sources";
/// The apt pin to the installed major version.
pub const PREFERENCES: &str = "/etc/apt/preferences.d/rynas-docker";
/// What rynas installs.
pub const PACKAGES: &[&str] = &[
    "docker-ce",
    "docker-ce-cli",
    "containerd.io",
    "docker-buildx-plugin",
    "docker-compose-plugin",
];
/// Packages the pin holds to the major version.
const PINNED: &str = "docker-ce docker-ce-cli docker-ce-rootless-extras";

/// apt can take long on a slow mirror or a small CPU.
pub const APT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const SHORT: Duration = Duration::from_secs(60);

/// One step of the install.
#[derive(Debug, Clone)]
pub enum InstallStep {
    Run(Cmd),
    /// Check the OpenPGP key downloaded to `staged` against [`KEY_FINGERPRINT`]
    /// ([`check_key`]), then install it as `dest` (0644) and remove `staged`; a mismatch
    /// removes `staged` and fails the install.
    CheckKey {
        staged: &'static str,
        dest: &'static str,
    },
    /// Write a whole file (root-owned).
    Write {
        path: &'static str,
        content: String,
        mode: u32,
    },
}

/// The deb822 source of Docker's repository.
pub fn sources_content(codename: &str, arch: &str) -> String {
    format!(
        "# Managed by rynas: Docker's repository (docker-ce)\nTypes: deb\nURIs: {REPO_URL}\nSuites: {codename}\nComponents: stable\nArchitectures: {arch}\nSigned-By: {KEYRING}\n"
    )
}

/// The apt preferences that keep the engine on `major` (a new major is an explicit action).
pub fn preferences_content(major: u32) -> String {
    format!(
        "# Managed by rynas: Docker stays on major version {major}; moving to a new major is an explicit action\nPackage: {PINNED}\nPin: version 5:{major}.*\nPin-Priority: 1001\n"
    )
}

/// The major version of `docker-ce`'s candidate in `apt-cache policy docker-ce`
/// (`Candidate: 5:29.0.1-1~debian.13~trixie` → 29).
pub fn candidate_major(policy: &str) -> Option<u32> {
    let c = policy
        .lines()
        .find_map(|l| l.trim().strip_prefix("Candidate:"))?
        .trim();
    let v = c.split_once(':').map(|(_, v)| v).unwrap_or(c);
    v.split('.').next()?.parse().ok()
}

/// The major version pinned in a [`preferences_content`] file.
pub fn pinned_major(text: &str) -> Option<u32> {
    text.lines()
        .find_map(|l| l.trim().strip_prefix("Pin: version 5:"))?
        .split('.')
        .next()?
        .parse()
        .ok()
}

/// apt-get in a transient unit outside rynasd's sandbox, like the system updates do:
/// rynasd runs with `ProtectSystem=yes`, so dpkg cannot write `/usr` from inside it (found
/// on the first real install: every package failed with "Read-only file system").
/// `systemd-run --wait --pipe` streams the output into the task log and returns apt's
/// exit status.
fn apt(args: &[&str]) -> Cmd {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let unit = format!(
        "--unit=rynas-apt-docker-{secs}-{}",
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    Cmd::new("systemd-run")
        .args(["--wait", "--pipe", "--collect", "--quiet", "--service-type=exec"])
        .arg(unit)
        .args([
            "--description=rynas: apt-get (Docker install)",
            "--setenv=DEBIAN_FRONTEND=noninteractive",
            "--setenv=NEEDRESTART_MODE=l",
            "--setenv=APT_LISTCHANGES_FRONTEND=none",
            "--setenv=LC_ALL=C",
            "--",
            "apt-get",
        ])
        .args(args)
        .env_clear()
        .timeout(APT_TIMEOUT)
}

/// `apt-cache policy docker-ce` (after the repository is added).
pub fn policy_cmd() -> Cmd {
    Cmd::new("apt-cache")
        .args(["policy", "docker-ce"])
        .env_clear()
        .timeout(SHORT)
}

/// Steps until the repository is known to apt (then rynasd reads [`policy_cmd`], writes the
/// pin, applies the engine settings and runs [`install_cmd`]).
pub fn repo_steps(codename: &str, arch: &str) -> Vec<InstallStep> {
    vec![
        InstallStep::Run(apt(&["update"])),
        InstallStep::Run(apt(&["install", "-y", "ca-certificates", "curl"])),
        InstallStep::Run(
            Cmd::new("install")
                .args(["-m", "0755", "-d", "/etc/apt/keyrings"])
                .env_clear()
                .timeout(SHORT),
        ),
        InstallStep::Run(
            Cmd::new("curl")
                .args(["-fsSL", "--proto", "=https", KEY_URL, "-o", KEY_STAGING])
                .env_clear()
                .timeout(Duration::from_secs(120)),
        ),
        InstallStep::CheckKey {
            staged: KEY_STAGING,
            dest: KEYRING,
        },
        InstallStep::Write {
            path: SOURCES,
            content: sources_content(codename, arch),
            mode: 0o644,
        },
        InstallStep::Run(apt(&["update"])),
    ]
}

/// The package install. `--force-confold` keeps configuration files rynas wrote before
/// (`/etc/containerd/config.toml`).
pub fn install_cmd() -> Cmd {
    apt(&[
        "install",
        "-y",
        "-o",
        "Dpkg::Options::=--force-confdef",
        "-o",
        "Dpkg::Options::=--force-confold",
    ])
    .args(PACKAGES)
}

/// Commands that create the `docker-apps` system group and user (no home, no shell) when
/// `getent` did not find them.
pub fn apps_account_cmds(group_exists: bool, user_exists: bool) -> Vec<Cmd> {
    let mut v = Vec::new();
    if !group_exists {
        v.push(
            Cmd::new("groupadd")
                .args(["--system", crate::APPS_USER])
                .env_clear()
                .timeout(SHORT),
        );
    }
    if !user_exists {
        v.push(
            Cmd::new("useradd")
                .args([
                    "--system",
                    "--gid",
                    crate::APPS_USER,
                    "--no-create-home",
                    "--home-dir",
                    "/nonexistent",
                    "--shell",
                    "/usr/sbin/nologin",
                    "--comment",
                    "rynas Docker apps",
                    crate::APPS_USER,
                ])
                .env_clear()
                .timeout(SHORT),
        );
    }
    v
}

/// `(uid, gid)` of `docker-apps` from `passwd` and `group` texts.
pub fn apps_ids(passwd: &str, group: &str) -> (Option<u32>, Option<u32>) {
    let field = |text: &str, n: usize| {
        text.lines().find_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            (f.first() == Some(&crate::APPS_USER))
                .then(|| f.get(n)?.parse().ok())
                .flatten()
        })
    };
    (field(passwd, 2), field(group, 2))
}

/// The v4 fingerprint (40 upper-case hex digits) of the first public key of an
/// ASCII-armored OpenPGP key block: SHA-1 over `0x99`, the 2-byte length and the body of the
/// public-key packet (RFC 4880 §12.2).
pub fn key_fingerprint(armored: &str) -> Result<String, String> {
    use base64::Engine;
    use sha1::Digest;
    let mut lines = armored.lines().map(str::trim);
    lines
        .by_ref()
        .find(|l| *l == "-----BEGIN PGP PUBLIC KEY BLOCK-----")
        .ok_or("not an armored OpenPGP public key")?;
    // armor headers end at the first empty line (there may be none)
    let mut b64 = String::new();
    let mut in_headers = true;
    for l in lines {
        if l.starts_with("-----END") {
            break;
        }
        if in_headers {
            if l.is_empty() {
                in_headers = false;
                continue;
            }
            if l.contains(": ") {
                continue;
            }
            in_headers = false;
        }
        if l.starts_with('=') {
            continue; // the CRC-24 line
        }
        b64.push_str(l);
    }
    let data = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .map_err(|e| format!("bad base64 in the key: {e}"))?;
    let (&tag, rest) = data.split_first().ok_or("empty key")?;
    if tag & 0x80 == 0 {
        return Err("not an OpenPGP packet".into());
    }
    let (kind, len, rest) = if tag & 0x40 != 0 {
        // new format
        let (&l0, r) = rest.split_first().ok_or("truncated key")?;
        match l0 {
            0..=191 => (tag & 0x3f, usize::from(l0), r),
            192..=223 => {
                let (&l1, r) = r.split_first().ok_or("truncated key")?;
                (tag & 0x3f, ((usize::from(l0) - 192) << 8) + usize::from(l1) + 192, r)
            }
            255 if r.len() >= 4 => (
                tag & 0x3f,
                u32::from_be_bytes([r[0], r[1], r[2], r[3]]) as usize,
                &r[4..],
            ),
            _ => return Err("unsupported packet length".into()),
        }
    } else {
        let n = match tag & 3 {
            0 => 1,
            1 => 2,
            2 => 4,
            _ => return Err("unsupported packet length".into()),
        };
        if rest.len() < n {
            return Err("truncated key".into());
        }
        let len = rest[..n].iter().fold(0usize, |a, &b| (a << 8) | usize::from(b));
        ((tag >> 2) & 0x0f, len, &rest[n..])
    };
    if kind != 6 {
        return Err("the block does not start with a public key".into());
    }
    let body = rest.get(..len).ok_or("truncated key")?;
    if body.first() != Some(&4) {
        return Err("not a version 4 key".into());
    }
    let len16 = u16::try_from(len).map_err(|_| "key packet too long")?;
    let mut h = sha1::Sha1::new();
    h.update([0x99]);
    h.update(len16.to_be_bytes());
    h.update(body);
    Ok(h.finalize().iter().map(|b| format!("{b:02X}")).collect())
}

/// Checks a downloaded key against [`KEY_FINGERPRINT`].
pub fn check_key(armored: &str) -> Result<(), String> {
    let fp = key_fingerprint(armored)?;
    if fp == KEY_FINGERPRINT {
        Ok(())
    } else {
        Err(format!(
            "the downloaded Docker repository key has fingerprint {fp}, expected {KEY_FINGERPRINT}: refused"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A made-up v4 RSA key (and a user id packet); its fingerprint was computed apart.
    const TEST_KEY: &str = "-----BEGIN PGP PUBLIC KEY BLOCK-----
Comment: test only

mQENBFloLwABCACAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAADA5ABEBAAHNFHRlc3QgPHRAZXhhbXBs
ZS5vcmc+
=abcd
-----END PGP PUBLIC KEY BLOCK-----
";

    #[test]
    fn key_fingerprint_and_pin() {
        assert_eq!(
            key_fingerprint(TEST_KEY).unwrap(),
            "ED687B515E83E07037DBA826A1D0CFBAD767D2DE"
        );
        // another key than Docker's is refused, garbage too
        assert!(check_key(TEST_KEY).unwrap_err().contains("refused"));
        assert!(check_key("<html>not found</html>").is_err());
        // the download never goes straight to the keyring
        let steps = repo_steps("trixie", "amd64");
        assert!(steps.iter().any(|s| matches!(
            s,
            InstallStep::CheckKey { staged, dest } if *staged == KEY_STAGING && *dest == KEYRING
        )));
        assert!(!steps.iter().any(|s| matches!(s, InstallStep::Run(c) if c.display().contains(&format!("-o {KEYRING}")))));
    }

    #[test]
    fn deb822_and_pin() {
        let s = sources_content("trixie", "amd64");
        assert!(
            s.contains("Suites: trixie\n")
                && s.contains("Signed-By: /etc/apt/keyrings/docker.asc\n")
        );
        assert!(s.contains("URIs: https://download.docker.com/linux/debian\n"));
        let p = preferences_content(29);
        assert!(p.contains("Pin: version 5:29.*\nPin-Priority: 1001\n"));
        assert_eq!(pinned_major(&p), Some(29));
    }

    #[test]
    fn candidate() {
        let out = "docker-ce:\n  Installed: (none)\n  Candidate: 5:29.0.1-1~debian.13~trixie\n  Version table:\n";
        assert_eq!(candidate_major(out), Some(29));
        assert_eq!(candidate_major("docker-ce:\n  Candidate: (none)\n"), None);
    }

    #[test]
    fn install_line_keeps_old_conffiles() {
        let c = install_cmd().display();
        assert!(c.starts_with("systemd-run --wait --pipe --collect --quiet"), "{c}");
        assert!(c.contains(" -- apt-get install -y -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold docker-ce docker-ce-cli containerd.io"), "{c}");
        let steps = repo_steps("bookworm", "arm64");
        assert!(matches!(&steps[5], InstallStep::Write { path, .. } if *path == SOURCES));
        assert!(matches!(&steps[3], InstallStep::Run(c) if c.display().contains("--proto =https")));
    }

    #[test]
    fn apps_account() {
        assert_eq!(apps_account_cmds(true, true).len(), 0);
        let v = apps_account_cmds(false, false);
        assert_eq!(v[0].display(), "groupadd --system docker-apps");
        assert!(v[1].display().contains("--shell /usr/sbin/nologin"));
        assert_eq!(
            apps_ids(
                "root:x:0:0::/root:/bin/bash\ndocker-apps:x:996:995::/nonexistent:/usr/sbin/nologin\n",
                "docker-apps:x:995:\n"
            ),
            (Some(996), Some(995))
        );
    }
}
