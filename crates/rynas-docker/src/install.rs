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

fn apt(args: &[&str]) -> Cmd {
    Cmd::new("apt-get")
        .args(args)
        .env_clear()
        .env("DEBIAN_FRONTEND", "noninteractive")
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
                .args(["-fsSL", "--proto", "=https", KEY_URL, "-o", KEYRING])
                .env_clear()
                .timeout(Duration::from_secs(120)),
        ),
        InstallStep::Run(
            Cmd::new("chmod")
                .args(["a+r", KEYRING])
                .env_clear()
                .timeout(SHORT),
        ),
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(c.starts_with("apt-get install -y -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold docker-ce docker-ce-cli containerd.io"), "{c}");
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
