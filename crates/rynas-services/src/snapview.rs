//! The `#snapshot` folder of a share ([`rynas_api_types::Share::show_snapshots`]).
//!
//! One read-only bind mount per share: the share's snapshot directory (the btrbk
//! `snapshot_dir` of its job, `<volume>/.snapshots/<job>`, which holds only that job's
//! snapshots) appears at `<share>/#snapshot`. Why this layout (see
//! `docs/research/snapshots-replication.md`, "Snapshots inside the share"):
//! - it follows the snapshots by itself: btrbk creates and prunes snapshots in the directory
//!   and the folder shows them at once, with no per-snapshot mount to keep in step. (A mount
//!   per snapshot would pin the snapshot subvolume: the kernel refuses to delete a mounted
//!   subvolume, so retention would fail.)
//! - read-only at the VFS level: clients cannot create, rename or delete anything in it,
//!   and the mount point itself cannot be removed while mounted;
//! - snapshots of the share do not contain it (a btrfs snapshot never crosses a mount), so
//!   there are no snapshots of snapshots; btrfs space accounting is unchanged, and walks
//!   that stay on one filesystem (sizes, `rm --one-file-system`) skip it;
//! - it survives a reboot: the mount units live in `/etc/systemd/system` and the target
//!   `rynas-snapshot-folders.target` (wanted by `multi-user.target`) pulls them in. rynasd
//!   never mounts anything itself: its own mount namespace is private.
//!
//! Units are managed files of the `shares` domain (header + marker line), so drift
//! detection and rollback cover them.

use rynas_api_types::{SNAPSHOT_FOLDER, Share};

pub const UNIT_DIR: &str = "/etc/systemd/system";
/// Pulls in every snapshot folder mount at boot.
pub const TARGET: &str = "rynas-snapshot-folders.target";
/// Marker line of the mount units this module writes.
pub const MARKER: &str = "# rynas: snapshot folder of a share";

/// `systemd-escape --path`.
pub fn escape_path(path: &str) -> String {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return "-".into();
    }
    let mut out = String::new();
    for (i, b) in trimmed.bytes().enumerate() {
        match b {
            b'/' => out.push('-'),
            b'.' if i == 0 => out.push_str("\\x2e"),
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b':' | b'_' | b'.' => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out
}

/// Where the folder of a share appears.
pub fn mount_point(share_path: &str) -> String {
    format!("{}/{SNAPSHOT_FOLDER}", share_path.trim_end_matches('/'))
}

/// Mount unit name of the snapshot folder of a share at `share_path`.
pub fn unit_name(share_path: &str) -> String {
    format!("{}.mount", escape_path(&mount_point(share_path)))
}

pub fn unit_path(share_path: &str) -> String {
    format!("{UNIT_DIR}/{}", unit_name(share_path))
}

/// The snapshot folder a share asks for: (share path, snapshot directory).
pub fn wanted(s: &Share) -> Option<(String, String)> {
    let dir = s.smb.shadow_copy.snapshot_dir.trim_end_matches('/');
    (s.show_snapshots && !dir.is_empty() && !s.path.is_empty())
        .then(|| (s.path.clone(), dir.to_string()))
}

/// The mount unit (without the rynas header).
pub fn unit(share: &str, share_path: &str, snapshot_dir: &str) -> String {
    format!(
        "{MARKER}\n\
         # Snapshots of share {share}, read-only (`shares` configuration key).\n\
         [Unit]\n\
         Description=Snapshots of share {share} (read-only)\n\
         PartOf={TARGET}\n\
         ConditionPathIsDirectory={snapshot_dir}\n\
         \n\
         [Mount]\n\
         What={snapshot_dir}\n\
         Where={where_}\n\
         Type=none\n\
         Options=bind,ro,nosuid,nodev,noexec,x-gvfs-hide\n\
         DirectoryMode=0755\n",
        where_ = mount_point(share_path),
    )
}

/// The target that pulls the mounts in at boot.
pub fn target(units: &[String]) -> String {
    let mut s = String::from(
        "# Snapshot folders of the shares (`shares` configuration key).\n\
         [Unit]\n\
         Description=Snapshot folders of the shares\n",
    );
    for u in units {
        s.push_str(&format!("Wants={u}\nAfter={u}\n"));
    }
    s.push_str("\n[Install]\nWantedBy=multi-user.target\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_names_follow_systemd_escape() {
        assert_eq!(
            unit_name("/vol1/photos"),
            "vol1-photos-\\x23snapshot.mount"
        );
        assert_eq!(
            unit_name("/vol1/my-docs.2026/"),
            "vol1-my\\x2ddocs.2026-\\x23snapshot.mount"
        );
        assert_eq!(escape_path("/.x/a b"), "\\x2ex-a\\x20b");
    }

    #[test]
    fn the_unit_is_a_read_only_bind() {
        let u = unit("photos", "/vol1/photos", "/vol1/.snapshots/photos");
        assert!(u.starts_with(MARKER));
        assert!(u.contains("What=/vol1/.snapshots/photos\n"));
        assert!(u.contains("Where=/vol1/photos/#snapshot\n"));
        assert!(u.contains("Options=bind,ro,nosuid,nodev,noexec"));
        let t = target(&["a.mount".into()]);
        assert!(t.contains("Wants=a.mount") && t.contains("WantedBy=multi-user.target"));
    }
}
