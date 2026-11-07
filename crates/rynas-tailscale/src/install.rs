//! Installing `tailscale` from Tailscale's own apt repository (deb822, signed-by keyring),
//! and removing it again. Only on request: an existing install is adopted as it is.
//!
//! Every step can run again: the key is downloaded to a staging file in rynasd's state
//! directory and only placed when it holds Tailscale's key and nothing else
//! ([`check_key`]); the source is rewritten whole; apt skips what is installed.
//!
//! rynasd runs with `ProtectSystem=yes`: `/usr` (where Tailscale's instructions put the
//! keyring) is read-only inside it. The keyring is placed and removed by `install`/`rm` in a
//! transient unit outside the sandbox ([`outside`]), like apt itself; the files rynasd writes
//! itself are in `/etc` ([`writable_in_sandbox`] says which paths it can write). The steps are data ([`Step`]) so that rynasd runs them in a task
//! (logging every command, `--dry-system` only logs them) and tests read them.

use std::time::Duration;

use rynas_tasks::Cmd;

/// Tailscale's stable repositories (`<base>/<distro>`).
pub const REPO_BASE: &str = "https://pkgs.tailscale.com/stable";
/// The keyring path Tailscale's own instructions use: a source written by hand from those
/// instructions names the same `Signed-By`, so apt never sees two conflicting values.
pub const KEYRING: &str = "/usr/share/keyrings/tailscale-archive-keyring.gpg";
/// Name of the staged download in rynasd's state directory (`<state>/tailscale/`).
pub const KEY_STAGING_NAME: &str = "archive-keyring.gpg.new";
/// Paths `ProtectSystem=yes` (rynasd.service) keeps read-only inside rynasd: what lands
/// there goes through [`outside`]. (`/boot` is in the unit's `ReadWritePaths`.)
pub const READ_ONLY_IN_SANDBOX: &[&str] = &["/usr/", "/efi/"];

/// rynasd can write `path` itself (not below [`READ_ONLY_IN_SANDBOX`]).
pub fn writable_in_sandbox(path: &str) -> bool {
    !READ_ONLY_IN_SANDBOX.iter().any(|ro| path.starts_with(ro))
}
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
    /// Check the key downloaded to `staged` ([`check_key`]), then place it as [`KEYRING`]
    /// with [`place_key`] (outside the sandbox) and remove the staged file; a refused key is
    /// removed and fails the install.
    CheckKey { staged: String },
    /// Write a whole file (root-owned; always [`writable_in_sandbox`]).
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
/// install): no download. `staged`: where the download lands (in rynasd's state directory,
/// writable inside its sandbox).
pub fn install_steps(d: &Distro, codename: &str, key_ok: bool, staged: &str) -> Vec<Step> {
    let mut v = vec![Step::Run(apt(&["install", "-y", "ca-certificates", "curl"]))];
    if !key_ok {
        v.push(Step::Run(
            Cmd::new("curl")
                .args(["-fsSL", "--proto", "=https", &key_url(d, codename), "-o", staged])
                .env_clear()
                .timeout(Duration::from_secs(120)),
        ));
        v.push(Step::CheckKey { staged: staged.to_string() });
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

/// The checked key placed as [`KEYRING`] (0644), outside the sandbox (`/usr` is read-only
/// in rynasd).
pub fn place_key(staged: &str) -> Cmd {
    outside("install", &["-D", "-m", "0644", staged, KEYRING])
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
    // the keyring is below /usr: removed outside the sandbox
    v.push(outside("rm", &["-f", KEYRING]));
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

/// Files rynasd removes after the uninstall (rynas's own: the source, the forwarding sysctl;
/// the keyring goes with a command of [`uninstall_cmds`]).
pub const UNINSTALL_FILES: &[&str] = &[SOURCES, SYSCTL];

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

/// The OpenPGP packets of a binary key file (`.noarmor.gpg`) as `(tag, body)`, every one
/// of them (RFC 4880 §4.2: old and new headers; partial lengths are refused, keys never use
/// them).
pub fn packets(bin: &[u8]) -> Result<Vec<(u8, &[u8])>, String> {
    let mut out = Vec::new();
    let mut at = 0usize;
    let byte = |i: usize| bin.get(i).copied().map(usize::from).ok_or("truncated key");
    while at < bin.len() {
        let first = bin[at];
        if first & 0x80 == 0 {
            return Err("not an OpenPGP key".into());
        }
        let (tag, len, hdr) = if first & 0x40 != 0 {
            let tag = first & 0x3f;
            let o1 = byte(at + 1)?;
            match o1 {
                0..=191 => (tag, o1, 2),
                192..=223 => (tag, ((o1 - 192) << 8) + byte(at + 2)? + 192, 3),
                255 => {
                    let b = bin.get(at + 2..at + 6).ok_or("truncated key")?;
                    (tag, u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize, 6)
                }
                _ => return Err("partial packet lengths in a key".into()),
            }
        } else {
            let tag = (first >> 2) & 0x0f;
            match first & 3 {
                0 => (tag, byte(at + 1)?, 2),
                1 => {
                    let b = bin.get(at + 1..at + 3).ok_or("truncated key")?;
                    (tag, u16::from_be_bytes([b[0], b[1]]) as usize, 3)
                }
                2 => {
                    let b = bin.get(at + 1..at + 5).ok_or("truncated key")?;
                    (tag, u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize, 5)
                }
                _ => return Err("indeterminate packet length".into()),
            }
        };
        let start = at + hdr;
        let end = start.checked_add(len).ok_or("truncated key")?;
        let body = bin.get(start..end).ok_or("truncated key")?;
        out.push((tag, body));
        at = end;
    }
    if out.is_empty() {
        return Err("empty key file".into());
    }
    Ok(out)
}

/// Packet tags a single exported public key may hold besides its one public-key packet
/// (6): signatures (2), user IDs (13), public subkeys (14), user attributes (17).
const KEY_EXTRA_TAGS: &[u8] = &[2, 13, 14, 17];

/// The v4 fingerprint (40 upper-case hex digits) of the only public key of a binary OpenPGP
/// key file: SHA-1 over `0x99`, the 2-byte length and the body of the public-key packet
/// (RFC 4880 §12.2). Every packet is read: a file that does not start with a public key,
/// holds a second one, or holds any packet a single key does not have is refused (apt would
/// trust every key of a keyring named by `Signed-By`).
pub fn key_fingerprint(bin: &[u8]) -> Result<String, String> {
    use sha1::Digest;
    let ps = packets(bin)?;
    if ps[0].0 != 6 {
        return Err("the file does not start with a public key".into());
    }
    let keys = ps.iter().filter(|(t, _)| *t == 6).count();
    if keys != 1 {
        return Err(format!("the file holds {keys} public keys, not one"));
    }
    if let Some((t, _)) = ps.iter().skip(1).find(|(t, _)| !KEY_EXTRA_TAGS.contains(t)) {
        return Err(format!(
            "the file holds an OpenPGP packet of type {t}, not part of a public key"
        ));
    }
    let body = ps[0].1;
    if body.first() != Some(&4) {
        return Err("not a version 4 key".into());
    }
    let len16 = u16::try_from(body.len()).map_err(|_| "key packet too long")?;
    let mut h = sha1::Sha1::new();
    h.update([0x99]);
    h.update(len16.to_be_bytes());
    h.update(body);
    Ok(h.finalize().iter().map(|b| format!("{b:02X}")).collect())
}

/// `Ok` when the binary key file holds exactly Tailscale's key ([`KEY_FINGERPRINT`]).
pub fn check_key(bin: &[u8]) -> Result<(), String> {
    let fp = key_fingerprint(bin)
        .map_err(|e| format!("the downloaded repository key is refused: {e}"))?;
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

    const STAGED: &str = "/var/lib/rynas/tailscale/archive-keyring.gpg.new";

    /// rynasd.service has `ProtectSystem=yes`: nothing rynasd writes itself may be below
    /// /usr (the first install on a real system failed on the keyring there).
    #[test]
    fn sandbox_writes_stay_writable() {
        let unit = include_str!("../../../packaging/systemd/rynasd.service");
        assert!(
            unit.lines().any(|l| l.trim() == "ProtectSystem=yes"),
            "rynasd.service changed ProtectSystem: update READ_ONLY_IN_SANDBOX"
        );
        assert!(!writable_in_sandbox(KEYRING));
        let d = distro("ID=debian\nVERSION_CODENAME=trixie\n").unwrap();
        for s in install_steps(&d, "trixie", false, STAGED) {
            match s {
                Step::Write { path, .. } => assert!(writable_in_sandbox(path), "{path}"),
                Step::CheckKey { staged } => assert!(writable_in_sandbox(&staged)),
                Step::Run(c) if c.program() != "systemd-run" => {
                    let shown = c.display();
                    for ro in READ_ONLY_IN_SANDBOX {
                        assert!(!shown.contains(&format!(" {ro}")), "{shown}");
                    }
                }
                Step::Run(_) => {}
            }
        }
        for f in UNINSTALL_FILES.iter().chain([&SYSCTL]) {
            assert!(writable_in_sandbox(f), "{f}");
        }
        let place = place_key(STAGED).display();
        assert!(
            place.starts_with("systemd-run ")
                && place.ends_with(&format!("install -D -m 0644 {STAGED} {KEYRING}")),
            "{place}"
        );
        let rm: Vec<String> = uninstall_cmds(false).iter().map(Cmd::display).collect();
        assert!(rm.iter().any(|c| c.starts_with("systemd-run ") && c.ends_with(&format!("rm -f {KEYRING}"))));
    }

    #[test]
    fn steps_download_only_without_a_good_key() {
        let d = distro("ID=debian\nVERSION_CODENAME=trixie\n").unwrap();
        let fresh = install_steps(&d, "trixie", false, STAGED);
        assert!(fresh.iter().any(|s| matches!(s, Step::CheckKey { staged } if staged == STAGED)));
        let again = install_steps(&d, "trixie", true, STAGED);
        assert!(!again.iter().any(|s| matches!(s, Step::CheckKey { .. })));
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

    /// Tailscale's real key passes; the same key with a second key appended, or with a
    /// packet a key does not have, is refused.
    #[test]
    fn only_tailscales_key_alone() {
        let real = include_bytes!("../testdata/tailscale-archive-keyring.gpg");
        assert_eq!(key_fingerprint(real).unwrap(), KEY_FINGERPRINT);
        assert!(check_key(real).is_ok());
        let tags: Vec<u8> = packets(real).unwrap().iter().map(|p| p.0).collect();
        assert_eq!(tags[0], 6);
        assert!(tags.contains(&14));
        let body: Vec<u8> = [4u8, 0x60, 0, 0, 0, 22].iter().copied().chain([7u8; 20]).collect();
        let mut two = real.to_vec();
        two.extend([0xc6, body.len() as u8]);
        two.extend(&body);
        let e = check_key(&two).unwrap_err();
        assert!(e.contains("2 public keys"), "{e}");
        let mut odd = real.to_vec();
        odd.extend([0xcb, 3, 1, 2, 3]); // literal data (11)
        assert!(check_key(&odd).unwrap_err().contains("type 11"));
        let mut cut = real.to_vec();
        cut.truncate(real.len() - 1);
        assert!(check_key(&cut).is_err());
    }
}
