//! Name and value rules shared by the server (rynasd and the domain crates), rynas-web and
//! the web UI, so the client checks exactly what the server will refuse (review wave A
//! m15). No dependencies: it compiles on `wasm32-unknown-unknown`.
//!
//! Each function only says whether a value is acceptable; the server adds the field and the
//! message to its error, the UI shows the translated rule under the field.

/// Kernel disk name ("sda", "nvme0n1"): it reaches sysfs paths, URLs and argv.
pub fn disk_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 32
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Host name: an RFC 1123 label, lowercase, 1–63 of `a-z 0-9 -`, not starting or ending
/// with `-`.
pub fn hostname(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 63
        && h.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !h.starts_with('-')
        && !h.ends_with('-')
}

/// Linux user or group name: `[a-z_][a-z0-9_-]{0,31}`.
pub fn account_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && (b[0].is_ascii_lowercase() || b[0] == b'_')
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

/// Names that rynas never manages as local users or groups.
pub const RESERVED_ACCOUNT_NAMES: &[&str] = &[
    "root",
    "daemon",
    "bin",
    "sys",
    "sync",
    "nobody",
    "nogroup",
    "users",
    "rynas",
    "rynas-admin",
    "rynas-ssh",
    "rynas-sftp",
    "sudo",
    "adm",
    "staff",
    "admin",
    "guest",
];

/// SMB share name: `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`, not a special section of smb.conf.
pub fn share_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
        && !["global", "homes", "printers", "print$", "ipc$"]
            .contains(&s.to_ascii_lowercase().as_str())
}

/// SMB workgroup as typed: 1–15 letters, digits, `-` or `_` once trimmed (the server stores
/// it upper-cased).
pub fn workgroup(w: &str) -> bool {
    let w = w.trim();
    !w.is_empty()
        && w.len() <= 15
        && w.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Mount names a volume label may not take.
pub const RESERVED_LABELS: &[&str] = &[
    "root", "boot", "home", "srv", "var", "run", "tmp", "usr", "etc", "proc", "sys", "dev", "mnt",
    "opt", "media", "bin", "sbin", "lib", "lib32", "lib64", "libx32", "snap", "nix", "efi",
    // the system volume (D-ROOTVOL)
    "system",
];

/// Volume label, also its mount name `/<label>`: `[a-z][a-z0-9-]{0,15}`, not a system
/// directory.
pub fn volume_label(l: &str) -> bool {
    let b = l.as_bytes();
    !b.is_empty()
        && b.len() <= 16
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && !RESERVED_LABELS.contains(&l)
}

/// A volume something can be put on: a rynas volume label or the system volume
/// ([`crate::storage::SYSTEM_VOLUME`], D-ROOTVOL).
pub fn place_label(l: &str) -> bool {
    l == crate::storage::SYSTEM_VOLUME || volume_label(l)
}

/// Subvolume name: `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`, so it also fits a path segment, a
/// systemd unit and an SMB share.
pub fn subvolume_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(disk_name("nvme0n1") && !disk_name("sda/../x") && !disk_name(""));
        assert!(hostname("nas-1") && !hostname("-nas") && !hostname("Nas"));
        assert!(account_name("_svc") && account_name("alice-2") && !account_name("Alice"));
        assert!(share_name("Media.2026") && !share_name("homes") && !share_name(".x"));
        assert!(workgroup(" workgroup ") && !workgroup("WORK GROUP") && !workgroup(""));
        assert!(!workgroup("ABCDEFGHIJKLMNOP"));
        assert!(volume_label("media-01") && !volume_label("home") && !volume_label("2vol"));
        // the system volume (D-ROOTVOL): a place, never a rynas volume label
        assert!(place_label("system") && !volume_label("system") && place_label("vol1"));
        use crate::storage::{low_space, split_volume_path, volume_dir};
        assert_eq!(volume_dir("system"), "/srv/rynas/system");
        assert_eq!(volume_dir("vol1"), "/vol1");
        assert_eq!(split_volume_path("/vol1/a/b"), Some(("vol1".into(), "a/b")));
        assert_eq!(split_volume_path("/srv/rynas/system/a"), Some(("system".into(), "a")));
        assert_eq!(split_volume_path("/srv/rynas/system"), Some(("system".into(), "")));
        assert_eq!(split_volume_path("/srv/rynas/systemx/a"), None);
        assert_eq!(split_volume_path("/lib/x"), None);
        const G: u64 = 1 << 30;
        assert!(low_space(100 * G, 5 * G) && !low_space(100 * G, 50 * G));
        assert!(low_space(1000 * G, 20 * G) && !low_space(0, 0));
        // a small system disk is always short of room for data
        assert!(low_space(8 * G, 7 * G) && !low_space(64 * G, 12 * G));
        assert!(subvolume_name("Video_2026.raw") && !subvolume_name(".snapshots"));
        assert!(!subvolume_name("a/b"));
    }
}
