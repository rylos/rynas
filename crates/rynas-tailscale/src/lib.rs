//! The Tailscale side of rynas (remote access, phase 2; `docs/phase2/remote-access.md`).
//!
//! Built into rynasd behind this crate boundary so that it can later move into a plugin
//! process (D-PLUG). Generic Debian/Ubuntu only: nothing here knows any hardware module.
//!
//! - [`install`]: Tailscale's deb822 repository with its key checked by fingerprint, the
//!   package, the forwarding sysctl of a subnet router or exit node, the uninstall;
//! - [`localapi`]: tailscaled's LocalAPI over its Unix socket (status, preferences), read
//!   only: changes go through the `tailscale` CLI, whose flags are the stable interface;
//! - [`cli`]: the `tailscale up / set / logout` command lines.

pub mod cli;
pub mod install;
pub mod localapi;

/// tailscaled's LocalAPI socket on Linux.
pub const SOCKET: &str = "/run/tailscale/tailscaled.sock";
/// The CLI the package installs.
pub const CLI: &str = "/usr/bin/tailscale";
/// tailscaled's own state (node key, preferences).
pub const STATE_DIR: &str = "/var/lib/tailscale";
/// Tailnet ranges: the CGNAT block Tailscale assigns from and its ULA prefix.
pub const TAILNET_V4: ([u8; 4], u8) = ([100, 64, 0, 0], 10);
pub const TAILNET_V6: ([u16; 3], u8) = ([0xfd7a, 0x115c, 0xa1e0], 48);

/// Is `ip` a tailnet address (100.64.0.0/10 or fd7a:115c:a1e0::/48)?
pub fn is_tailnet(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (o[1] & 0xc0) == 64
        }
        std::net::IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_tailnet(std::net::IpAddr::V4(v4));
            }
            let s = v6.segments();
            s[0] == TAILNET_V6.0[0] && s[1] == TAILNET_V6.0[1] && s[2] == TAILNET_V6.0[2]
        }
    }
}

/// Real location of the absolute path `abs` below `root` (`/` in production, a dry root or
/// a temporary folder elsewhere).
pub fn under(root: &std::path::Path, abs: &str) -> std::path::PathBuf {
    root.join(abs.trim_start_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailnet_ranges() {
        for a in ["100.64.0.1", "100.127.255.254", "fd7a:115c:a1e0::5", "::ffff:100.100.1.1"] {
            assert!(is_tailnet(a.parse().unwrap()), "{a}");
        }
        for a in ["100.63.255.255", "100.128.0.1", "192.168.1.2", "fd7a:115c:a1e1::1", "::1"] {
            assert!(!is_tailnet(a.parse().unwrap()), "{a}");
        }
    }
}
