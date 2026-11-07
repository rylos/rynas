//! Is there an engine, where does it come from, can rynas install one. Commands only read
//! (`dpkg-query`, `docker compose version`, `docker buildx version`); the daemon itself is
//! asked through the [`crate::client`].

use std::path::Path;
use std::time::Duration;

use rynas_api_types::docker::{DockerPackage, EngineSource, IncompletePackage, InstallBlock};
use rynas_tasks::Cmd;

use crate::{Runner, under};

/// Packages whose presence tells where the engine comes from.
pub const PACKAGES: &[&str] = &[
    "docker-ce",
    "docker-ce-cli",
    "containerd.io",
    "docker-compose-plugin",
    "docker-buildx-plugin",
    "docker.io",
    "docker-cli",
    "docker-compose",
    "containerd",
];

/// Where `docker`/`dockerd` binaries are looked for.
const BINARIES: &[&str] = &[
    "/usr/bin/docker",
    "/usr/bin/dockerd",
    "/usr/local/bin/docker",
    "/usr/local/bin/dockerd",
    "/snap/bin/docker",
];

/// Debian releases Docker's repository serves.
pub const SUPPORTED_CODENAMES: &[&str] = &["bullseye", "bookworm", "trixie"];
/// Architectures Docker builds `docker-ce` for.
pub const SUPPORTED_ARCHES: &[&str] = &["amd64", "arm64", "armhf", "ppc64el", "s390x"];

const QUICK: Duration = Duration::from_secs(10);

/// What the system says without the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstallFacts {
    /// A docker CLI or daemon binary exists, or an engine package is installed; false while
    /// engine packages are half-installed.
    pub installed: bool,
    pub source: EngineSource,
    pub packages: Vec<DockerPackage>,
    /// Engine packages dpkg left half-done (an interrupted install): the install repairs
    /// them.
    pub incomplete: Vec<IncompletePackage>,
    /// An install rynas started (its Docker source file exists) did not finish: the engine
    /// is complete but the pin or the `docker-apps` account is missing. The install action
    /// is offered again to finish it.
    pub resumable: bool,
    pub compose_version: Option<String>,
    pub buildx_version: Option<String>,
    pub install_block: Option<InstallBlock>,
    /// `VERSION_CODENAME` of os-release.
    pub codename: Option<String>,
    /// `dpkg --print-architecture`.
    pub arch: Option<String>,
}

/// `(ID, VERSION_CODENAME)` of an os-release text.
pub fn os_release(text: &str) -> (Option<String>, Option<String>) {
    let mut id = None;
    let mut code = None;
    for l in text.lines() {
        let Some((k, v)) = l.split_once('=') else {
            continue;
        };
        let v = v.trim().trim_matches('"').to_string();
        match k.trim() {
            "ID" => id = Some(v),
            "VERSION_CODENAME" if !v.is_empty() => code = Some(v),
            _ => {}
        }
    }
    (id, code)
}

/// Installed packages from `dpkg-query -W -f '${Package}\t${Version}\t${db:Status-Abbrev}\n'`
/// (`ii ` = installed; packages dpkg does not know are on stderr and ignored).
pub fn parse_dpkg(out: &str) -> Vec<DockerPackage> {
    parse_dpkg_status(out).0
}

/// The same output split in complete packages (status "install ok installed": `ii `, `hi `)
/// and half-done ones: status half-installed `H`, unpacked `U`, half-configured `F`,
/// triggers awaiting `W` or pending `t`, or the reinstall-required flag `R`. Packages that
/// are not installed (`un`, `rc`, `pn`) are in neither.
pub fn parse_dpkg_status(out: &str) -> (Vec<DockerPackage>, Vec<IncompletePackage>) {
    let mut done = Vec::new();
    let mut half = Vec::new();
    for l in out.lines() {
        let mut f = l.split('\t');
        let (Some(name), Some(version), Some(abbrev)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        let b = abbrev.as_bytes();
        let reinst = b.get(2) == Some(&b'R');
        match b.get(1) {
            Some(b'i') if !reinst => {
                if !version.is_empty() {
                    done.push(DockerPackage {
                        name: name.to_string(),
                        version: version.to_string(),
                    });
                }
            }
            Some(b'H' | b'U' | b'F' | b'W' | b't' | b'i') => half.push(IncompletePackage {
                name: name.to_string(),
                version: version.to_string(),
                status: abbrev.trim_end().to_string(),
            }),
            _ if reinst => half.push(IncompletePackage {
                name: name.to_string(),
                version: version.to_string(),
                status: abbrev.trim_end().to_string(),
            }),
            _ => {}
        }
    }
    (done, half)
}

/// Where the engine comes from, from the installed packages and the binaries found.
pub fn source_of(packages: &[DockerPackage], binary: bool) -> EngineSource {
    let has = |n: &str| packages.iter().any(|p| p.name == n);
    if has("docker-ce") || has("docker-ce-cli") {
        EngineSource::DockerCe
    } else if has("docker.io") || has("docker-cli") {
        EngineSource::DockerIo
    } else if binary {
        EngineSource::Other
    } else {
        EngineSource::None
    }
}

/// The version printed by `docker compose version --short` (`2.40.3`, `v2.40.3`).
pub fn parse_short_version(out: &str) -> Option<String> {
    let v = out.lines().next()?.trim().trim_start_matches('v');
    (!v.is_empty() && v.chars().next()?.is_ascii_digit()).then(|| v.to_string())
}

/// The version in `docker buildx version`: `github.com/docker/buildx v0.29.1 abc...`.
pub fn parse_buildx(out: &str) -> Option<String> {
    let w = out.split_whitespace().nth(1)?;
    parse_short_version(w)
}

/// Reads the facts: binaries and os-release below `root`, commands through `runner`.
pub fn install_facts(root: &Path, runner: &dyn Runner) -> InstallFacts {
    let binary = BINARIES.iter().any(|b| under(root, b).exists());
    let (packages, incomplete) = runner
        .run(
            &Cmd::new("dpkg-query")
                .arg("-W")
                .arg("-f")
                .arg("${Package}\t${Version}\t${db:Status-Abbrev}\n")
                .args(PACKAGES)
                .env_clear()
                .timeout(QUICK),
        )
        .map(|o| parse_dpkg_status(&o.stdout))
        .unwrap_or_default();
    let source = source_of(&packages, binary);
    // half-installed packages are no engine, even when their files (binaries) are unpacked
    let installed = incomplete.is_empty() && (binary || source != EngineSource::None);
    let resumable = installed
        && source == EngineSource::DockerCe
        && under(root, crate::install::SOURCES).exists()
        && (!under(root, crate::install::PREFERENCES).exists() || !apps_account_exists(root));
    let (compose_version, buildx_version) = if installed {
        let compose = runner
            .run(
                &Cmd::new("docker")
                    .args(["compose", "version", "--short"])
                    .env_clear()
                    .timeout(QUICK),
            )
            .ok()
            .filter(|o| o.success())
            .and_then(|o| parse_short_version(&o.stdout));
        let buildx = runner
            .run(
                &Cmd::new("docker")
                    .args(["buildx", "version"])
                    .env_clear()
                    .timeout(QUICK),
            )
            .ok()
            .filter(|o| o.success())
            .and_then(|o| parse_buildx(&o.stdout));
        (compose, buildx)
    } else {
        (None, None)
    };
    let (id, codename) =
        os_release(&std::fs::read_to_string(under(root, "/etc/os-release")).unwrap_or_default());
    let debian = id.as_deref() == Some("debian") || under(root, "/etc/debian_version").exists();
    let arch = runner
        .run(
            &Cmd::new("dpkg")
                .arg("--print-architecture")
                .env_clear()
                .timeout(QUICK),
        )
        .ok()
        .filter(|o| o.success())
        .map(|o| o.stdout.trim().to_string())
        .filter(|a| !a.is_empty());
    let install_block = if installed && !resumable {
        Some(InstallBlock::Installed)
    } else if !debian || id.as_deref().is_some_and(|i| i != "debian") {
        Some(InstallBlock::NotDebian)
    } else if !codename
        .as_deref()
        .is_some_and(|c| SUPPORTED_CODENAMES.contains(&c))
    {
        Some(InstallBlock::UnsupportedRelease)
    } else if !arch
        .as_deref()
        .is_some_and(|a| SUPPORTED_ARCHES.contains(&a))
    {
        Some(InstallBlock::UnsupportedArch)
    } else {
        None
    };
    InstallFacts {
        installed,
        source,
        packages,
        incomplete,
        resumable,
        compose_version,
        buildx_version,
        install_block,
        codename,
        arch,
    }
}

/// The `docker-apps` user and group exist in `<root>/etc/passwd` and `<root>/etc/group`.
pub fn apps_account_exists(root: &Path) -> bool {
    let read = |p: &str| std::fs::read_to_string(under(root, p)).unwrap_or_default();
    matches!(
        crate::install::apps_ids(&read("/etc/passwd"), &read("/etc/group")),
        (Some(_), Some(_))
    )
}

/// `Some(major)` of an engine version (`29.0.1` → 29).
pub fn major_of(version: &str) -> Option<u32> {
    version
        .trim_start_matches('v')
        .split('.')
        .next()?
        .parse()
        .ok()
}

/// Is an API version (`1.44`) older than `min`?
pub fn api_older(version: &str, min: (u32, u32)) -> bool {
    let mut it = version.split('.');
    let (Some(a), Some(b)) = (it.next(), it.next()) else {
        return false;
    };
    match (a.parse::<u32>(), b.parse::<u32>()) {
        (Ok(a), Ok(b)) => (a, b) < min,
        _ => false,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rynas_tasks::CmdOutput;
    use std::sync::Mutex;

    pub(crate) struct Fake {
        pub rules: Vec<(&'static str, i32, &'static str)>,
        pub calls: Mutex<Vec<String>>,
    }

    impl Runner for Fake {
        fn run(&self, cmd: &Cmd) -> Result<CmdOutput, String> {
            let line = cmd.display();
            self.calls.lock().unwrap().push(line.clone());
            for (sub, code, out) in &self.rules {
                if line.contains(sub) {
                    return Ok(CmdOutput {
                        code: Some(*code),
                        stdout: (*out).into(),
                        stderr: String::new(),
                    });
                }
            }
            Err("cannot start: No such file or directory".into())
        }
    }

    fn root(os: &str, bins: &[&str]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("etc")).unwrap();
        std::fs::write(d.path().join("etc/os-release"), os).unwrap();
        for b in bins {
            let p = under(d.path(), b);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "").unwrap();
        }
        d
    }

    const TRIXIE: &str =
        "PRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\nID=debian\nVERSION_CODENAME=trixie\n";

    #[test]
    fn nothing_installed_on_trixie_can_install() {
        let r = root(TRIXIE, &[]);
        let fake = Fake {
            rules: vec![
                ("dpkg --print-architecture", 0, "amd64\n"),
                ("dpkg-query", 1, ""),
            ],
            calls: Mutex::new(vec![]),
        };
        let f = install_facts(r.path(), &fake);
        assert!(!f.installed);
        assert_eq!(f.source, EngineSource::None);
        assert_eq!(f.install_block, None);
        assert_eq!(f.codename.as_deref(), Some("trixie"));
        // no compose probe without an engine
        assert!(
            !fake
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.contains("compose version"))
        );
    }

    #[test]
    fn docker_ce_adopted() {
        let r = root(TRIXIE, &["/usr/bin/docker", "/usr/bin/dockerd"]);
        let fake = Fake {
            rules: vec![
                (
                    "dpkg-query",
                    1,
                    "docker-ce\t5:29.0.1-1~debian.13~trixie\tii \ndocker-ce-cli\t5:29.0.1-1~debian.13~trixie\tii \ncontainerd.io\t2.1.5-1~debian.13~trixie\tii \ndocker.io\t\tun \n",
                ),
                ("compose version", 0, "2.40.3\n"),
                (
                    "buildx version",
                    0,
                    "github.com/docker/buildx v0.29.1 a1b2c3\n",
                ),
                ("--print-architecture", 0, "arm64\n"),
            ],
            calls: Mutex::new(vec![]),
        };
        let f = install_facts(r.path(), &fake);
        assert!(f.installed);
        assert_eq!(f.source, EngineSource::DockerCe);
        assert_eq!(f.packages.len(), 3);
        assert_eq!(f.compose_version.as_deref(), Some("2.40.3"));
        assert_eq!(f.buildx_version.as_deref(), Some("0.29.1"));
        assert_eq!(f.install_block, Some(InstallBlock::Installed));
    }

    #[test]
    fn debian_docker_io_and_static_binaries() {
        let pk = parse_dpkg("docker.io\t26.1.5+dfsg1-9\tii \n");
        assert_eq!(source_of(&pk, true), EngineSource::DockerIo);
        assert_eq!(source_of(&[], true), EngineSource::Other);
        assert_eq!(source_of(&[], false), EngineSource::None);
    }

    #[test]
    fn blocks() {
        let fake = Fake {
            rules: vec![("--print-architecture", 0, "amd64\n")],
            calls: Mutex::new(vec![]),
        };
        let r = root("ID=ubuntu\nVERSION_CODENAME=noble\n", &[]);
        assert_eq!(
            install_facts(r.path(), &fake).install_block,
            Some(InstallBlock::NotDebian)
        );
        let r = root("ID=debian\nVERSION_CODENAME=buster\n", &[]);
        assert_eq!(
            install_facts(r.path(), &fake).install_block,
            Some(InstallBlock::UnsupportedRelease)
        );
        let fake = Fake {
            rules: vec![("--print-architecture", 0, "i386\n")],
            calls: Mutex::new(vec![]),
        };
        let r = root(TRIXIE, &[]);
        assert_eq!(
            install_facts(r.path(), &fake).install_block,
            Some(InstallBlock::UnsupportedArch)
        );
    }

    /// What `dpkg-query` printed after the first real install died inside dpkg.
    const HALF_DONE: &str = "docker-ce\t5:29.0.1-1~debian.13~trixie\tiHR\ndocker-ce-cli\t5:29.0.1-1~debian.13~trixie\tii \ncontainerd.io\t2.1.5-1~debian.13~trixie\tiU \ndocker-buildx-plugin\t0.29.1-1~debian.13~trixie\tiF \ndocker.io\t\tun \ncontainerd\t\trc \n";

    #[test]
    fn half_installed_packages_are_no_engine() {
        let (done, half) = parse_dpkg_status(HALF_DONE);
        assert_eq!(
            done.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["docker-ce-cli"]
        );
        assert_eq!(
            half.iter()
                .map(|p| (p.name.as_str(), p.status.as_str()))
                .collect::<Vec<_>>(),
            [
                ("docker-ce", "iHR"),
                ("containerd.io", "iU"),
                ("docker-buildx-plugin", "iF")
            ]
        );
        // unpacked binaries do not make it an engine either; the install is offered (repair)
        let r = root(TRIXIE, &["/usr/bin/docker", "/usr/bin/dockerd"]);
        let fake = Fake {
            rules: vec![
                ("dpkg-query", 0, HALF_DONE),
                ("--print-architecture", 0, "amd64\n"),
            ],
            calls: Mutex::new(vec![]),
        };
        let f = install_facts(r.path(), &fake);
        assert!(!f.installed, "{f:?}");
        assert_eq!(f.incomplete.len(), 3);
        assert_eq!(f.install_block, None);
        assert!(!f.resumable);
    }

    #[test]
    fn an_unfinished_rynas_install_is_resumable() {
        let r = root(TRIXIE, &["/usr/bin/docker", "/usr/bin/dockerd"]);
        let fake = Fake {
            rules: vec![
                (
                    "dpkg-query",
                    0,
                    "docker-ce\t5:29.0.1-1~debian.13~trixie\tii \ndocker-ce-cli\t5:29.0.1-1~debian.13~trixie\tii \n",
                ),
                ("--print-architecture", 0, "amd64\n"),
            ],
            calls: Mutex::new(vec![]),
        };
        // adopted: not installed by rynas
        assert_eq!(
            install_facts(r.path(), &fake).install_block,
            Some(InstallBlock::Installed)
        );
        // rynas's source file, no pin, no account: the install finishes the job
        let src = under(r.path(), crate::install::SOURCES);
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, "x").unwrap();
        let f = install_facts(r.path(), &fake);
        assert!(
            f.installed && f.resumable && f.install_block.is_none(),
            "{f:?}"
        );
        // pin and account there: done, adopted
        let pin = under(r.path(), crate::install::PREFERENCES);
        std::fs::create_dir_all(pin.parent().unwrap()).unwrap();
        std::fs::write(&pin, crate::install::preferences_content(29)).unwrap();
        std::fs::write(
            r.path().join("etc/passwd"),
            "docker-apps:x:996:995::/nonexistent:/usr/sbin/nologin\n",
        )
        .unwrap();
        assert!(install_facts(r.path(), &fake).resumable, "group missing");
        std::fs::write(r.path().join("etc/group"), "docker-apps:x:995:\n").unwrap();
        let f = install_facts(r.path(), &fake);
        assert!(!f.resumable);
        assert_eq!(f.install_block, Some(InstallBlock::Installed));
    }

    #[test]
    fn versions() {
        assert_eq!(major_of("29.0.1"), Some(29));
        assert_eq!(parse_short_version("v2.24.0\n").as_deref(), Some("2.24.0"));
        assert_eq!(parse_short_version("unknown"), None);
        assert!(api_older("1.43", (1, 44)) && !api_older("1.52", (1, 44)));
    }
}
