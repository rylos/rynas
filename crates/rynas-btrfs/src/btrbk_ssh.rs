//! Small, reusable pieces for btrbk over SSH: a dedicated key pair, the forced command of
//! `ssh_filter_btrbk.sh` and the `authorized_keys` line that pins a key to it, host keys.
//!
//! Used by snapshot replication ([`crate::replication`]: this NAS pushes to another host) and
//! meant for the other direction too (the NAS as a target of client machines): every function
//! is a pure builder or a blocking command, without rynas state.
//!
//! `ssh_filter_btrbk.sh` before btrbk 0.32.7 lets a restricted key run any command
//! (CVE-2026-62943, missing end anchor in its allow-list): the remote side needs 0.32.7 or a
//! fixed copy of the script.

use std::path::Path;
use std::time::Duration;

use rynas_tasks::Cmd;

/// Where Debian (and most distributions) install the filter.
pub const FILTER_SCRIPT: &str = "/usr/share/btrbk/scripts/ssh_filter_btrbk.sh";

/// The btrbk release that fixes CVE-2026-62943 in `ssh_filter_btrbk.sh`.
pub const FILTER_MIN_VERSION: &str = "0.32.7";

/// What a key may do on the host that runs the filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterRole {
    /// Receives backups and prunes them (`--target --delete`).
    Target,
    /// Is read from: snapshots and sends (`--source`), for pull setups.
    Source,
}

/// The filter's command line: role, `--restrict-path`, `--sudo` when the key's user runs
/// btrfs through sudo, `--log` (accepted and rejected commands in the system log).
pub fn filter_command(role: FilterRole, restrict_path: &str, sudo: bool) -> String {
    let role = match role {
        FilterRole::Target => "--target --delete",
        FilterRole::Source => "--source",
    };
    let sudo = if sudo { " --sudo" } else { "" };
    format!("{FILTER_SCRIPT} {role} --restrict-path {restrict_path}{sudo} --log")
}

/// An `authorized_keys` line: the key forced into `command` with `restrict` (no pty, no
/// forwarding, no agent, no X11, no user rc). `public_key` is `type base64 [comment]`.
pub fn authorized_keys_line(public_key: &str, command: &str) -> String {
    let key = public_key.trim();
    let command = command.replace('\\', "\\\\").replace('"', "\\\"");
    format!("command=\"{command}\",restrict {key}")
}

/// `ssh-keygen` creating an ed25519 key pair without passphrase at `private`
/// (`private.pub` beside it).
pub fn keygen_cmd(private: &Path, comment: &str) -> Cmd {
    Cmd::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", comment, "-f"])
        .arg(private)
        .env_clear()
        .timeout(Duration::from_secs(30))
}

/// Creates the key pair when missing (directory 0700, private key 0600, both root's as
/// rynasd runs as root) and returns the public key line.
pub fn ensure_keypair(private: &Path, comment: &str) -> Result<String, String> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if let Some(dir) = private.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    if !private.exists() {
        keygen_cmd(private, comment)
            .run_blocking()
            .map_err(|e| format!("ssh-keygen: {e}"))?;
    }
    std::fs::set_permissions(private, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("{}: {e}", private.display()))?;
    public_key(private).ok_or_else(|| format!("{}.pub is missing", private.display()))
}

/// The public key line next to `private`, if any.
pub fn public_key(private: &Path) -> Option<String> {
    let mut p = private.as_os_str().to_owned();
    p.push(".pub");
    std::fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `ssh-keygen -lf -` reading key lines (public keys or `ssh-keyscan` output) on stdin; one
/// output line per key: `bits SHA256:… comment (TYPE)`.
pub fn fingerprint_cmd(lines: &str) -> Cmd {
    Cmd::new("ssh-keygen")
        .args(["-l", "-f", "-"])
        .stdin(lines.as_bytes().to_vec())
        .env_clear()
        .timeout(Duration::from_secs(10))
}

/// The `SHA256:…` fields of `ssh-keygen -l` output, in order.
pub fn parse_fingerprints(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .filter(|f| f.starts_with("SHA256:"))
        .map(str::to_string)
        .collect()
}

/// `ssh-keyscan` of one host (ed25519, ecdsa and rsa keys, 10 s).
pub fn keyscan_cmd(host: &str, port: u16) -> Cmd {
    Cmd::new("ssh-keyscan")
        .args(["-T", "10", "-t", "ed25519,ecdsa,rsa", "-p"])
        .arg(port.to_string())
        .arg("--")
        .arg(host)
        .env_clear()
        .timeout(Duration::from_secs(20))
}

/// The host keys of `ssh-keyscan` output (comment lines dropped).
pub fn parse_keyscan(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter(|l| l.split_whitespace().count() == 3)
        .map(str::to_string)
        .collect()
}

/// The name ssh looks a host up with in known_hosts: `host`, or `[host]:port` when the port
/// is not 22.
pub fn known_hosts_name(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

/// A host key line from `ssh-keyscan` (`<name> <type> <base64>`) written for `host:port`;
/// `None` when it is not a key line.
pub fn known_hosts_line(scanned: &str, host: &str, port: u16) -> Option<String> {
    let mut it = scanned.split_whitespace();
    let (_, kind, key) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() || !valid_key_type(kind) || !valid_base64(key) {
        return None;
    }
    Some(format!("{} {kind} {key}", known_hosts_name(host, port)))
}

fn valid_key_type(k: &str) -> bool {
    matches!(
        k,
        "ssh-ed25519"
            | "ssh-rsa"
            | "ecdsa-sha2-nistp256"
            | "ecdsa-sha2-nistp384"
            | "ecdsa-sha2-nistp521"
            | "sk-ssh-ed25519@openssh.com"
            | "sk-ecdsa-sha2-nistp256@openssh.com"
    )
}

fn valid_base64(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 4096
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_and_authorized_keys() {
        let c = filter_command(FilterRole::Target, "/vol1/replica/docs", true);
        assert_eq!(
            c,
            "/usr/share/btrbk/scripts/ssh_filter_btrbk.sh --target --delete --restrict-path /vol1/replica/docs --sudo --log"
        );
        let l = authorized_keys_line("ssh-ed25519 AAAAC3Nz rynas-replication@nas\n", &c);
        assert_eq!(
            l,
            format!("command=\"{c}\",restrict ssh-ed25519 AAAAC3Nz rynas-replication@nas")
        );
        assert!(filter_command(FilterRole::Source, "/home", false).contains("--source --restrict-path /home --log"));
    }

    #[test]
    fn host_keys() {
        let out = "# backup.lan:2222 SSH-2.0-OpenSSH_9.9\n[backup.lan]:2222 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA\nbad line\n";
        let keys = parse_keyscan(out);
        assert_eq!(keys, ["[backup.lan]:2222 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA"]);
        assert_eq!(
            known_hosts_line(&keys[0], "backup.lan", 2222).unwrap(),
            "[backup.lan]:2222 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA"
        );
        assert_eq!(
            known_hosts_line("x ssh-ed25519 AAAA", "h", 22).unwrap(),
            "h ssh-ed25519 AAAA"
        );
        assert!(known_hosts_line("x evil AAAA", "h", 22).is_none());
        assert!(known_hosts_line("x ssh-rsa AA\"AA", "h", 22).is_none());
        assert_eq!(
            parse_fingerprints("256 SHA256:abc/def no comment (ED25519)\n"),
            ["SHA256:abc/def"]
        );
    }
}
