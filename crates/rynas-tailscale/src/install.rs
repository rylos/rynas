//! Installing `tailscale` from Tailscale's own apt repository (deb822, signed-by keyring),
//! and removing it again. Only on request: an existing install is adopted as it is.
//!
//! Every step can run again: the key is downloaded to a staging file and only moved into
//! place when its fingerprint matches [`KEY_FINGERPRINT`]; the source is rewritten whole; apt
//! skips what is installed. The steps are data ([`Step`]) so that rynasd runs them in a task
//! (logging every command, `--dry-system` only logs them) and tests read them.

use std::time::Duration;

use rynas_tasks::Cmd;

/// Tailscale's stable repositories (`<base>/<distro>`).
pub const REPO_BASE: &str = "https://pkgs.tailscale.com/stable";
/// The keyring path Tailscale's own instructions use: a source written by hand from those
/// instructions names the same `Signed-By`, so apt never sees two conflicting values.
pub const KEYRING: &str = "/usr/share/keyrings/tailscale-archive-keyring.gpg";
/// Where the download lands until its fingerprint is checked.
pub const KEY_STAGING: &str = "/usr/share/keyrings/.tailscale-archive-keyring.gpg.rynas-new";
/// Fingerprint of Tailscale's package signing key ("Tailscale Inc. (Package repository
/// signing key) <info@tailscale.com>"): a download with another primary key is refused.
pub const KEY_FINGERPRINT: &str = "2596A99EAAB33821893C0A79458CA832957F5868";
pub const SOURCES: &str = "/etc/apt/sources.list.d/tailscale.sources";
/// Forwarding for a subnet router or an exit node (written only while one is offered).
pub const SYSCTL: &str = "/etc/sysctl.d/60-rynas-tailscale.conf";
pub const PACKAGE: &str = "tailscale";
pub const SERVICE: &str = "tailscaled.service";

/// apt can take long on a slow mirror or a small CPU.
pub const APT_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const SHORT: Duration = Duration::from_secs(60);
/// How long apt waits for another holder of the dpkg lock.
const DPKG_LOCK_TIMEOUT: &str = "DPkg::Lock::Timeout=120";

/// The distribution a repository exists for, from `/etc/os-release`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Distro {
    /// `debian`, `ubuntu` or `raspbian`.
    pub id: String,
    /// `trixie`, `bookworm`, `noble`…
    pub codename: Option<String>,
}

/// The distribution from an `/etc/os-release` text: `None` when Tailscale has no Debian-style
/// repository for it.
pub fn distro(os_release: &str) -> Option<Distro> {
    let get = |k: &str| {
        os_release.lines().find_map(|l| {
            let v = l.strip_prefix(k)?.strip_prefix('=')?;
            Some(v.trim().trim_matches('"').to_string())
        })
    };
    let id = get("ID")?;
    let like = get("ID_LIKE").unwrap_or_default();
    let id = match id.as_str() {
        "debian" | "ubuntu" | "raspbian" => id,
        // derivatives keep their parent's codename in a separate field (best effort)
        _ if like.split_whitespace().any(|w| w == "debian") => "debian".into(),
        _ => return None,
    };
    let codename = get("VERSION_CODENAME")
        .filter(|c| !c.is_empty() && c.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()));
    Some(Distro { id, codename })
}

/// One step of the install.
#[derive(Debug, Clone)]
pub enum Step {
    Run(Cmd),
    /// Check the key downloaded to [`KEY_STAGING`] against [`KEY_FINGERPRINT`]
    /// ([`check_key`]), then install it as [`KEYRING`] (0644) and remove the staged file; a
    /// mismatch removes it and fails the install.
    CheckKey,
    /// Write a whole file (root-owned).
    Write {
        path: &'static str,
        content: String,
        mode: u32,
    },
}

pub fn repo_url(d: &Distro) -> String {
    format!("{REPO_BASE}/{}", d.id)
}

/// The binary (`noarmor`) keyring of the release.
pub fn key_url(d: &Distro, codename: &str) -> String {
    format!("{}/{codename}.noarmor.gpg", repo_url(d))
}

/// The deb822 source of Tailscale's repository.
pub fn sources_content(d: &Distro, codename: &str) -> String {
    format!(
        "# Managed by rynas: Tailscale's repository\nTypes: deb\nURIs: {}\nSuites: {codename}\nComponents: main\nSigned-By: {KEYRING}\n",
        repo_url(d)
    )
}

/// apt-get in a transient unit outside rynasd's sandbox (rynasd runs with
/// `ProtectSystem=yes`, dpkg must write `/usr`); `systemd-run --wait --pipe` streams the
/// output into the task log and returns apt's exit status.
fn apt(args: &[&str]) -> Cmd {
    let mut v = vec!["-o", DPKG_LOCK_TIMEOUT];
    v.extend_from_slice(args);
    outside("apt-get", &v)
}

/// `program args` in a transient unit outside rynasd's sandbox (see [`apt`]).
pub fn outside(program: &str, args: &[&str]) -> Cmd {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let unit = format!(
        "--unit=rynas-apt-tailscale-{secs}-{}",
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    Cmd::new("systemd-run")
        .args(["--wait", "--pipe", "--collect", "--quiet", "--service-type=exec"])
        .arg(unit)
        .arg(format!("--property=RuntimeMaxSec={}", APT_TIMEOUT.as_secs()))
        .args([
            &format!("--description=rynas: {program} (Tailscale)"),
            "--setenv=DEBIAN_FRONTEND=noninteractive",
            "--setenv=NEEDRESTART_MODE=l",
            "--setenv=APT_LISTCHANGES_FRONTEND=none",
            "--setenv=LC_ALL=C",
            "--",
            program,
        ])
        .args(args)
        .env_clear()
        .timeout(APT_TIMEOUT)
}

/// What to run after a step was cancelled or timed out: the transient unit is stopped, so a
/// retry does not hit the dpkg lock.
pub fn on_abort(cmd: &Cmd) -> Option<Cmd> {
    if cmd.program() != "systemd-run" {
        return None;
    }
    let unit = cmd
        .display()
        .split_whitespace()
        .find_map(|a| a.strip_prefix("--unit="))
        .filter(|u| u.starts_with("rynas-apt-tailscale-"))
        .map(|u| format!("{u}.service"))?;
    Some(
        Cmd::new("systemctl")
            .args(["stop", &unit])
            .env_clear()
            .timeout(Duration::from_secs(120)),
    )
}

/// Every step of the install. `key_ok`: the keyring holds Tailscale's key already (a retried
/// install): no download.
pub fn install_steps(d: &Distro, codename: &str, key_ok: bool) -> Vec<Step> {
    let mut v = vec![Step::Run(apt(&["install", "-y", "ca-certificates", "curl"]))];
    if !key_ok {
        v.push(Step::Run(
            Cmd::new("curl")
                .args(["-fsSL", "--proto", "=https", &key_url(d, codename), "-o", KEY_STAGING])
                .env_clear()
                .timeout(Duration::from_secs(120)),
        ));
        v.push(Step::CheckKey);
    }
    v.extend([
        Step::Write {
            path: SOURCES,
            content: sources_content(d, codename),
            mode: 0o644,
        },
        Step::Run(apt(&["update"])),
        Step::Run(apt(&[
            "install",
            "-y",
            "-o",
            "Dpkg::Options::=--force-confdef",
            "-o",
            "Dpkg::Options::=--force-confold",
            PACKAGE,
        ])),
        Step::Run(enable_cmd()),
    ]);
    v
}

/// The daemon enabled and started (the package does it; again in case a step failed).
pub fn enable_cmd() -> Cmd {
    Cmd::new("systemctl")
        .args(["enable", "--now", SERVICE])
        .env_clear()
        .timeout(SHORT)
}

/// The commands of the uninstall, in order. `purge_state`: the node identity in
/// [`crate::STATE_DIR`] goes too (and the device logs out first, so its key is no longer
/// valid); otherwise it stays, and a new install comes back as the same device.
pub fn uninstall_cmds(purge_state: bool) -> Vec<Cmd> {
    let mut v = Vec::new();
    if purge_state {
        v.push(crate::cli::logout());
    }
    v.push(
        Cmd::new("systemctl")
            .args(["disable", "--now", SERVICE])
            .env_clear()
            .timeout(SHORT),
    );
    v.push(apt(&[if purge_state { "purge" } else { "remove" }, "-y", PACKAGE]));
    if purge_state {
        v.push(
            Cmd::new("rm")
                .args(["-rf", "--one-file-system", crate::STATE_DIR])
                .env_clear()
                .timeout(SHORT),
        );
    }
    v
}

/// Files the uninstall removes (rynas's own: the source, the key, the forwarding sysctl).
pub const UNINSTALL_FILES: &[&str] = &[SOURCES, KEYRING, SYSCTL];

/// The forwarding sysctl of a subnet router or an exit node.
pub fn sysctl_content() -> &'static str {
    "# Managed by rynas: forwarding for the Tailscale subnet router or exit node\n\
     # (removed when neither is offered; the running value stays until the next boot)\n\
     net.ipv4.ip_forward = 1\n\
     net.ipv6.conf.all.forwarding = 1\n"
}

/// Loads [`SYSCTL`] now.
pub fn sysctl_load() -> Cmd {
    Cmd::new("sysctl")
        .args(["-p", SYSCTL])
        .env_clear()
        .timeout(SHORT)
}

/// The v4 fingerprint (40 upper-case hex digits) of the first public key of a binary
/// OpenPGP key (`.noarmor.gpg`): SHA-1 over `0x99`, the 2-byte length and the body of the
/// public-key packet (RFC 4880 §12.2).
pub fn key_fingerprint(bin: &[u8]) -> Result<String, String> {
    use sha1::Digest;
    let first = *bin.first().ok_or("empty key file")?;
    if first & 0x80 == 0 {
        return Err("not an OpenPGP key".into());
    }
    let (tag, len, hdr) = if first & 0x40 != 0 {
        let tag = first & 0x3f;
        let o1 = *bin.get(1).ok_or("truncated key")? as usize;
        match o1 {
            0..=191 => (tag, o1, 2),
            192..=223 => {
                let o2 = *bin.get(2).ok_or("truncated key")? as usize;
                (tag, ((o1 - 192) << 8) + o2 + 192, 3)
            }
            255 => {
                let b = bin.get(2..6).ok_or("truncated key")?;
                (tag, u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize, 6)
            }
            _ => return Err("partial packet lengths in a key".into()),
        }
    } else {
        let tag = (first >> 2) & 0x0f;
        match first & 3 {
            0 => (tag, *bin.get(1).ok_or("truncated key")? as usize, 2),
            1 => {
                let b = bin.get(1..3).ok_or("truncated key")?;
                (tag, u16::from_be_bytes([b[0], b[1]]) as usize, 3)
            }
            2 => {
                let b = bin.get(1..5).ok_or("truncated key")?;
                (tag, u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize, 5)
            }
            _ => return Err("indeterminate packet length".into()),
        }
    };
    if tag != 6 {
        return Err("the file does not start with a public key".into());
    }
    let body = bin.get(hdr..hdr + len).ok_or("truncated key")?;
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

/// `Ok` when the binary key's fingerprint is [`KEY_FINGERPRINT`].
pub fn check_key(bin: &[u8]) -> Result<(), String> {
    let fp = key_fingerprint(bin)?;
    if fp == KEY_FINGERPRINT {
        Ok(())
    } else {
        Err(format!(
            "the downloaded repository key has fingerprint {fp}, not Tailscale's {KEY_FINGERPRINT}: refused"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distro_from_os_release() {
        let trixie = "PRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\nID=debian\nVERSION_CODENAME=trixie\n";
        assert_eq!(
            distro(trixie),
            Some(Distro { id: "debian".into(), codename: Some("trixie".into()) })
        );
        let sid = "ID=debian\nVERSION_CODENAME=\n";
        assert_eq!(distro(sid).unwrap().codename, None);
        let noble = "ID=ubuntu\nID_LIKE=debian\nVERSION_CODENAME=noble\n";
        assert_eq!(distro(noble).unwrap().id, "ubuntu");
        assert_eq!(distro("ID=arch\n"), None);
        assert_eq!(distro("ID=fedora\nID_LIKE=\"rhel centos\"\n"), None);
    }

    #[test]
    fn source_is_deb822_with_the_keyring() {
        let d = distro("ID=debian\nVERSION_CODENAME=trixie\n").unwrap();
        let s = sources_content(&d, "trixie");
        assert!(s.contains("URIs: https://pkgs.tailscale.com/stable/debian\n"));
        assert!(s.contains("Suites: trixie\n"));
        assert!(s.contains(&format!("Signed-By: {KEYRING}\n")));
        assert_eq!(
            key_url(&d, "trixie"),
            "https://pkgs.tailscale.com/stable/debian/trixie.noarmor.gpg"
        );
    }

    #[test]
    fn steps_download_only_without_a_good_key() {
        let d = distro("ID=debian\nVERSION_CODENAME=trixie\n").unwrap();
        let fresh = install_steps(&d, "trixie", false);
        assert!(fresh.iter().any(|s| matches!(s, Step::CheckKey)));
        let again = install_steps(&d, "trixie", true);
        assert!(!again.iter().any(|s| matches!(s, Step::CheckKey)));
        let shown: Vec<String> = again
            .iter()
            .filter_map(|s| match s {
                Step::Run(c) => Some(c.display()),
                _ => None,
            })
            .collect();
        assert!(shown.iter().any(|c| c.contains("apt-get") && c.ends_with(" tailscale")));
        assert!(shown.iter().all(|c| !c.contains("curl -fsSL")));
        assert!(shown.last().unwrap().contains("enable --now tailscaled.service"));
    }

    #[test]
    fn uninstall_keeps_the_identity_unless_asked() {
        let keep: Vec<String> = uninstall_cmds(false).iter().map(Cmd::display).collect();
        assert!(keep.iter().all(|c| !c.contains("logout") && !c.contains("rm -rf")));
        assert!(keep.iter().any(|c| c.contains(" remove -y tailscale")));
        let purge: Vec<String> = uninstall_cmds(true).iter().map(Cmd::display).collect();
        assert!(purge[0].contains("logout"));
        assert!(purge.iter().any(|c| c.contains(" purge -y tailscale")));
        assert!(purge.last().unwrap().contains("rm -rf --one-file-system /var/lib/tailscale"));
        let stop = on_abort(&apt(&["update"])).unwrap();
        assert!(stop.display().starts_with("systemctl stop rynas-apt-tailscale-"));
    }

    /// A minimal v4 public-key packet (new and old header formats give one fingerprint).
    #[test]
    fn fingerprint_of_binary_keys() {
        let body: Vec<u8> = [4u8, 0x60, 0, 0, 0, 22].iter().copied().chain([7u8; 20]).collect();
        let mut old = vec![0x99, 0, body.len() as u8];
        old.extend(&body);
        let mut new = vec![0xc6, body.len() as u8];
        new.extend(&body);
        let a = key_fingerprint(&old).unwrap();
        assert_eq!(a.len(), 40);
        assert_eq!(a, key_fingerprint(&new).unwrap());
        assert!(check_key(&old).unwrap_err().contains("refused"));
        assert!(key_fingerprint(b"-----BEGIN PGP").is_err());
        assert!(key_fingerprint(&[0xc6, 50, 4]).is_err());
    }
}
