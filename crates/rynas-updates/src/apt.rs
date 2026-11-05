//! Parsers of apt's simulation output (`LC_ALL=C apt-get -s ...`).
//!
//! The machine-readable part of a simulation is one line per action:
//!
//! ```text
//! Remv libfuse2t64 [2.9.9-9]
//! Inst libssl3t64 [3.5.1-1] (3.5.4-1~deb13u1 Debian-Security:13/stable-security [amd64])
//! Inst linux-image-6.12.57+deb13-amd64 (6.12.57-1 Debian:13.2/stable, Debian-Security:13/stable-security [amd64])
//! ```
//!
//! (`[current]` is missing for a new package; a trailing `[]` lists broken packages.) The
//! kept-back packages only appear in the human summary: `The following packages have been kept
//! back:` (apt 2.x) or `Not upgrading:` (apt 3.x, Debian 13).

use rynas_api_types::updates::UpdatePackage;

/// What a full-upgrade simulation says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Simulation {
    pub packages: Vec<UpdatePackage>,
    pub removals: Vec<String>,
    pub held: Vec<String>,
}

/// An origin from a security suite (`Debian-Security:13/stable-security`).
fn is_security(origin: &str) -> bool {
    origin
        .split(", ")
        .any(|o| o.contains("-security") || o.starts_with("Debian-Security:"))
}

fn parse_inst(rest: &str) -> Option<UpdatePackage> {
    let (name, mut rest) = rest.split_once(' ').unwrap_or((rest, ""));
    let mut current = None;
    if let Some(r) = rest.strip_prefix('[') {
        let (cur, after) = r.split_once(']')?;
        current = Some(cur.to_string());
        rest = after.trim_start();
    }
    let inner = rest.strip_prefix('(')?;
    let inner = &inner[..inner.rfind(')')?];
    let (candidate, tail) = inner.split_once(' ').unwrap_or((inner, ""));
    let (origin, arch) = match tail.rfind(" [").or_else(|| tail.starts_with('[').then_some(0)) {
        Some(i) => {
            let a = tail[i..].trim_start();
            (
                tail[..i].trim(),
                a.trim_start_matches('[').trim_end_matches(']'),
            )
        }
        None => (tail.trim(), ""),
    };
    Some(UpdatePackage {
        name: name.to_string(),
        arch: arch.to_string(),
        current,
        candidate: candidate.to_string(),
        security: is_security(origin),
        origin: origin.to_string(),
    })
}

/// Parses `apt-get -s full-upgrade` (or `install --only-upgrade`).
pub fn parse_simulation(text: &str) -> Simulation {
    let mut s = Simulation::default();
    let mut in_held = false;
    for line in text.lines() {
        if in_held {
            if let Some(l) = line.strip_prefix(' ') {
                s.held.extend(l.split_whitespace().map(str::to_string));
                continue;
            }
            in_held = false;
        }
        if line.starts_with("The following packages have been kept back:")
            || line == "Not upgrading:"
        {
            in_held = true;
        } else if let Some(r) = line.strip_prefix("Inst ") {
            if let Some(p) = parse_inst(r) {
                s.packages.push(p);
            }
        } else if let Some(n) = line
            .strip_prefix("Remv ")
            .or_else(|| line.strip_prefix("Purg "))
            .and_then(|r| r.split_whitespace().next())
        {
            s.removals.push(n.to_string());
        }
    }
    s.packages.sort_by(|a, b| a.name.cmp(&b.name));
    s.removals.sort();
    s.removals.dedup();
    s.held.sort();
    s.held.dedup();
    s
}

/// The packages a simulation removes (`apt-get -s autoremove`).
pub fn parse_removals(text: &str) -> Vec<String> {
    parse_simulation(text).removals
}

/// A Debian package name, optionally with `:arch` (policy §5.6.1: lowercase letters, digits,
/// `+ - .`, at least two characters, starting with a letter or digit). Nothing that apt could
/// read as an option or a pattern.
pub fn valid_package_name(n: &str) -> bool {
    let (name, arch) = match n.split_once(':') {
        Some((a, b)) => (a, Some(b)),
        None => (n, None),
    };
    let ok_name = name.len() >= 2
        && name.len() <= 128
        && name
            .bytes()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"+-.".contains(&c));
    let ok_arch = arch.is_none_or(|a| {
        !a.is_empty() && a.len() <= 32 && a.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
    });
    ok_name && ok_arch
}

/// `/var/run/reboot-required.pkgs`: one package per line, repeated per trigger.
pub fn parse_reboot_pkgs(text: &str) -> Vec<String> {
    let mut v: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    v.sort();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{AUTOREMOVE, FULL_UPGRADE};

    #[test]
    fn full_upgrade_fixture() {
        let s = parse_simulation(FULL_UPGRADE);
        assert_eq!(s.packages.len(), 10);
        assert_eq!(s.removals, ["libfuse2t64"]);
        assert_eq!(s.held, ["synodeb"]);
        let ssl = s.packages.iter().find(|p| p.name == "libssl3t64").unwrap();
        assert_eq!(ssl.current.as_deref(), Some("3.5.1-1+deb13u1"));
        assert_eq!(ssl.candidate, "3.5.4-1~deb13u1");
        assert_eq!(ssl.origin, "Debian-Security:13/stable-security");
        assert_eq!(ssl.arch, "amd64");
        assert!(ssl.security);
        let new = s
            .packages
            .iter()
            .find(|p| p.name == "linux-image-6.12.57+deb13-amd64")
            .unwrap();
        assert_eq!(new.current, None);
        assert!(new.security, "{new:?}");
        let ssh = s.packages.iter().find(|p| p.name == "openssh-client").unwrap();
        assert_eq!(ssh.candidate, "1:10.0p1-7+deb13u1");
        assert_eq!(ssh.arch, "amd64");
        assert!(!ssh.security);
        let tz = s.packages.iter().find(|p| p.name == "tzdata").unwrap();
        assert_eq!(tz.arch, "all");
        assert!(!tz.security);
        assert_eq!(s.packages.iter().filter(|p| p.security).count(), 4);
    }

    #[test]
    fn apt2_kept_back_and_purge() {
        let t = "The following packages have been kept back:\n  a b\n  c\nThe following packages will be upgraded:\n  d\nPurg x [1]\nInst d [1] (2 Debian:12.1/stable [amd64])\n";
        let s = parse_simulation(t);
        assert_eq!(s.held, ["a", "b", "c"]);
        assert_eq!(s.removals, ["x"]);
        assert_eq!(s.packages[0].name, "d");
    }

    #[test]
    fn autoremove_fixture() {
        assert_eq!(
            parse_removals(AUTOREMOVE),
            ["linux-image-6.12.38+deb13-amd64", "synology-ds923"]
        );
    }

    #[test]
    fn names() {
        for ok in ["libssl3t64", "g++", "linux-image-6.12.57+deb13-amd64", "libc6:i386"] {
            assert!(valid_package_name(ok), "{ok}");
        }
        for bad in ["", "a", "-o", "Foo", "a b", "a;b", "x:", "~i", "a*", "pkg/stable"] {
            assert!(!valid_package_name(bad), "{bad}");
        }
    }

    #[test]
    fn reboot_pkgs() {
        assert_eq!(
            parse_reboot_pkgs("linux-image-amd64\nlibc6\nlinux-image-amd64\n\n"),
            ["libc6", "linux-image-amd64"]
        );
    }
}
